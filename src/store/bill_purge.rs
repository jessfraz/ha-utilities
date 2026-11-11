//! Explicit bill removal retains intake identities that future imports must reject.
use crate::{
    config::AccountBinding,
    imports::{BillScope, ImportBillPurge, ImportBillPurgeReceipt, ImportError, ImportRecord},
    store::{Store, connections::binding_hash, queries},
};
use sqlx::{Row, Sqlite, Transaction};
use std::collections::BTreeSet;

const ORIGIN: &str = "SELECT a.id,a.import_only=1 AND a.connection_id IS NULL
        AND NOT EXISTS(SELECT 1 FROM sync_jobs WHERE account_id=a.id) AS eligible,
        o.binding AS origin_binding,a.binding AS canonical_binding
    FROM import_origins o JOIN accounts a ON a.id=o.account_id
    WHERE o.id=? AND a.deployment=? AND a.binding IS NOT NULL";
const TOMBSTONE: &str =
    "SELECT revision,removed_services FROM import_bill_tombstones WHERE origin_id=? AND bill_id=?";
const BILL: &str =
    "SELECT revision,payload FROM imported_records WHERE account_id=? AND kind='bill' AND id=?";
const SAVE_TOMBSTONE: &str = "INSERT INTO import_bill_tombstones(origin_id,bill_id,revision,removed_services) VALUES(?,?,?,?)";
const SAVE_REMOVED: &str =
    "INSERT INTO import_bill_removed_records(origin_id,bill_id,kind,record_id)
    SELECT ?,?,kind,id FROM imported_records WHERE account_id=? AND kind='bill' AND id=?
    UNION ALL SELECT ?,?,kind,id FROM imported_records WHERE account_id=? AND bill_id=?";
const DIGESTS: &str = "SELECT DISTINCT e.digest FROM import_bill_removed_records t
    JOIN imported_evidence e ON e.account_id=? AND e.kind=t.kind AND e.id=t.record_id
    WHERE t.origin_id=? AND t.bill_id=? AND e.deployment=?";
const DELETE_EVIDENCE: &str = "DELETE FROM imported_evidence WHERE account_id=?
    AND (kind,id) IN (SELECT kind,record_id FROM import_bill_removed_records WHERE origin_id=? AND bill_id=?)";
const DELETE_REVISIONS: &str = "DELETE FROM imported_revisions WHERE account_id=?
    AND (kind,id) IN (SELECT kind,record_id FROM import_bill_removed_records WHERE origin_id=? AND bill_id=?)";
const DELETE_PROJECTIONS: &str = "DELETE FROM observations WHERE account_id=?
    AND (kind,id) IN (SELECT kind,record_id FROM import_bill_removed_records WHERE origin_id=? AND bill_id=?)";
const DELETE_RECORDS: &str = "DELETE FROM imported_records WHERE account_id=?
    AND (kind,id) IN (SELECT kind,record_id FROM import_bill_removed_records WHERE origin_id=? AND bill_id=?)";
const DELETE_DOCUMENTS: &str = "DELETE FROM import_documents AS d WHERE deployment=?
    AND digest IN (SELECT value FROM json_each(?))
    AND NOT EXISTS(SELECT 1 FROM imported_evidence e WHERE e.deployment=d.deployment AND e.digest=d.digest)";
const REMOVED_INPUT: &str = "SELECT EXISTS(SELECT 1 FROM json_each(?) candidate
    JOIN import_bill_removed_records removed ON removed.origin_id=? AND removed.record_id=candidate.value)";
const OTHER_SERVICE_ORIGIN: &str = "SELECT EXISTS(SELECT 1 FROM import_origins o JOIN json_each(o.binding,'$.services') service
    WHERE o.account_id=? AND o.id<>? AND json_extract(service.value,'$.id') IN (SELECT value FROM json_each(?)))";
const REMAINING_RECORDS: &str = "SELECT kind,id,payload FROM imported_records
    WHERE account_id=? AND (kind,id)>(?,?) ORDER BY kind,id LIMIT 200";
const REMAINING_PROJECTIONS: &str = "SELECT EXISTS(SELECT 1 FROM observations
    WHERE account_id=? AND service_id IN (SELECT value FROM json_each(?)))";
const SAVE_BINDING: &str = "UPDATE accounts SET binding=?,binding_hash=? WHERE id=?";

