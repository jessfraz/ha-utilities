//! One transaction changes ownership, never collector namespaces or evidence.
use crate::{
    config::AccountBinding,
    imports::{
        ImportAccountConsolidation, ImportAccountConsolidationReceipt, ImportError, ImportRequest,
        digest,
    },
    models::{Coverage, CoverageState, Observation},
    store::{ObservationRow, Store, connections::binding_hash, queries},
};
use serde::{Deserialize, Serialize};
use sqlx::{Row, Sqlite, Transaction};

#[derive(Deserialize, Serialize)]
struct SavedConsolidation {
    request_digest: String,
    receipt: ImportAccountConsolidationReceipt,
}

async fn checked_account(
    tx: &mut Transaction<'_, Sqlite>,
    deployment: &str,
    input: &crate::imports::ImportAccount,
    currency: &str,
    expected_records: u64,
) -> Result<AccountBinding, ImportError> {
    let row = sqlx::query("SELECT binding,import_start_on,import_only=1 AND connection_id IS NULL AND NOT EXISTS(SELECT 1 FROM sync_jobs WHERE account_id=accounts.id) AS eligible,(SELECT count(*) FROM imported_records WHERE account_id=accounts.id) AS records FROM accounts WHERE id=? AND deployment=? AND binding IS NOT NULL")
        .bind(&input.id).bind(deployment).fetch_optional(&mut **tx).await?.ok_or(ImportError::Binding)?;
    let binding: AccountBinding = serde_json::from_str(row.try_get("binding")?)?;
    let proposed = ImportRequest {
        account: input.clone(),
        currency: currency.to_owned(),
        records: Vec::new(),
    }
    .binding(deployment);
    if !row.try_get::<bool, _>("eligible")?
        || u64::try_from(row.try_get::<i64, _>("records")?).ok() != Some(expected_records)
        || row
            .try_get::<Option<String>, _>("import_start_on")?
            .as_deref()
            != Some(input.start_on.to_string().as_str())
        || binding_hash(&binding)? != binding_hash(&proposed)?
    {
        return Err(ImportError::Binding);
    }
    Ok(binding)
}

impl Store {
    pub async fn consolidate_import_accounts(
        &self,
        deployment: &str,
        input: ImportAccountConsolidation,
    ) -> Result<ImportAccountConsolidationReceipt, ImportError> {
        input.validate()?;
        let request_digest = digest(&serde_json::to_vec(&(deployment, &input))?);
        let mut tx = self.pool.begin_with(queries::BEGIN_INGEST).await?;
        let saved: Option<String> = sqlx::query_scalar("SELECT o.consolidation FROM import_origins o JOIN accounts a ON a.id=o.account_id WHERE o.id=? AND a.deployment=?")
            .bind(&input.source.id).bind(deployment).fetch_optional(&mut *tx).await?.flatten();
        if let Some(saved) = saved {
            let mut saved: SavedConsolidation = serde_json::from_str(&saved)?;
            if saved.request_digest != request_digest {
                return Err(ImportError::Binding);
            }
            saved.receipt.replayed = true;
            tx.commit().await?;
            return Ok(saved.receipt);
        }
        let source = checked_account(
            &mut tx,
            deployment,
            &input.source,
            &input.currency,
            input.expected_source_records,
        )
        .await?;
        let mut target = checked_account(
            &mut tx,
            deployment,
            &input.target,
            &input.currency,
            input.expected_target_records,
        )
        .await?;
        // Distinct collector service IDs remain provenance. A shared ID with a
        // different type or provider reference must never silently change meaning.
        for service in source.services {
            if let Some(existing) = target.services.iter().find(|s| s.id == service.id) {
                if existing.kind != service.kind
                    || existing.provider_service != service.provider_service
                {
                    return Err(ImportError::Binding);
                }
            } else {
                target.services.push(service);
            }
        }
        target.services.sort_by(|a, b| a.id.cmp(&b.id));
        if target.services.len() > 64 {
            return Err(ImportError::Binding);
        }
        let conflict: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM imported_records s JOIN imported_records t ON t.account_id=? AND t.kind=s.kind AND t.id=s.id WHERE s.account_id=?) OR EXISTS(SELECT 1 FROM observations s JOIN observations t ON t.account_id=? AND t.kind=s.kind AND t.id=s.id WHERE s.account_id=?)")
            .bind(&target.id).bind(&source.id).bind(&target.id).bind(&source.id).fetch_one(&mut *tx).await?;
        if conflict {
            return Err(ImportError::Conflict);
        }

