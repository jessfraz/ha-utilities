mod support;
use chrono::{Duration, Utc};
use ha_utilities::{
    models::*,
    store::{Store, StoreError},
    utility::SyncPage,
};
use rust_decimal::Decimal;

#[tokio::test]
async fn binding_reconciliation_overlap_and_atomicity_are_enforced() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("state.sqlite3"))
        .await
        .unwrap();
    let config = support::config();
    store.register(&config).await.unwrap();
    let mut changed = support::config();
    changed.accounts[0].provider_account = "DIFFERENT".to_owned();
    assert!(store.register(&changed).await.is_err());
    let binding = &config.accounts[1];
    let now = Utc::now();
    let start = (now - Duration::hours(3))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    let usage = Usage {
        id: "demo-energy:usage:a".to_owned(),
        account_id: binding.id.clone(),
        service_id: "demo-electric".to_owned(),
        start,
        end: start + Duration::hours(1),
        quantity: Decimal::from(100),
        unit: UsageUnit::Wh,
        observed_at: now,
    };
    let page = SyncPage {
        observations: vec![Observation::Usage(usage.clone())],
        coverage: vec![],
        continuation: None,
    };
    store.ingest(binding, &page).await.unwrap();
    store.ingest(binding, &page).await.unwrap();
    let mut older = usage.clone();
    older.quantity = Decimal::from(800);
    older.observed_at = now - Duration::hours(1);
    store
        .ingest(
            binding,
            &SyncPage {
                observations: vec![Observation::Usage(older)],
                coverage: vec![],
                continuation: None,
            },
        )
        .await
        .unwrap();
    let list = store
        .list::<Usage>(
            "usage",
            &ListQuery::default(),
            std::slice::from_ref(&binding.id),
        )
        .await
        .unwrap();
    assert_eq!(list.items, vec![usage.clone()]);
    let mut valid = usage.clone();
    valid.id = "demo-energy:usage:b".to_owned();
    valid.start = usage.end;
    valid.end += Duration::hours(1);
    let mut overlap = usage.clone();
    overlap.id = "demo-energy:usage:c".to_owned();
    overlap.start += Duration::minutes(30);
    overlap.end += Duration::minutes(30);
    assert!(matches!(
        store
            .ingest(
                binding,
                &SyncPage {
                    observations: vec![Observation::Usage(valid), Observation::Usage(overlap)],
                    coverage: vec![],
                    continuation: None
                }
            )
            .await,
        Err(StoreError::InvalidObservation)
    ));
    assert_eq!(
        store
            .list::<Usage>(
                "usage",
                &ListQuery::default(),
                std::slice::from_ref(&binding.id)
            )
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    let mut foreign = usage;
    foreign.account_id = config.accounts[0].id.clone();
    assert!(
        store
            .ingest(
                binding,
                &SyncPage {
                    observations: vec![Observation::Usage(foreign)],
                    coverage: vec![],
                    continuation: None
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_sync_admission_and_restart_are_durable() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(&temp.path().join("state.sqlite3"))
        .await
        .unwrap();
    store.register(&support::config()).await.unwrap();
    let (a, b) = tokio::join!(store.start_job("demo-wm"), store.start_job("demo-wm"));
    assert_ne!(a.is_ok(), b.is_ok());
    let job = a.or(b).unwrap();
    store.recover_interrupted().await.unwrap();
    let persisted = store.job(&job.id).await.unwrap();
    assert_eq!(persisted.state, SyncState::Interrupted);
    assert!(persisted.finished_at.is_some());
    assert!(store.start_job("demo-wm").await.is_ok());
}

#[tokio::test]
async fn usage_ingestion_can_run_while_other_syncs_are_admitted() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("race.sqlite3"))
        .await
        .unwrap();
    let config = support::config();
    store.register(&config).await.unwrap();
    let now = Utc::now();
    let binding = &config.accounts[1];
    let mut errors = Vec::new();
    for batch in 0..10 {
        let observations = (0..50)
            .map(|offset| {
                let index = batch * 50 + offset;
                let start = now + Duration::minutes(index * 15);
                Observation::Usage(Usage {
                    id: format!("demo-energy:usage:{index}"),
                    account_id: binding.id.clone(),
                    service_id: "demo-electric".to_owned(),
                    start,
                    end: start + Duration::minutes(15),
                    quantity: Decimal::from(100),
                    unit: UsageUnit::Wh,
                    observed_at: now,
                })
            })
            .collect();
        let page = SyncPage {
            observations,
            coverage: vec![],
            continuation: None,
        };
        let (ingest, admit) = tokio::join!(store.ingest(binding, &page), async {
            for _ in 0..20 {
                let job = store.start_job("demo-wm").await?;
                store
                    .finish_job(&job.id, SyncState::Complete, 0, None)
                    .await?;
            }
            Ok::<(), StoreError>(())
        });
        if let Err(error) = ingest {
            errors.push(error.to_string());
        }
        admit.unwrap();
    }
    assert!(
        errors.is_empty(),
        "valid pages must survive concurrent admission: {errors:?}"
    );
    assert_eq!(
        store
            .summary(std::slice::from_ref(&binding.id))
            .await
            .unwrap()
            .usage_intervals,
        500
    );
}

#[tokio::test]
async fn bills_start_with_the_latest_and_page_without_losing_records() {
    let directory = tempfile::tempdir().unwrap();
    let store = support::populated(&directory.path().join("bills.sqlite3")).await;
    let accounts = vec!["demo-wm".to_owned()];
    let mut query = ListQuery {
        limit: Some(2),
        ..Default::default()
    };
    let mut ids = Vec::new();
    loop {
        let page: Page<Bill> = store.list("bill", &query, &accounts).await.unwrap();
        ids.extend(page.items.into_iter().map(|bill| bill.id));
        query.page = page.next_page;
        if query.page.is_none() {
            break;
        }
    }
    assert_eq!(
        ids,
        vec![
            "demo-wm:bill:0",
            "demo-wm:bill:1",
            "demo-wm:bill:2",
            "demo-wm:bill:3",
            "demo-wm:bill:4"
        ]
    );
}
