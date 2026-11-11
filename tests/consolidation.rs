mod support;

use ha_utilities::{
    client::{Client, Transport},
    imports::{
        BillInput, BillReference, BillScope, BillTotals, ImportAccount, ImportAccountConsolidation,
        ImportAccountConsolidationReceipt, ImportError, ImportProvider, ImportRecord,
        ImportRequest, PaymentInput, PaymentStatus, SourceRef, SourceRole, digest,
    },
    models::{ListQuery, Service, ServiceKind},
    store::Store,
};

fn input(id: &str, source: &str, reference: &str) -> ImportRequest {
    ImportRequest {
        account: ImportAccount {
            id: id.to_owned(),
            provider: ImportProvider::Spectrum,
            start_on: "2025-02-01".parse().unwrap(),
            provider_account_sha256: Some(digest(id.as_bytes())),
            services: vec![Service {
                id: format!("{id}-internet"),
                kind: ServiceKind::Internet,
            }],
        },
        currency: "USD".to_owned(),
        records: vec![ImportRecord::Bill(BillInput {
            reference: BillReference::InvoiceNumber(reference.to_owned()),
            scope: BillScope::Account,
            issued_on: "2025-02-20".parse().unwrap(),
            due_on: Some("2025-03-10".parse().unwrap()),
            period: None,
            totals: BillTotals {
                new_charges_minor: Some(12500),
                amount_due_minor: Some(12500),
                ..BillTotals::default()
            },
            services: vec![],
            charges: vec![],
            sources: vec![SourceRef {
                sha256: source.to_owned(),
                role: SourceRole::Invoice,
                pages: vec![],
            }],
            replaces_revision: None,
        })],
    }
}

fn operation(source: &ImportRequest, target: &ImportRequest) -> ImportAccountConsolidation {
    ImportAccountConsolidation {
        source: source.account.clone(),
        target: target.account.clone(),
        currency: source.currency.clone(),
        expected_source_records: 1,
        expected_target_records: 1,
    }
}

async fn setup(store: &Store, same_reference: bool) -> (ImportRequest, ImportRequest) {
    let source = digest(b"Fictional original PDF bytes");
    store
        .put_document("demo", &source, b"Fictional original PDF bytes")
        .await
        .unwrap();
    let first = input("fictional-construction", &source, "EXAMPLE-1");
    let second = input(
        "fictional-recurring",
        &source,
        if same_reference {
            "EXAMPLE-1"
        } else {
            "EXAMPLE-2"
        },
    );
    store.import("demo", first.clone()).await.unwrap();
    store.import("demo", second.clone()).await.unwrap();
    (first, second)
}

#[derive(Debug, PartialEq, Eq)]
struct FinancialSnapshot {
    records: Vec<(String, String, Option<String>, String, String)>,
    revisions: Vec<(String, String, String, String, String)>,
    evidence: Vec<(String, String, String, String)>,
    documents: Vec<(String, String, Vec<u8>)>,
    receipts: Vec<(String, String)>,
}

async fn financial_snapshot(store: &Store) -> FinancialSnapshot {
    FinancialSnapshot {
        records: sqlx::query_as("SELECT kind,id,bill_id,revision,payload FROM imported_records ORDER BY kind,id").fetch_all(store.pool()).await.unwrap(),
        revisions: sqlx::query_as("SELECT kind,id,revision,facts,sources FROM imported_revisions ORDER BY kind,id,revision").fetch_all(store.pool()).await.unwrap(),
        evidence: sqlx::query_as("SELECT deployment,digest,kind,id FROM imported_evidence ORDER BY deployment,digest,kind,id").fetch_all(store.pool()).await.unwrap(),
        documents: sqlx::query_as("SELECT deployment,digest,bytes FROM import_documents ORDER BY deployment,digest").fetch_all(store.pool()).await.unwrap(),
        receipts: sqlx::query_as("SELECT origin_id,digest FROM import_receipts ORDER BY origin_id,digest").fetch_all(store.pool()).await.unwrap(),
    }
}

