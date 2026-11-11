mod support;

use ha_utilities::{
    client::{Client, Transport},
    imports::{ImportAccountPurge, ImportAccountPurgeReceipt, ImportError, ImportRequest, digest},
    models::{Bill, ListQuery, Page, ProviderId, Summary, SyncJob, SyncState},
    store::Store,
};
use serde_json::json;

async fn document(store: &Store, bytes: &[u8]) -> String {
    let key = digest(bytes);
    store.put_document("demo", &key, bytes).await.unwrap();
    key
}

fn request(records: serde_json::Value) -> ImportRequest {
    serde_json::from_value(json!({
        "account": {
            "id": "fictional-water", "name": "Fictional water", "provider": "lvmwd",
            "start_on": "2025-02-01",
            "services": [{"id": "fixture-meter", "name": "Meter", "kind": "water"}]
        },
        "currency": "USD",
        "records": records
    }))
    .unwrap()
}

#[tokio::test]
async fn concurrent_imports_share_one_record_and_corrections_keep_revision_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("imports.sqlite3"))
        .await
        .unwrap();
    let original = document(&store, b"Original statement").await;
    let corrected = document(&store, b"Corrected statement").await;
    let bill = |amount, source, replaces| {
        request(json!([{
            "kind": "bill", "reference": {"kind": "invoice_number", "value": "EXAMPLE-1"},
            "issued_on": "2025-02-20", "totals": {"new_charges_minor": amount},
            "sources": [{"sha256": source, "role": "invoice"}],
            "replaces_revision": replaces
        }]))
    };
    let initial = bill(1000, &original, None::<&str>);
    let (first, second) = tokio::join!(
        store.import("demo", initial.clone()),
        store.import("demo", initial.clone())
    );
    let receipts = [first.unwrap(), second.unwrap()];
    assert_eq!(receipts.iter().map(|r| r.inserted).sum::<usize>(), 1);
    assert_eq!(receipts.iter().filter(|r| r.replayed).count(), 1);
    let id: String = sqlx::query_scalar("SELECT id FROM observations WHERE kind='bill'")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let prior = store.bill_details("demo", &id).await.unwrap();
    assert!(matches!(
        store.import("demo", bill(1200, &corrected, None)).await,
        Err(ImportError::Conflict)
    ));
    let receipt = store
        .import("demo", bill(1200, &corrected, Some(&prior.bill.revision)))
        .await
        .unwrap();
    assert_eq!(receipt.corrected, 1);
    let detail = store.bill_details("demo", &id).await.unwrap();
    assert_eq!(
        detail
            .bill
            .record
            .sources()
            .iter()
            .map(|s| &s.sha256)
            .collect::<Vec<_>>(),
        [&corrected]
    );
    assert_ne!(detail.bill.revision, prior.bill.revision);
    assert_eq!(
        store.document("demo", &original).await.unwrap(),
        b"Original statement"
    );
    assert!(
        store
            .document("another-deployment", &original)
            .await
            .is_err()
    );
    assert!(store.import("demo", initial).await.unwrap().replayed);
    assert_eq!(
        store.bill_details("demo", &id).await.unwrap().bill.revision,
        detail.bill.revision
    );
}

#[tokio::test]
async fn replacing_measured_intervals_and_validating_charge_units_are_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("intervals.sqlite3"))
        .await
        .unwrap();
    let source = document(&store, b"Water export").await;
    let interval = |end, quality, replaces| {
        json!({
            "kind": "meter_interval", "service_id": "fixture-meter",
            "start": "2025-02-08T08:00:00Z", "end": end,
            "quantity": {"value": "1.5", "unit": "us_gallon"},
            "quality": quality,
            "sources": [{"sha256": source, "role": "usage_export"}],
            "replaces_revision": replaces
        })
    };
    let old_end = "2025-02-08T10:00:00Z";
    let new_end = "2025-02-08T09:00:00Z";
    store
        .import(
            "demo",
            request(json!([interval(old_end, "actual", None::<&str>)])),
        )
        .await
        .unwrap();
    let revision: String = sqlx::query_scalar("SELECT revision FROM imported_records")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let replacement = request(json!([
        interval(new_end, "actual", None),
        interval(old_end, "estimated", Some(&revision))
    ]));
    let receipt = store.import("demo", replacement).await.unwrap();
    assert_eq!((receipt.inserted, receipt.corrected), (1, 1));
    let ends: Vec<String> =
        sqlx::query_scalar("SELECT end_key FROM observations WHERE kind='usage'")
            .fetch_all(store.pool())
            .await
            .unwrap();
    assert_eq!(ends, ["2025-02-08T09:00:00+00:00"]);
    let invalid = request(json!([{
        "kind": "bill", "reference": {"kind": "invoice_number", "value": "EXAMPLE-2"},
        "issued_on": "2025-02-20", "totals": {"new_charges_minor": 100},
        "charges": [{"kind": "usage", "service_id": "fixture-meter", "label": "Invalid",
            "amount_minor": 100, "quantity": {"value": "-1", "unit": "gigabyte"},
            "rate": {"amount": "1", "per": "gigabyte"}}],
        "sources": [{"sha256": source, "role": "supporting"}]
    }]));
    assert!(matches!(
        store.import("demo", invalid).await,
        Err(ImportError::Invalid)
    ));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM imported_records")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn imported_accounts_remain_outside_provider_sync_and_freshness() {
    let app = support::App::start().await;
    for account in &app.config.accounts {
        let job = app.store.start_job(&account.id).await.unwrap();
        app.store
            .finish_job(&job.id, SyncState::Complete, 0, None)
            .await
            .unwrap();
    }
    let source = document(&app.store, b"Imported statement").await;
    app.store
        .import(
            &app.config.deployment,
            request(json!([{
                "kind": "bill",
                "reference": {"kind": "invoice_number", "value": "EXAMPLE-3"},
                "issued_on": "2025-02-20",
                "totals": {"new_charges_minor": 1000},
                "sources": [{"sha256": source, "role": "invoice"}]
            }])),
        )
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let summary: Summary = client
        .get(format!("{}/v1/summary", app.url))
        .bearer_auth(support::TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary.import_only_accounts, 1);
    assert!(summary.last_imported_at.is_some());
    assert!(!summary.manual_accounts_stale);
    assert!(summary.stale_automatic_accounts.is_empty());

    let sync = format!("{}/v1/sync", app.url);
    for request in [
        json!({"account": "fictional-water", "range": "all"}),
        json!({"account": "fictional-water", "from": "2025-02-01", "to": "2025-02-28"}),
    ] {
        let response = client
            .post(&sync)
            .bearer_auth(support::TOKEN)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    }
    let jobs: Vec<SyncJob> = client
        .get(&sync)
        .bearer_auth(support::TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(jobs.len(), app.config.accounts.len());
    assert!(jobs.iter().all(|job| job.account_id != "fictional-water"));
    app.close().await;
}