async fn remove_unused_services(
    tx: &mut Transaction<'_, Sqlite>,
    origin: &AccountBinding,
    canonical: &mut AccountBinding,
    services: &[String],
) -> Result<(), ImportError> {
    let requested: BTreeSet<_> = services.iter().map(String::as_str).collect();
    if requested.is_empty() {
        return Ok(());
    }
    if requested.iter().any(|id| {
        !origin.services.iter().any(|s| s.id == *id)
            || !canonical.services.iter().any(|s| s.id == *id)
    }) {
        return Err(ImportError::Binding);
    }
    let encoded = serde_json::to_string(services)?;
    let other_origin: bool = sqlx::query_scalar(OTHER_SERVICE_ORIGIN)
        .bind(&canonical.id)
        .bind(&origin.id)
        .bind(&encoded)
        .fetch_one(&mut **tx)
        .await?;
    let projection: bool = sqlx::query_scalar(REMAINING_PROJECTIONS)
        .bind(&canonical.id)
        .bind(&encoded)
        .fetch_one(&mut **tx)
        .await?;
    if other_origin || projection {
        return Err(ImportError::Binding);
    }
    let mut kind = String::new();
    let mut id = String::new();
    loop {
        let rows = sqlx::query(REMAINING_RECORDS)
            .bind(&canonical.id)
            .bind(&kind)
            .bind(&id)
            .fetch_all(&mut **tx)
            .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let record: ImportRecord = serde_json::from_str(row.try_get("payload")?)?;
            // An account-wide bill from this financial origin covers its bound
            // services even when the document omits individual service details.
            let covers_origin = matches!(&record,ImportRecord::Bill(bill) if bill.scope==BillScope::Account)
                && record.id(&origin.id)? == row.try_get::<&str, _>("id")?;
            if covers_origin || record.references_services(&requested) {
                return Err(ImportError::Binding);
            }
            kind = row.try_get("kind")?;
            id = row.try_get("id")?;
        }
    }
    canonical
        .services
        .retain(|service| !requested.contains(service.id.as_str()));
    sqlx::query(SAVE_BINDING)
        .bind(serde_json::to_string(canonical)?)
        .bind(binding_hash(canonical)?)
        .bind(&canonical.id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

impl Store {
    pub(crate) async fn reject_purged_bill_records(
        tx: &mut Transaction<'_, Sqlite>,
        origin_id: &str,
        records: &[ImportRecord],
    ) -> Result<(), ImportError> {
        let mut ids = BTreeSet::new();
        for record in records {
            ids.insert(record.id(origin_id)?);
            ids.extend(record.bill_link(origin_id)?);
        }
        let removed: bool = sqlx::query_scalar(REMOVED_INPUT)
            .bind(serde_json::to_string(&ids)?)
            .bind(origin_id)
            .fetch_one(&mut **tx)
            .await?;
        if removed {
            return Err(ImportError::PurgedBill);
        }
        Ok(())
    }

    pub async fn purge_import_bill(
        &self,
        deployment: &str,
        bill_id: &str,
        input: ImportBillPurge,
    ) -> Result<ImportBillPurgeReceipt, ImportError> {
        input.validate()?;
        if bill_id.is_empty() || bill_id.len() > 500 {
            return Err(ImportError::Invalid);
        }
        let mut tx = self.pool.begin_with(queries::BEGIN_INGEST).await?;
        let row = sqlx::query(ORIGIN)
            .bind(&input.origin_id)
            .bind(deployment)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ImportError::Binding)?;
        if !row.try_get::<bool, _>("eligible")? {
            return Err(ImportError::Binding);
        }
        let account_id: &str = row.try_get("id")?;
        let origin: AccountBinding = serde_json::from_str(row.try_get("origin_binding")?)?;
        let mut canonical: AccountBinding =
            serde_json::from_str(row.try_get("canonical_binding")?)?;
        if origin.id != input.origin_id || origin.deployment != deployment {
            return Err(ImportError::Binding);
        }
        let mut services = input.remove_unused_services.clone();
        services.sort_unstable();
        let prior: Option<(String, String)> = sqlx::query_as(TOMBSTONE)
            .bind(&input.origin_id)
            .bind(bill_id)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some((revision, removed_services)) = prior {
            let removed_services: Vec<String> = serde_json::from_str(&removed_services)?;
            if revision != input.expected_revision || removed_services != services {
                return Err(ImportError::Conflict);
            }
            tx.commit().await?;
            return Ok(ImportBillPurgeReceipt::default());
        }
        let row = sqlx::query(BILL)
            .bind(account_id)
            .bind(bill_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ImportError::Binding)?;
        let record: ImportRecord = serde_json::from_str(row.try_get("payload")?)?;
        if !matches!(record, ImportRecord::Bill(_)) || record.id(&input.origin_id)? != bill_id {
            return Err(ImportError::Binding);
        }
        let revision: &str = row.try_get("revision")?;
        if revision != input.expected_revision {
            return Err(ImportError::Conflict);
        }
        sqlx::query(SAVE_TOMBSTONE)
            .bind(&input.origin_id)
            .bind(bill_id)
            .bind(revision)
            .bind(serde_json::to_string(&services)?)
            .execute(&mut *tx)
            .await?;
        // Save the complete family before deleting FK children and their parents.
        let records = sqlx::query(SAVE_REMOVED)
            .bind(&input.origin_id)
            .bind(bill_id)
            .bind(account_id)
            .bind(bill_id)
            .bind(&input.origin_id)
            .bind(bill_id)
            .bind(account_id)
            .bind(bill_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let digests: Vec<String> = sqlx::query_scalar(DIGESTS)
            .bind(account_id)
            .bind(&input.origin_id)
            .bind(bill_id)
            .bind(deployment)
            .fetch_all(&mut *tx)
            .await?;
        for sql in [
            DELETE_EVIDENCE,
            DELETE_REVISIONS,
            DELETE_PROJECTIONS,
            DELETE_RECORDS,
        ] {
            sqlx::query(sql)
                .bind(account_id)
                .bind(&input.origin_id)
                .bind(bill_id)
                .execute(&mut *tx)
                .await?;
        }
        remove_unused_services(&mut tx, &origin, &mut canonical, &services).await?;
        let documents = sqlx::query(DELETE_DOCUMENTS)
            .bind(deployment)
            .bind(serde_json::to_string(&digests)?)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(ImportBillPurgeReceipt {
            removed: true,
            records,
            documents,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        imports::{
            BillInput, BillReference, BillScope, BillTotals, ImportAccount, ImportBillPurge,
            ImportProvider, ImportRecord, ImportRequest, SourceRef, SourceRole, digest,
        },
        models::{Service, ServiceKind},
        store::{Store, bill_purge::*},
    };
    use serde::Serialize;
    use sqlx::Row;

    #[derive(Serialize)]
    struct QueryPlan {
        name: &'static str,
        sql: &'static str,
        detail: Vec<String>,
        bytecode_operations: usize,
    }

    #[derive(Serialize)]
    struct Evidence {
        sqlite_version: String,
        accounts: i64,
        records_before_removal: usize,
        tombstones: i64,
        plans: Vec<QueryPlan>,
        real_purge_micros: u128,
    }

    #[tokio::test]
    async fn bill_purge_queries_use_scoped_indexes_on_representative_native_sqlite_data() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("plans.sqlite3"))
            .await
            .unwrap();
        let document = digest(b"Fictional SQL plan source");
        store
            .put_document("fixture", &document, b"Fictional SQL plan source")
            .await
            .unwrap();
        let mut first_bill = String::new();
        let mut current_bill = String::new();
        for account_index in 0..25 {
            let origin_id = format!("fictional-plan-origin-{account_index:02}");
            let account = ImportAccount {
                id: origin_id.clone(),
                provider: ImportProvider::Spectrum,
                start_on: "2025-01-01".parse().unwrap(),
                provider_account_sha256: Some(digest(origin_id.as_bytes())),
                services: vec![Service {
                    id: format!("{origin_id}-internet"),
                    kind: ServiceKind::Internet,
                }],
            };
            let records: Vec<_> = (0..80)
                .map(|index| {
                    ImportRecord::Bill(BillInput {
                        reference: BillReference::InvoiceNumber(format!("EXAMPLE-{index:03}")),
                        scope: BillScope::Account,
                        issued_on: "2025-02-01".parse().unwrap(),
                        due_on: None,
                        period: None,
                        totals: BillTotals {
                            new_charges_minor: Some(1000),
                            ..BillTotals::default()
                        },
                        services: Vec::new(),
                        charges: Vec::new(),
                        sources: vec![SourceRef {
                            sha256: document.clone(),
                            role: SourceRole::Invoice,
                            pages: Vec::new(),
                        }],
                        replaces_revision: None,
                    })
                })
                .collect();
            let removed = records[0].id(&origin_id).unwrap();
            if account_index == 0 {
                first_bill.clone_from(&removed);
                current_bill = records[1].id(&origin_id).unwrap();
            }
            store
                .import(
                    "fixture",
                    ImportRequest {
                        account,
                        currency: "USD".to_owned(),
                        records,
                    },
                )
                .await
                .unwrap();
            let revision = store
                .bill_details("fixture", &removed)
                .await
                .unwrap()
                .bill
                .revision;
            store
                .purge_import_bill(
                    "fixture",
                    &removed,
                    ImportBillPurge {
                        origin_id,
                        expected_revision: revision,
                        remove_unused_services: Vec::new(),
                    },
                )
                .await
                .unwrap();
        }
        let current_revision = store
            .bill_details("fixture", &current_bill)
            .await
            .unwrap()
            .bill
            .revision;
        sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();
        let origin = "fictional-plan-origin-00".to_owned();
        let candidates =
            serde_json::to_string(&vec![first_bill.clone(), current_bill.clone()]).unwrap();
        let digests = serde_json::to_string(&vec![document.clone()]).unwrap();
        let scoped = vec![origin.clone(), origin.clone(), first_bill.clone()];
        let service_ids = serde_json::to_string(&vec![format!("{origin}-internet")]).unwrap();
        let binding = store.binding("fixture", &origin).await.unwrap();
        let statements = [
            ("origin", ORIGIN, vec![origin.clone(), "fixture".to_owned()]),
            ("retry", TOMBSTONE, vec![origin.clone(), first_bill.clone()]),
            ("bill", BILL, vec![origin.clone(), current_bill.clone()]),
            (
                "save bill",
                SAVE_TOMBSTONE,
                vec![
                    origin.clone(),
                    current_bill.clone(),
                    current_revision.clone(),
                    "[]".to_owned(),
                ],
            ),
            (
                "save family",
                SAVE_REMOVED,
                vec![
                    origin.clone(),
                    current_bill.clone(),
                    origin.clone(),
                    current_bill.clone(),
                    origin.clone(),
                    current_bill.clone(),
                    origin.clone(),
                    current_bill.clone(),
                ],
            ),
            (
                "digests",
                DIGESTS,
                vec![
                    origin.clone(),
                    origin.clone(),
                    first_bill.clone(),
                    "fixture".to_owned(),
                ],
            ),
            ("delete evidence", DELETE_EVIDENCE, scoped.clone()),
            ("delete revisions", DELETE_REVISIONS, scoped.clone()),
            ("delete projections", DELETE_PROJECTIONS, scoped.clone()),
            ("delete records", DELETE_RECORDS, scoped),
            (
                "document GC",
                DELETE_DOCUMENTS,
                vec!["fixture".to_owned(), digests],
            ),
            (
                "reject removed input",
                REMOVED_INPUT,
                vec![candidates, origin.clone()],
            ),
            (
                "other service origin",
                OTHER_SERVICE_ORIGIN,
                vec![origin.clone(), origin.clone(), service_ids.clone()],
            ),
            (
                "surviving typed records",
                REMAINING_RECORDS,
                vec![origin.clone(), String::new(), String::new()],
            ),
            (
                "surviving service projections",
                REMAINING_PROJECTIONS,
                vec![origin.clone(), service_ids],
            ),
            (
                "retire selected services",
                SAVE_BINDING,
                vec![
                    serde_json::to_string(&binding).unwrap(),
                    binding_hash(&binding).unwrap(),
                    origin.clone(),
                ],
            ),
        ];
        let mut plans = Vec::new();
        for (name, sql, params) in statements {
            let mut plan = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")));
            let mut bytecode = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")));
            for value in &params {
                plan = plan.bind(value);
                bytecode = bytecode.bind(value);
            }
            let rows = plan.fetch_all(store.pool()).await.unwrap();
            let detail = rows
                .iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect::<Vec<_>>();
            assert!(
                !detail.iter().any(|line| line.starts_with("SCAN imported_")
                    || line.starts_with("SCAN observations")
                    || line.starts_with("SCAN removed")),
                "{name}: {detail:?}"
            );
            plans.push(QueryPlan {
                name,
                sql,
                detail,
                bytecode_operations: bytecode.fetch_all(store.pool()).await.unwrap().len(),
            });
        }
        let started = std::time::Instant::now();
        let receipt = store
            .purge_import_bill(
                "fixture",
                &current_bill,
                ImportBillPurge {
                    origin_id: origin,
                    expected_revision: current_revision,
                    remove_unused_services: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(receipt.records, 1);
        let real_purge_micros = started.elapsed().as_micros();
        let evidence = Evidence {
            sqlite_version: sqlx::query_scalar("SELECT sqlite_version()")
                .fetch_one(store.pool())
                .await
                .unwrap(),
            accounts: sqlx::query_scalar("SELECT count(*) FROM accounts")
                .fetch_one(store.pool())
                .await
                .unwrap(),
            records_before_removal: 2000,
            tombstones: sqlx::query_scalar("SELECT count(*) FROM import_bill_tombstones")
                .fetch_one(store.pool())
                .await
                .unwrap(),
            plans,
            real_purge_micros,
        };
        println!("{}", serde_json::to_string(&evidence).unwrap());
    }
}