#[tokio::test]
async fn consolidation_preserves_history_sources_links_and_scoped_replays() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("data.sqlite3"))
        .await
        .unwrap();
    let (source, target) = setup(&store, false).await;
    let source_id: String =
        sqlx::query_scalar("SELECT id FROM imported_records WHERE account_id=?")
            .bind(&source.account.id)
            .fetch_one(store.pool())
            .await
            .unwrap();
    let original = store.bill_details("demo", &source_id).await.unwrap();
    let corrected_pdf = b"Fictional corrected PDF bytes";
    let corrected_digest = digest(corrected_pdf);
    store
        .put_document("demo", &corrected_digest, corrected_pdf)
        .await
        .unwrap();
    let mut correction = source.clone();
    let ImportRecord::Bill(bill) = &mut correction.records[0] else {
        panic!("bill fixture")
    };
    bill.totals.new_charges_minor = Some(15000);
    bill.totals.amount_due_minor = Some(15000);
    bill.sources[0].sha256 = corrected_digest.clone();
    bill.replaces_revision = Some(original.bill.revision);
    store.import("demo", correction.clone()).await.unwrap();
    let mut payment = source.clone();
    payment.records = vec![ImportRecord::Payment(PaymentInput {
        reference: "EXAMPLE-PAYMENT".to_owned(),
        bill: Some(BillReference::InvoiceNumber("EXAMPLE-1".to_owned())),
        paid_on: "2025-03-01".parse().unwrap(),
        amount_minor: 15000,
        status: PaymentStatus::Confirmed,
        sources: vec![SourceRef {
            sha256: corrected_digest.clone(),
            role: SourceRole::PaymentConfirmation,
            pages: vec![],
        }],
        replaces_revision: None,
    })];
    store.import("demo", payment.clone()).await.unwrap();
    let before = financial_snapshot(&store).await;
    let bill_before = store
        .bill(&source_id, std::slice::from_ref(&source.account.id))
        .await
        .unwrap();
    let mut request = operation(&source, &target);
    request.expected_source_records = 2;
    let (one, two, concurrent_import) = tokio::join!(
        store.consolidate_import_accounts("demo", request.clone()),
        store.consolidate_import_accounts("demo", request.clone()),
        store.import("demo", source.clone())
    );
    assert!(concurrent_import.unwrap().replayed);
    let receipts = [one.unwrap(), two.unwrap()];
    assert_eq!(receipts.iter().filter(|r| r.replayed).count(), 1);
    assert_eq!(before, financial_snapshot(&store).await);
    let origin_bindings: Vec<String> =
        sqlx::query_scalar("SELECT binding FROM import_origins ORDER BY id")
            .fetch_all(store.pool())
            .await
            .unwrap();
    let fingerprints: std::collections::BTreeSet<_> = origin_bindings
        .iter()
        .map(|binding| {
            serde_json::from_str::<ha_utilities::config::AccountBinding>(binding)
                .unwrap()
                .provider_account
        })
        .collect();
    assert_eq!(
        fingerprints,
        [
            source.account.provider_account_sha256.clone().unwrap(),
            target.account.provider_account_sha256.clone().unwrap()
        ]
        .into_iter()
        .collect()
    );
    let bindings = store.bindings("demo").await.unwrap();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].id, target.account.id);
    assert_eq!(bindings[0].services.len(), 2);
    assert_eq!(
        bindings[0].provider_account,
        target.account.provider_account_sha256.clone().unwrap()
    );
    let mut bill_expected = bill_before;
    bill_expected.account_id.clone_from(&target.account.id);
    assert_eq!(
        store
            .bill(&source_id, std::slice::from_ref(&target.account.id))
            .await
            .unwrap(),
        bill_expected
    );
    assert_eq!(
        store
            .bill_details("demo", &source_id)
            .await
            .unwrap()
            .bill
            .revision,
        before.records.iter().find(|r| r.1 == source_id).unwrap().3
    );
    assert_eq!(
        store.document("demo", &corrected_digest).await.unwrap(),
        corrected_pdf
    );
    for request in [source.clone(), target.clone(), correction] {
        assert!(store.import("demo", request).await.unwrap().replayed);
    }
    assert_eq!(before, financial_snapshot(&store).await);
    assert!(
        store
            .import("demo", payment.clone())
            .await
            .unwrap()
            .replayed
    );
    let ImportRecord::Payment(value) = &mut payment.records[0] else {
        panic!("payment fixture")
    };
    value.reference = "EXAMPLE-PAYMENT-AFTER".to_owned();
    assert_eq!(store.import("demo", payment).await.unwrap().inserted, 1);
    let details = store.bill_details("demo", &source_id).await.unwrap();
    assert_eq!(details.related.len(), 2);
    let linked: Vec<String> = sqlx::query_scalar(
        "SELECT json_extract(payload,'$.bill_id') FROM observations WHERE kind='payment'",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(linked, vec![source_id.clone(), source_id.clone()]);
    for mut new in [source, target] {
        let ImportRecord::Bill(bill) = &mut new.records[0] else {
            panic!("bill fixture")
        };
        bill.reference = BillReference::InvoiceNumber("EXAMPLE-NEXT".to_owned());
        assert_eq!(store.import("demo", new.clone()).await.unwrap().inserted, 1);
        assert!(store.import("demo", new).await.unwrap().replayed);
    }
    assert_eq!(store.bindings("demo").await.unwrap().len(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM imported_records WHERE account_id=?")
            .bind(&request.target.id)
            .fetch_one(store.pool())
            .await
            .unwrap(),
        6
    );
    let foreign_keys = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(store.pool())
        .await
        .unwrap();
    assert!(foreign_keys.is_empty());
    let alias = store.binding("demo", &request.source.id).await;
    assert!(alias.is_err());
}

#[tokio::test]
async fn identical_batches_remain_separate_financial_origins_after_consolidation() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("data.sqlite3"))
        .await
        .unwrap();
    let (source, target) = setup(&store, true).await;
    let before = financial_snapshot(&store).await;
    assert_eq!(before.receipts[0].1, before.receipts[1].1);
    store
        .consolidate_import_accounts("demo", operation(&source, &target))
        .await
        .unwrap();
    for request in [source.clone(), target.clone()] {
        assert!(store.import("demo", request).await.unwrap().replayed);
    }
    assert_eq!(before, financial_snapshot(&store).await);
    let page = store
        .list::<ha_utilities::models::Bill>(
            "bill",
            &ListQuery::default(),
            std::slice::from_ref(&target.account.id),
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    let mut spoof = source;
    spoof.account.id = "new-unrelated-id".to_owned();
    spoof.account.services[0].id = "unrelated-service".to_owned();
    assert!(matches!(
        store.import("demo", spoof).await,
        Err(ImportError::Binding)
    ));
    let purge = ha_utilities::imports::ImportAccountPurge {
        provider: ha_utilities::models::ProviderId::Spectrum,
        expected_records: 2,
    };
    assert!(
        !store
            .purge_import_account("demo", "fictional-construction", purge.clone())
            .await
            .unwrap()
            .removed
    );
    assert!(
        store
            .purge_import_account("demo", &target.account.id, purge)
            .await
            .unwrap()
            .removed
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_origins")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_receipts")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn consolidation_rejects_mismatches_and_rolls_back_after_child_moves() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("data.sqlite3"))
        .await
        .unwrap();
    let (source, target) = setup(&store, false).await;
    let operation = operation(&source, &target);
    let before = financial_snapshot(&store).await;
    let mut cases = vec![];
    let mut wrong = operation.clone();
    wrong.expected_source_records += 1;
    cases.push(wrong);
    let mut wrong = operation.clone();
    wrong.source.provider_account_sha256 = Some(digest(b"wrong"));
    cases.push(wrong);
    let mut wrong = operation.clone();
    wrong.source.services[0].id = "wrong-service".to_owned();
    cases.push(wrong);
    let mut wrong = operation.clone();
    wrong.currency = "CAD".to_owned();
    cases.push(wrong);
    let mut wrong = operation.clone();
    wrong.source.provider = ImportProvider::Starlink;
    cases.push(wrong);
    for wrong in cases {
        assert!(
            store
                .consolidate_import_accounts("demo", wrong)
                .await
                .is_err()
        );
    }
    assert!(
        store
            .consolidate_import_accounts("other", operation.clone())
            .await
            .is_err()
    );
    assert_eq!(before, financial_snapshot(&store).await);
    sqlx::query("UPDATE accounts SET import_only=0 WHERE id=?")
        .bind(&source.account.id)
        .execute(store.pool())
        .await
        .unwrap();
    assert!(matches!(
        store
            .consolidate_import_accounts("demo", operation.clone())
            .await,
        Err(ImportError::Binding)
    ));
    sqlx::query("UPDATE accounts SET import_only=1 WHERE id=?")
        .bind(&source.account.id)
        .execute(store.pool())
        .await
        .unwrap();
    let owners_before: Vec<(String, String)> =
        sqlx::query_as("SELECT account_id,id FROM imported_records ORDER BY id")
            .fetch_all(store.pool())
            .await
            .unwrap();
    // Fail after parent and revision/evidence ownership have already moved.
    sqlx::query("UPDATE coverage SET payload=json_set(payload,'$.reason','native-provider') WHERE account_id=?").bind(&source.account.id).execute(store.pool()).await.unwrap();
    assert!(matches!(
        store
            .consolidate_import_accounts("demo", operation.clone())
            .await,
        Err(ImportError::Binding)
    ));
    assert_eq!(before, financial_snapshot(&store).await);
    assert_eq!(store.bindings("demo").await.unwrap().len(), 2);
    let owners_after: Vec<(String, String)> =
        sqlx::query_as("SELECT account_id,id FROM imported_records ORDER BY id")
            .fetch_all(store.pool())
            .await
            .unwrap();
    assert_eq!(owners_after, owners_before);
    sqlx::query("UPDATE coverage SET payload=json_set(payload,'$.reason','document_import') WHERE account_id=?").bind(&source.account.id).execute(store.pool()).await.unwrap();
    sqlx::query("INSERT INTO imported_records SELECT ?,kind,id,bill_id,revision,payload FROM imported_records WHERE account_id=?").bind(&target.account.id).bind(&source.account.id).execute(store.pool()).await.unwrap();
    let mut colliding = operation;
    colliding.expected_target_records = 2;
    assert!(matches!(
        store.consolidate_import_accounts("demo", colliding).await,
        Err(ImportError::Conflict)
    ));
}