        // Parent keys must exist before changing their immediately checked FK children.
        sqlx::query("INSERT INTO imported_records(account_id,kind,id,bill_id,revision,payload) SELECT ?,kind,id,bill_id,revision,payload FROM imported_records WHERE account_id=?")
            .bind(&target.id).bind(&source.id).execute(&mut *tx).await?;
        for sql in [
            "UPDATE imported_revisions SET account_id=? WHERE account_id=?",
            "UPDATE imported_evidence SET account_id=? WHERE account_id=?",
        ] {
            sqlx::query(sql)
                .bind(&target.id)
                .bind(&source.id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM imported_records WHERE account_id=?")
            .bind(&source.id)
            .execute(&mut *tx)
            .await?;
        let observations = sqlx::query(
            "SELECT kind,payload,observed_at FROM observations WHERE account_id=? ORDER BY kind,id",
        )
        .bind(&source.id)
        .fetch_all(&mut *tx)
        .await?;
        for row in observations {
            // Observation payloads are the variant structs, without an enum tag.
            let kind: &str = row.try_get("kind")?;
            let payload: &str = row.try_get("payload")?;
            let observation = match kind {
                "bill" => {
                    let mut value: crate::models::Bill = serde_json::from_str(payload)?;
                    value.account_id.clone_from(&target.id);
                    Observation::Bill(value)
                }
                "balance" => {
                    let mut value: crate::models::Balance = serde_json::from_str(payload)?;
                    value.account_id.clone_from(&target.id);
                    Observation::Balance(value)
                }
                "payment" => {
                    let mut value: crate::models::Payment = serde_json::from_str(payload)?;
                    value.account_id.clone_from(&target.id);
                    Observation::Payment(value)
                }
                "usage" => {
                    let mut value: crate::models::Usage = serde_json::from_str(payload)?;
                    value.account_id.clone_from(&target.id);
                    Observation::Usage(value)
                }
                _ => return Err(ImportError::Binding),
            };
            let projected = ObservationRow::new(&target, &observation)?;
            sqlx::query(queries::UPSERT_OBSERVATION)
                .bind(&target.id)
                .bind(projected.kind)
                .bind(projected.id)
                .bind(projected.service_id)
                .bind(&projected.sort_key)
                .bind(&projected.end_key)
                .bind(row.try_get::<&str, _>("observed_at")?)
                .bind(&projected.payload)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("DELETE FROM observations WHERE account_id=?")
            .bind(&source.id)
            .execute(&mut *tx)
            .await?;
        let coverage: Vec<String> =
            sqlx::query_scalar("SELECT payload FROM coverage WHERE account_id IN (?,?)")
                .bind(&source.id)
                .bind(&target.id)
                .fetch_all(&mut *tx)
                .await?;
        let mut latest = std::collections::BTreeMap::new();
        for payload in coverage {
            let mut value: Coverage = serde_json::from_str(&payload)?;
            if value.state != CoverageState::Partial
                || value.reason.as_deref() != Some("document_import")
            {
                return Err(ImportError::Binding);
            }
            value.account_id.clone_from(&target.id);
            let key = crate::store::enum_key(&value.capability)?;
            let old: Option<&Coverage> = latest.get(&key);
            if old.is_none_or(|old| old.checked_at < value.checked_at) {
                latest.insert(key, value);
            }
        }
        for value in latest.values() {
            Self::write_coverage(&mut tx, value).await?;
        }
        sqlx::query("DELETE FROM coverage WHERE account_id=?")
            .bind(&source.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE import_origins SET account_id=? WHERE account_id=?")
            .bind(&target.id)
            .bind(&source.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE accounts SET binding=?,binding_hash=?,imported_at=max(coalesce(imported_at,''),coalesce((SELECT imported_at FROM accounts WHERE id=?),'')) WHERE id=?")
            .bind(serde_json::to_string(&target)?).bind(binding_hash(&target)?).bind(&source.id).bind(&target.id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM accounts WHERE id=?")
            .bind(&source.id)
            .execute(&mut *tx)
            .await?;
        let receipt = ImportAccountConsolidationReceipt {
            account_id: target.id,
            absorbed_account_id: source.id,
            records_moved: input.expected_source_records,
            replayed: false,
        };
        let saved = SavedConsolidation {
            request_digest,
            receipt: receipt.clone(),
        };
        sqlx::query("UPDATE import_origins SET consolidation=? WHERE id=?")
            .bind(serde_json::to_string(&saved)?)
            .bind(&input.source.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(receipt)
    }
}
