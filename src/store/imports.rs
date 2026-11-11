use crate::{
    config::{AccountBinding, valid_id},
    imports::{
        BillDetails, BillReference, ImportAccountPurge, ImportAccountPurgeReceipt, ImportError,
        ImportReceipt, ImportRecord, ImportRequest, ImportedRecord, MAX_DOCUMENT_BYTES, Quality,
        combined_sources, digest, valid_digest,
    },
    models::{Bill, Capability, Coverage, CoverageState, Observation, Payment, ProviderId, Usage},
    store::{ObservationRow, Store, StoreError, connections, queries},
};
use chrono::Utc;
use sqlx::{QueryBuilder, Row, Sqlite, Transaction};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct ImportState {
    pub id: String,
    pub import_only: bool,
    pub imported_at: Option<chrono::DateTime<Utc>>,
}

impl From<sqlx::Error> for ImportError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(StoreError::from(error))
    }
}

impl From<serde_json::Error> for ImportError {
    fn from(error: serde_json::Error) -> Self {
        Self::Store(StoreError::from(error))
    }
}

impl Store {
    pub async fn put_document(
        &self,
        deployment: &str,
        sha256: &str,
        bytes: &[u8],
    ) -> Result<(), ImportError> {
        if bytes.is_empty()
            || bytes.len() > MAX_DOCUMENT_BYTES
            || !valid_digest(sha256)
            || digest(bytes) != sha256
        {
            return Err(ImportError::Invalid);
        }
        sqlx::query("INSERT INTO import_documents(deployment,digest,bytes) VALUES(?,?,?) ON CONFLICT DO NOTHING")
            .bind(deployment).bind(sha256).bind(bytes).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn document(&self, deployment: &str, sha256: &str) -> Result<Vec<u8>, StoreError> {
        if !valid_digest(sha256) {
            return Err(StoreError::InvalidQuery);
        }
        sqlx::query_scalar(
            "SELECT bytes FROM import_documents d WHERE d.deployment=? AND d.digest=? AND EXISTS(SELECT 1 FROM imported_evidence e JOIN accounts a ON a.id=e.account_id WHERE e.deployment=d.deployment AND e.digest=d.digest AND a.deployment=d.deployment)",
        )
        .bind(deployment)
        .bind(sha256)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(StoreError::NotFound)
    }

    pub async fn is_import_only(&self, account_id: &str) -> Result<bool, StoreError> {
        Ok(
            sqlx::query_scalar::<_, bool>("SELECT import_only FROM accounts WHERE id=?")
                .bind(account_id)
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or(false),
        )
    }

    pub(crate) async fn import_account_ids(
        &self,
        deployment: &str,
        provider: ProviderId,
    ) -> Result<Vec<String>, StoreError> {
        let provider = crate::store::enum_key(&provider)?;
        Ok(sqlx::query_scalar(
            "SELECT id FROM accounts WHERE deployment=? AND import_only=1
                AND json_extract(binding,'$.provider')=? ORDER BY id",
        )
        .bind(deployment)
        .bind(provider)
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn purge_import_account(
        &self,
        deployment: &str,
        account_id: &str,
        input: ImportAccountPurge,
    ) -> Result<ImportAccountPurgeReceipt, ImportError> {
        if !valid_id(account_id) || input.expected_records == 0 {
            return Err(ImportError::Invalid);
        }
        let provider = crate::store::enum_key(&input.provider)?;
        let mut tx = self.pool.begin_with(queries::BEGIN_INGEST).await?;
        let account: Option<(bool, i64)> = sqlx::query_as(
            "SELECT import_only=1 AND connection_id IS NULL
                    AND COALESCE(json_extract(binding,'$.provider')=?,0)
                    AND NOT EXISTS (SELECT 1 FROM sync_jobs WHERE account_id=accounts.id),
                    (SELECT count(*) FROM imported_records WHERE account_id=accounts.id)
             FROM accounts WHERE id=? AND deployment=?",
        )
        .bind(provider)
        .bind(account_id)
        .bind(deployment)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((eligible, records)) = account else {
            tx.commit().await?;
            return Ok(ImportAccountPurgeReceipt::default());
        };
        if !eligible || u64::try_from(records).ok() != Some(input.expected_records) {
            return Err(ImportError::Binding);
        }
        let digests: Vec<String> = sqlx::query_scalar(
            "DELETE FROM imported_evidence WHERE deployment=? AND account_id=? RETURNING digest",
        )
        .bind(deployment)
        .bind(account_id)
        .fetch_all(&mut *tx)
        .await?;
        for sql in [
            "DELETE FROM imported_revisions WHERE account_id=?",
            "DELETE FROM import_receipts WHERE account_id=?",
            "DELETE FROM imported_records WHERE account_id=?",
            "DELETE FROM observations WHERE account_id=?",
            "DELETE FROM coverage WHERE account_id=?",
        ] {
            sqlx::query(sql).bind(account_id).execute(&mut *tx).await?;
        }
        sqlx::query("DELETE FROM accounts WHERE id=? AND deployment=?")
            .bind(account_id)
            .bind(deployment)
            .execute(&mut *tx)
            .await?;
        let documents = sqlx::query(
            "DELETE FROM import_documents AS d WHERE deployment=?
                AND digest IN (SELECT value FROM json_each(?))
                AND NOT EXISTS (SELECT 1 FROM imported_evidence e
                    WHERE e.deployment=d.deployment AND e.digest=d.digest)",
        )
        .bind(deployment)
        .bind(serde_json::to_string(&digests)?)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        Ok(ImportAccountPurgeReceipt {
            removed: true,
            records: input.expected_records,
            documents,
        })
    }

    pub(crate) async fn import_states(
        &self,
        accounts: &[String],
    ) -> Result<Vec<ImportState>, StoreError> {
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT id,import_only,imported_at FROM accounts WHERE id IN (",
        );
        crate::store::bind_accounts(&mut query, accounts);
        query.push(")");
        query
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| {
                let timestamp: Option<String> = row.try_get("imported_at")?;
                let imported_at = timestamp
                    .as_deref()
                    .map(chrono::DateTime::parse_from_rfc3339)
                    .transpose()
                    .map_err(|_| StoreError::InvalidObservation)?
                    .map(|time| time.with_timezone(&Utc));
                Ok(ImportState {
                    id: row.try_get("id")?,
                    import_only: row.try_get("import_only")?,
                    imported_at,
                })
            })
            .collect()
    }

    pub(crate) async fn imported_binding_exists(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        binding: &AccountBinding,
    ) -> Result<bool, StoreError> {
        let provider = crate::store::enum_key(&binding.provider)?;
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM accounts WHERE import_only=1 AND deployment=? AND json_extract(binding,'$.provider')=? AND json_extract(binding,'$.provider_account')=?)")
            .bind(&binding.deployment)
            .bind(provider)
            .bind(digest(binding.provider_account.as_bytes()))
            .fetch_one(&mut **tx).await?)
    }