#[tokio::test]
async fn origin_upgrade_preserves_legacy_receipts_including_inactive_owners() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let (source, target) = setup(&store, false).await;
    sqlx::query("UPDATE accounts SET import_only=0,binding=NULL,deployment=NULL WHERE id=?")
        .bind(&target.account.id)
        .execute(store.pool())
        .await
        .unwrap();
    let before = financial_snapshot(&store).await;
    // Reconstruct the pre-origin schema around real importer-created history.
    sqlx::raw_sql("CREATE TABLE legacy_receipts(account_id TEXT NOT NULL REFERENCES accounts(id),digest TEXT NOT NULL,PRIMARY KEY(account_id,digest)) WITHOUT ROWID;
        INSERT INTO legacy_receipts SELECT origin_id,digest FROM import_receipts;
        DROP TABLE import_receipts;
        DROP TABLE import_origins;
        DROP INDEX imported_evidence_account;
        ALTER TABLE legacy_receipts RENAME TO import_receipts;
        DELETE FROM _sqlx_migrations WHERE version=8;")
        .execute(store.pool()).await.unwrap();
    store.pool().close().await;
    let upgraded = Store::open(&path).await.unwrap();
    assert_eq!(financial_snapshot(&upgraded).await, before);
    assert!(upgraded.import("demo", source).await.unwrap().replayed);
    assert!(matches!(
        upgraded.import("demo", target).await,
        Err(ImportError::Binding)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_origins")
            .fetch_one(upgraded.pool())
            .await
            .unwrap(),
        2
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(upgraded.pool())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn actual_cli_and_http_consolidate_only_for_authenticated_owners() {
    let app = support::App::start().await;
    let (source, target) = setup(&app.store, false).await;
    let operation = operation(&source, &target);
    let response = reqwest::Client::new()
        .post(format!("{}/v1/imports/consolidate", app.url))
        .json(&operation)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let path = app.directory.path().join("consolidation.json");
    std::fs::write(&path, serde_json::to_vec(&operation).unwrap()).unwrap();
    let token = app.directory.path().join("token");
    std::fs::write(&token, support::TOKEN).unwrap();
    let url = app.url.clone();
    let result = tokio::task::spawn_blocking(move || {
        std::process::Command::new(env!("CARGO_BIN_EXE_ha-utilities"))
            .args(["--url", &url, "--token-file"])
            .arg(token)
            .args(["--json", "consolidate-imports"])
            .arg(path)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let receipt: ImportAccountConsolidationReceipt =
        serde_json::from_slice(&result.stdout).unwrap();
    assert!(!receipt.replayed);
    let client = Client::new(&app.url, Transport::Native, support::TOKEN).unwrap();
    assert!(
        client
            .consolidate_import_accounts(&operation)
            .await
            .unwrap()
            .replayed
    );
    assert!(client.import(&source).await.unwrap().replayed);
    assert!(client.import(&target).await.unwrap().replayed);
    app.close().await;
}