#[tokio::test]
async fn owner_can_purge_only_the_confirmed_import_account() {
    use reqwest::StatusCode;

    let app = support::App::start().await;
    let deployment = &app.config.deployment;
    let private = document(&app.store, b"Statement for removed account").await;
    let shared = document(&app.store, b"Statement evidence shared across accounts").await;
    let bill = |source: &str, supporting: &str| {
        request(json!([{
            "kind": "bill",
            "reference": {"kind": "invoice_number", "value": "EXAMPLE-4"},
            "issued_on": "2025-02-20",
            "totals": {"new_charges_minor": 1000},
            "sources": [{"sha256": source, "role": "invoice"},
                        {"sha256": supporting, "role": "supporting"}]
        }]))
    };
    let removed = bill(&private, &shared);
    let mut retained = bill(&shared, &shared);
    retained.account.id = "retained-water".to_owned();
    retained.account.services[0].id = "retained-meter".to_owned();
    app.store.import(deployment, retained).await.unwrap();
    app.store.import(deployment, removed.clone()).await.unwrap();

    let native = Client::new(&app.url, Transport::Native, support::TOKEN).unwrap();
    let mut group = ListQuery {
        import_provider: Some(ProviderId::Lvmwd),
        limit: Some(1),
        ..ListQuery::default()
    };
    let first = native.bills(&group).await.unwrap();
    group.page = first.next_page;
    assert!(group.page.is_some());
    let second = native.bills(&group).await.unwrap();
    assert!(second.next_page.is_none());
    let mut account_ids = [
        first.items[0].account_id.as_str(),
        second.items[0].account_id.as_str(),
    ];
    account_ids.sort_unstable();
    assert_eq!(account_ids, ["fictional-water", "retained-water"]);
    group.import_provider = None;
    assert_eq!(native.bills(&group).await.unwrap_err().status, Some(400));

    let client = reqwest::Client::new();
    let purge_url = |id: &str| format!("{}/v1/imports/accounts/{id}/purge", app.url);
    let purge = |id: &str, provider, expected_records| {
        client.post(purge_url(id)).json(&ImportAccountPurge {
            provider,
            expected_records,
        })
    };
    let request = || purge("fictional-water", ProviderId::Lvmwd, 1);
    assert_eq!(
        request().send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    for conflict in [
        purge("fictional-water", ProviderId::Lvmwd, 2),
        purge("fictional-water", ProviderId::Starlink, 1),
        purge("demo-wm", ProviderId::WasteManagement, 1),
    ] {
        assert_eq!(
            conflict
                .bearer_auth(support::TOKEN)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
    }
    let execute = || async {
        request()
            .bearer_auth(support::TOKEN)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<ImportAccountPurgeReceipt>()
            .await
            .unwrap()
    };
    assert_eq!(
        execute().await,
        ImportAccountPurgeReceipt {
            removed: true,
            records: 1,
            documents: 1
        }
    );
    assert_eq!(execute().await, ImportAccountPurgeReceipt::default());
    let bills = client
        .get(format!("{}/v1/bills?limit=100", app.url))
        .bearer_auth(support::TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json::<Page<Bill>>()
        .await
        .unwrap();
    assert!(
        bills
            .items
            .iter()
            .all(|bill| bill.account_id != "fictional-water")
    );
    assert!(
        bills
            .items
            .iter()
            .any(|bill| bill.account_id == "retained-water")
    );
    assert!(bills.items.iter().any(|bill| bill.account_id == "demo-wm"));
    for (source, status) in [(&private, StatusCode::NOT_FOUND), (&shared, StatusCode::OK)] {
        assert_eq!(
            client
                .get(format!("{}/v1/documents/{source}", app.url))
                .bearer_auth(support::TOKEN)
                .send()
                .await
                .unwrap()
                .status(),
            status
        );
    }
    let retained_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM imported_revisions")
        .fetch_one(app.store.pool())
        .await
        .unwrap();
    assert_eq!(retained_rows, 1);
    let blobs: i64 = sqlx::query_scalar("SELECT count(*) FROM import_documents")
        .fetch_one(app.store.pool())
        .await
        .unwrap();
    assert_eq!(blobs, 1);
    app.store
        .put_document(deployment, &private, b"Statement for removed account")
        .await
        .unwrap();
    assert_eq!(
        app.store
            .import(deployment, removed)
            .await
            .unwrap()
            .inserted,
        1
    );
    app.close().await;
}