    pub async fn import(
        &self,
        deployment: &str,
        input: ImportRequest,
    ) -> Result<ImportReceipt, ImportError> {
        input.validate()?;
        let mut records = input.records.clone();
        for record in &mut records {
            *record.provenance_mut().0 = combined_sources(record.sources().iter());
        }
        // Withdraw estimate demotions before testing replacement reported intervals.
        records.sort_by_key(|record| !matches!(record, ImportRecord::MeterInterval(interval) if interval.quality == Quality::Estimated));
        let mut canonical = records
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?;
        canonical.sort_unstable();
        let receipt_digest = digest(&serde_json::to_vec(&(&input.currency, &canonical))?);
        let mut tx = self.pool.begin_with(queries::BEGIN_INGEST).await?;
        let (binding, import_only) = import_binding(&mut tx, deployment, &input).await?;
        let replayed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM import_receipts WHERE account_id=? AND digest=?)",
        )
        .bind(&binding.id)
        .bind(&receipt_digest)
        .fetch_one(&mut *tx)
        .await?;
        if replayed {
            if import_only {
                Self::restore_unknown_usage(&mut tx, &binding, &input.records).await?;
            }
            tx.commit().await?;
            return Ok(ImportReceipt {
                unchanged: records.len(),
                replayed: true,
                ..ImportReceipt::default()
            });
        }
        let documents: BTreeSet<_> = input.document_digests().collect();
        let mut query =
            QueryBuilder::<Sqlite>::new("SELECT count(*) FROM import_documents WHERE deployment=");
        query.push_bind(deployment).push(" AND digest IN (");
        {
            let mut list = query.separated(",");
            for digest in &documents {
                list.push_bind(*digest);
            }
        }
        query.push(")");
        let stored: i64 = query.build_query_scalar().fetch_one(&mut *tx).await?;
        if usize::try_from(stored).ok() != Some(documents.len()) {
            return Err(ImportError::MissingDocument);
        }
        let now: String = sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')")
            .fetch_one(&mut *tx)
            .await?;
        let observed_at = chrono::DateTime::parse_from_rfc3339(&now)
            .map_err(|_| StoreError::InvalidObservation)?
            .with_timezone(&Utc);
        let mut receipt = ImportReceipt::default();
        let mut capabilities = Vec::new();
        for incoming in records {
            let kind = incoming.kind();
            let (id, bill_link) = if import_only {
                (incoming.id(&binding.id)?, incoming.bill_link(&binding.id)?)
            } else {
                native_wm_record(&mut tx, &binding, &incoming).await?
            };
            let row = sqlx::query("SELECT revision,payload FROM imported_records WHERE account_id=? AND kind=? AND id=?")
                .bind(&binding.id).bind(kind).bind(&id).fetch_optional(&mut *tx).await?;
            let (value, facts_changed) = if let Some(row) = row {
                let revision: String = row.try_get("revision")?;
                let current: ImportRecord = serde_json::from_str(row.try_get("payload")?)?;
                if incoming.facts() != current.facts()
                    && incoming.replacement().is_some_and(|r| r != revision)
                {
                    return Err(ImportError::Conflict);
                }
                let (value, corrected) = incoming.reconcile(&current, &revision)?;
                if value == current {
                    receipt.unchanged += 1;
                    continue;
                }
                let changed = value.facts() != current.facts();
                if corrected {
                    receipt.corrected += 1;
                } else if changed {
                    receipt.enriched += 1;
                } else {
                    receipt.evidence_added += 1;
                }
                (value, changed)
            } else {
                if incoming.replacement().is_some() {
                    return Err(ImportError::Conflict);
                }
                receipt.inserted += 1;
                (incoming, true)
            };
            value.validate(&input.account)?;
            let bill_link = if import_only {
                value.bill_link(&binding.id)?
            } else {
                bill_link
            };
            let facts = serde_json::to_string(&value.facts())?;
            let revision = digest(facts.as_bytes());
            let payload = serde_json::to_string(&value)?;
            sqlx::query("INSERT INTO imported_records(account_id,kind,id,bill_id,revision,payload) VALUES(?,?,?,?,?,?) ON CONFLICT(account_id,kind,id) DO UPDATE SET bill_id=excluded.bill_id,revision=excluded.revision,payload=excluded.payload")
                .bind(&binding.id).bind(kind).bind(&id).bind(&bill_link).bind(&revision).bind(payload)
                .execute(&mut *tx).await?;
            sqlx::query("INSERT INTO imported_revisions(account_id,kind,id,revision,facts,sources) VALUES(?,?,?,?,?,?) ON CONFLICT(account_id,kind,id,revision) DO UPDATE SET sources=excluded.sources")
                    .bind(&binding.id).bind(kind).bind(&id).bind(&revision).bind(facts)
                    .bind(serde_json::to_string(value.sources())?)
                    .execute(&mut *tx).await?;
            for source in value.sources() {
                sqlx::query("INSERT INTO imported_evidence(deployment,digest,account_id,kind,id) VALUES(?,?,?,?,?) ON CONFLICT DO NOTHING")
                    .bind(deployment).bind(&source.sha256).bind(&binding.id).bind(kind).bind(&id)
                    .execute(&mut *tx).await?;
            }
            if import_only && facts_changed {
                if let Some(observation) = value.observation(&binding, &id, observed_at)? {
                    let row = ObservationRow::new(&binding, &observation)?;
                    if row.kind == "usage" {
                        let overlap: bool = sqlx::query_scalar(queries::OVERLAP)
                            .bind(&binding.id)
                            .bind(row.service_id)
                            .bind(row.id)
                            .bind(&row.sort_key)
                            .bind(&row.end_key)
                            .fetch_one(&mut *tx)
                            .await?;
                        if overlap {
                            return Err(ImportError::Conflict);
                        }
                    }
                    sqlx::query(queries::UPSERT_OBSERVATION)
                        .bind(&binding.id)
                        .bind(row.kind)
                        .bind(row.id)
                        .bind(row.service_id)
                        .bind(&row.sort_key)
                        .bind(&row.end_key)
                        .bind(&now)
                        .bind(row.payload)
                        .execute(&mut *tx)
                        .await?;
                    let capability = match observation {
                        Observation::Bill(_) => Capability::Bills,
                        Observation::Payment(_) => Capability::Payments,
                        Observation::Balance(_) => Capability::Balances,
                        Observation::Usage(_) => Capability::Usage,
                        _ => return Err(ImportError::Invalid),
                    };
                    if !capabilities.contains(&capability) {
                        capabilities.push(capability);
                    }
                } else if kind == "meter_interval" {
                    sqlx::query(
                        "DELETE FROM observations WHERE account_id=? AND kind='usage' AND id=?",
                    )
                    .bind(&binding.id)
                    .bind(&id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        for capability in capabilities {
            Self::write_coverage(
                &mut tx,
                &Coverage {
                    account_id: binding.id.clone(),
                    capability,
                    state: CoverageState::Partial,
                    reason: Some("document_import".to_owned()),
                    checked_at: observed_at,
                    from: None,
                    to: None,
                },
            )
            .await?;
        }
        if import_only {
            Self::restore_unknown_usage(&mut tx, &binding, &input.records).await?;
        }
        sqlx::query("INSERT INTO import_receipts(account_id,digest) VALUES(?,?)")
            .bind(&binding.id)
            .bind(&receipt_digest)
            .execute(&mut *tx)
            .await?;
        if receipt.inserted + receipt.enriched + receipt.evidence_added + receipt.corrected > 0 {
            sqlx::query("UPDATE accounts SET imported_at=? WHERE id=?")
                .bind(&now)
                .bind(&binding.id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(receipt)
    }

    async fn restore_unknown_usage(
        tx: &mut Transaction<'_, Sqlite>,
        binding: &AccountBinding,
        incoming: &[ImportRecord],
    ) -> Result<(), ImportError> {
        let requested = incoming
            .iter()
            .filter(|record| matches!(record, ImportRecord::MeterInterval(value) if value.quality == Quality::Unknown))
            .map(|record| Ok((record.id(&binding.id)?, record.facts())))
            .collect::<Result<BTreeMap<_, _>, ImportError>>()?;
        if requested.is_empty() {
            return Ok(());
        }
        let ids = requested.keys().cloned().collect::<Vec<_>>();
        let mut query = unknown_usage_records_sql(&binding.id, &ids);
        let stored = query
            .build_query_as::<(String, String)>()
            .fetch_all(&mut **tx)
            .await?;
        if stored.is_empty() {
            return Ok(());
        }
        let now: String = sqlx::query_scalar("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')")
            .fetch_one(&mut **tx)
            .await?;
        let observed_at = chrono::DateTime::parse_from_rfc3339(&now)
            .map_err(|_| StoreError::InvalidObservation)?
            .with_timezone(&Utc);
        let mut candidates = Vec::new();
        for (id, payload) in stored {
            let record: ImportRecord = serde_json::from_str(&payload)?;
            // An old receipt must not resurrect facts changed by a later correction.
            if requested.get(&id) != Some(&record.facts()) {
                continue;
            }
            if let Some(Observation::Usage(usage)) =
                record.observation(binding, &id, observed_at)?
            {
                candidates.push(usage);
            }
        }
        if candidates.is_empty() {
            return Ok(());
        }
        let mut query = nonoverlapping_usage_sql(&binding.id, &candidates);
        let eligible = query
            .build_query_scalar::<String>()
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut projected = false;
        for usage in &candidates {
            // Legacy unknown windows could overlap each other or actual usage.
            // Keep ambiguous windows raw instead of replacing existing observations.
            if !eligible.contains(&usage.id)
                || candidates.iter().any(|other| {
                    other.id != usage.id
                        && eligible.contains(&other.id)
                        && other.service_id == usage.service_id
                        && other.start < usage.end
                        && other.end > usage.start
                })
            {
                continue;
            }
            let observation = Observation::Usage(usage.clone());
            let row = ObservationRow::new(binding, &observation)?;
            sqlx::query(queries::UPSERT_OBSERVATION)
                .bind(&binding.id)
                .bind(row.kind)
                .bind(row.id)
                .bind(row.service_id)
                .bind(&row.sort_key)
                .bind(&row.end_key)
                .bind(&now)
                .bind(row.payload)
                .execute(&mut **tx)
                .await?;
            projected = true;
        }
        if projected {
            Self::write_coverage(
                tx,
                &Coverage {
                    account_id: binding.id.clone(),
                    capability: Capability::Usage,
                    state: CoverageState::Partial,
                    reason: Some("document_import".to_owned()),
                    checked_at: observed_at,
                    from: None,
                    to: None,
                },
            )
            .await?;
        }
        Ok(())
    }

    pub async fn bill_details(
        &self,
        deployment: &str,
        id: &str,
    ) -> Result<BillDetails, StoreError> {
        let row = sqlx::query(
            "SELECT r.account_id,r.revision,r.payload,json_extract(a.binding,'$.currency') AS currency FROM imported_records r JOIN accounts a ON a.id=r.account_id WHERE a.deployment=? AND r.kind='bill' AND r.id=?",
        )
        .bind(deployment).bind(id).fetch_optional(&self.pool).await?.ok_or(StoreError::NotFound)?;
        let account: &str = row.try_get("account_id")?;
        let related = sqlx::query("SELECT id,revision,payload FROM imported_records WHERE account_id=? AND bill_id=? ORDER BY kind,id LIMIT 200")
            .bind(account).bind(id).fetch_all(&self.pool).await?
            .into_iter().map(|row| Ok(ImportedRecord {
                id: row.try_get("id")?, revision: row.try_get("revision")?,
                record: serde_json::from_str(row.try_get("payload")?)?,
            })).collect::<Result<Vec<_>, StoreError>>()?;
        Ok(BillDetails {
            currency: row.try_get("currency")?,
            bill: ImportedRecord {
                id: id.to_owned(),
                revision: row.try_get("revision")?,
                record: serde_json::from_str(row.try_get("payload")?)?,
            },
            related,
        })
    }
}

async fn import_binding(
    tx: &mut Transaction<'_, Sqlite>,
    deployment: &str,
    input: &ImportRequest,
) -> Result<(AccountBinding, bool), ImportError> {
    let proposed = input.binding(deployment);
    let existing = sqlx::query("SELECT binding,import_only,import_start_on FROM accounts WHERE id=? AND deployment=? AND binding IS NOT NULL")
        .bind(&proposed.id).bind(deployment).fetch_optional(&mut **tx).await?;
    let start = input.account.start_on.to_string();
    if let Some(row) = existing {
        let current: AccountBinding = serde_json::from_str(row.try_get("binding")?)?;
        let import_only: bool = row.try_get("import_only")?;
        let start_on: Option<String> = row.try_get("import_start_on")?;
        if start_on
            .as_deref()
            .is_some_and(|existing| existing != start)
            || current.provider != proposed.provider
            || current.currency != proposed.currency
        {
            return Err(ImportError::Binding);
        }
        if import_only {
            if current.provider_account != proposed.provider_account
                || !current
                    .services
                    .iter()
                    .map(|s| (&s.id, s.kind))
                    .eq(proposed.services.iter().map(|s| (&s.id, s.kind)))
            {
                return Err(ImportError::Binding);
            }
        } else if current.provider != ProviderId::WasteManagement
            || input.account.provider_account_sha256.as_deref()
                != Some(&digest(current.provider_account.as_bytes()))
            || proposed.services.iter().any(|p| {
                !current
                    .services
                    .iter()
                    .any(|c| c.id == p.id && c.kind == p.kind)
            })
        {
            return Err(ImportError::Binding);
        }
        if start_on.is_none() {
            sqlx::query("UPDATE accounts SET import_start_on=? WHERE id=?")
                .bind(&start)
                .bind(&current.id)
                .execute(&mut **tx)
                .await?;
        }
        return Ok((current, import_only));
    }
    let bindings = sqlx::query_scalar::<_, String>(connections::queries::BINDINGS)
        .bind(deployment)
        .fetch_all(&mut **tx)
        .await?;
    if bindings.len() >= 100 {
        return Err(ImportError::Binding);
    }
    for existing in bindings {
        let existing: AccountBinding = serde_json::from_str(&existing)?;
        if existing
            .services
            .iter()
            .any(|s| proposed.services.iter().any(|p| p.id == s.id))
            || (existing.provider == proposed.provider
                && (existing.provider_account == proposed.provider_account
                    || (existing.provider == ProviderId::WasteManagement
                        && (input.account.provider_account_sha256.is_none()
                            || digest(existing.provider_account.as_bytes())
                                == proposed.provider_account))))
        {
            return Err(ImportError::Binding);
        }
    }
    Store::register_binding(tx, &proposed, None)
        .await
        .map_err(|error| match error {
            StoreError::BindingChanged => ImportError::Binding,
            error => error.into(),
        })?;
    sqlx::query("UPDATE accounts SET import_only=1,import_start_on=? WHERE id=?")
        .bind(&start)
        .bind(&proposed.id)
        .execute(&mut **tx)
        .await?;
    Ok((proposed, true))
}

fn unknown_usage_records_sql(account: &str, ids: &[String]) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new(
        "SELECT raw.id,raw.payload FROM imported_records AS raw WHERE raw.account_id=",
    );
    query
        .push_bind(account)
        .push(" AND raw.kind='meter_interval' AND raw.id IN (");
    crate::store::bind_accounts(&mut query, ids);
    query.push(") AND NOT EXISTS (SELECT 1 FROM observations WHERE account_id=raw.account_id AND kind='usage' AND id=raw.id)");
    query
}

fn nonoverlapping_usage_sql(account: &str, candidates: &[Usage]) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new("WITH candidate(id,service_id,start_key,end_key) AS (");
    query.push_values(candidates, |mut values, usage| {
        values
            .push_bind(usage.id.clone())
            .push_bind(usage.service_id.clone())
            .push_bind(usage.start.to_rfc3339())
            .push_bind(usage.end.to_rfc3339());
    });
    query.push(") SELECT candidate.id FROM candidate WHERE NOT EXISTS (SELECT 1 FROM (SELECT sort_key FROM observations WHERE account_id=")
        .push_bind(account)
        .push(" AND kind='usage' AND service_id=candidate.service_id AND end_key>candidate.start_key ORDER BY end_key LIMIT 1) WHERE sort_key<candidate.end_key)");
    query
}

async fn native_wm_record(
    tx: &mut Transaction<'_, Sqlite>,
    binding: &AccountBinding,
    record: &ImportRecord,
) -> Result<(String, Option<String>), ImportError> {
    let lookup = |invoice: &str| format!("{}:bill:{invoice}", binding.id);
    let load = "SELECT payload FROM observations WHERE account_id=? AND kind=? AND id=?";
    match record {
        ImportRecord::Bill(value) => {
            let BillReference::InvoiceNumber(invoice) = &value.reference else {
                return Err(ImportError::Conflict);
            };
            let id = lookup(invoice);
            let json: String = sqlx::query_scalar(load)
                .bind(&binding.id)
                .bind("bill")
                .bind(&id)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(ImportError::Conflict)?;
            let bill: Bill = serde_json::from_str(&json)?;
            if bill.issued_on != value.issued_on
                || value.totals.new_charges_minor != Some(bill.total.minor_units)
            {
                return Err(ImportError::Conflict);
            }
            Ok((id, None))
        }
        ImportRecord::Payment(value) => {
            let Some(BillReference::InvoiceNumber(invoice)) = &value.bill else {
                return Err(ImportError::Conflict);
            };
            let bill_id = lookup(invoice);
            let id = crate::utility::waste_management::payment_id(
                &binding.id,
                &bill_id,
                value.paid_on,
                value.amount_minor,
                &value.reference,
            );
            let json: String = sqlx::query_scalar(load)
                .bind(&binding.id)
                .bind("payment")
                .bind(&id)
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(ImportError::Conflict)?;
            let payment: Payment = serde_json::from_str(&json)?;
            if payment.bill_id.as_deref() != Some(&bill_id)
                || payment.paid_on != value.paid_on
                || payment.amount.minor_units != value.amount_minor
            {
                return Err(ImportError::Conflict);
            }
            Ok((id, Some(bill_id)))
        }
        _ => Err(ImportError::Conflict),
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        config::Config,
        imports::{
            ExactDecimal, ImportAccount, ImportRecord, ImportRequest, MeterIntervalInput, Quality,
            Quantity, QuantityUnit, SourceRef, SourceRole, digest,
        },
        models::{Observation, ProviderId, Service, ServiceKind, Usage, UsageUnit},
        store::{
            Store,
            imports::{nonoverlapping_usage_sql, unknown_usage_records_sql},
        },
        utility::SyncPage,
    };
    use chrono::{Duration, Utc};
    use rust_decimal::Decimal;
    use sqlx::{Execute, Row, Sqlite, sqlite::SqliteArguments};

    async fn explain(
        name: &str,
        store: &Store,
        mut query: sqlx::query::Query<'_, Sqlite, SqliteArguments>,
    ) -> Vec<String> {
        let args = query.take_arguments().unwrap().unwrap_or_default();
        let plans = sqlx::query_with(
            sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {}", query.sql().as_str())),
            args,
        )
        .fetch_all(store.pool())
        .await
        .unwrap()
        .iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect::<Vec<_>>();
        println!("{name}: {}", plans.join("; "));
        plans
    }

    #[tokio::test]
    async fn unknown_replay_queries_use_scoped_indexes() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("replay-plans.sqlite3"))
            .await
            .unwrap();
        let config: Config = toml::from_str(include_str!("../../config/example.toml")).unwrap();
        store.register(&config).await.unwrap();
        let now = Utc::now();
        let start = now - Duration::days(300);
        let binding = &config.accounts[1];
        let observations = (0..5_000)
            .map(|index| {
                Observation::Usage(Usage {
                    id: format!("plan-native:{index}"),
                    account_id: binding.id.clone(),
                    service_id: binding.services[0].id.clone(),
                    start: start + Duration::hours(index),
                    end: start + Duration::hours(index + 1),
                    quantity: Decimal::ONE,
                    unit: UsageUnit::Wh,
                    observed_at: now,
                })
            })
            .collect();
        store
            .ingest(
                binding,
                &SyncPage {
                    observations,
                    coverage: vec![],
                    continuation: None,
                },
            )
            .await
            .unwrap();
        let source = digest(b"Representative hourly export");
        store
            .put_document(&config.deployment, &source, b"Representative hourly export")
            .await
            .unwrap();
        let request = ImportRequest {
            account: ImportAccount {
                id: "plan-water".to_owned(),
                name: "Water".to_owned(),
                provider: ProviderId::Lvmwd,
                start_on: start.date_naive(),
                provider_account_sha256: None,
                services: vec![Service {
                    id: "plan-meter".to_owned(),
                    name: "Meter".to_owned(),
                    kind: ServiceKind::Water,
                }],
            },
            currency: "USD".to_owned(),
            records: (0..200)
                .map(|index| {
                    ImportRecord::MeterInterval(MeterIntervalInput {
                        service_id: "plan-meter".to_owned(),
                        start: start + Duration::hours(index),
                        end: start + Duration::hours(index + 1),
                        quality: Quality::Unknown,
                        quantity: Quantity {
                            value: ExactDecimal(Decimal::ONE),
                            unit: QuantityUnit::UsGallon,
                        },
                        sources: vec![SourceRef {
                            sha256: source.clone(),
                            role: SourceRole::UsageExport,
                            pages: vec![],
                        }],
                        replaces_revision: None,
                    })
                })
                .collect(),
        };
        store
            .import(&config.deployment, request.clone())
            .await
            .unwrap();
        sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();
        let binding = request.binding(&config.deployment);
        let ids = request
            .records
            .iter()
            .map(|record| record.id(&binding.id).unwrap())
            .collect::<Vec<_>>();
        let candidates = request
            .records
            .iter()
            .zip(&ids)
            .map(|(record, id)| {
                let Some(Observation::Usage(usage)) =
                    record.observation(&binding, id, now).unwrap()
                else {
                    panic!("expected reported usage")
                };
                usage
            })
            .collect::<Vec<_>>();
        let mut missing = unknown_usage_records_sql(&binding.id, &ids);
        let plans = explain("unknown_missing_projection", &store, missing.build()).await;
        assert!(
            plans
                .iter()
                .any(|plan| plan.starts_with("SEARCH raw USING PRIMARY KEY"))
        );
        assert!(
            !plans
                .iter()
                .any(|plan| plan == "SCAN raw" || plan == "SCAN observations")
        );
        let mut overlap = nonoverlapping_usage_sql(&binding.id, &candidates);
        let plans = explain("unknown_overlap", &store, overlap.build()).await;
        assert!(plans.iter().any(|plan| plan.contains("usage_overlap")));
        assert!(!plans.iter().any(|plan| plan == "SCAN observations"));
    }
}
