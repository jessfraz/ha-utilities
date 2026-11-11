use ha_utilities::{
    models::{
        ConnectionAccount, ConnectionMeter, ConnectionProgress, ConnectionState, ServiceKind,
    },
    store::Store,
};
use serde::Serialize;
use sqlx::{SqlitePool, migrate::Migrator, sqlite::SqliteConnectOptions};
use std::path::Path;

#[derive(Serialize)]
struct LegacyService {
    id: String,
    name: String,
    provider_service: String,
    kind: String,
}

#[derive(Serialize)]
struct LegacyBinding {
    id: String,
    name: String,
    deployment: String,
    provider: String,
    customer_id: String,
    provider_account: String,
    currency: String,
    services: Vec<LegacyService>,
}

async fn legacy_database(path: &Path) -> SqlitePool {
    let directory = tempfile::tempdir().unwrap();
    for (name, content) in [
        (
            "0001_read_model.sql",
            include_str!("../migrations/0001_read_model.sql"),
        ),
        (
            "0002_connections.sql",
            include_str!("../migrations/0002_connections.sql"),
        ),
        (
            "0004_sync_job_order.sql",
            include_str!("../migrations/0004_sync_job_order.sql"),
        ),
        (
            "0005_imports.sql",
            include_str!("../migrations/0005_imports.sql"),
        ),
        (
            "0006_automatic_sync.sql",
            include_str!("../migrations/0006_automatic_sync.sql"),
        ),
    ] {
        std::fs::write(directory.path().join(name), content).unwrap();
    }
    let pool = SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true),
    )
    .await
    .unwrap();
    Migrator::new(directory.path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    let progress = ConnectionProgress {
        state: ConnectionState::Connected,
        accounts: vec![ConnectionAccount {
            id: "fictional-remote-account".to_owned(),
            meters: vec![ConnectionMeter {
                id: "fictional-remote-meter".to_owned(),
                kind: ServiceKind::Electricity,
            }],
        }],
        linked_account_ids: vec!["legacy-account".to_owned()],
        ..ConnectionProgress::default()
    };
    sqlx::query("INSERT INTO utility_connections(id,deployment,request_id,external_id,utility_id,utility_name,name,currency,progress) VALUES('legacy-connection','fixture','fictional-request','fictional-external','fixture_energy','Fixture Energy','Redundant custom title','USD',?)")
        .bind(serde_json::to_string(&progress).unwrap()).execute(&pool).await.unwrap();
    for id in ["legacy-account", "hidden-account"] {
        let binding = LegacyBinding {
            id: id.to_owned(),
            name: "Another old custom title".to_owned(),
            deployment: "fixture".to_owned(),
            provider: "bayou".to_owned(),
            customer_id: "fictional-customer".to_owned(),
            provider_account: id.to_owned(),
            currency: "USD".to_owned(),
            services: vec![LegacyService {
                id: format!("{id}-meter"),
                name: "Freeform old meter title".to_owned(),
                provider_service: format!("fictional-{id}"),
                kind: "electricity".to_owned(),
            }],
        };
        sqlx::query("INSERT INTO accounts(id,binding_hash,deployment,binding,connection_id) VALUES(?,'immutable-hash',?,?, 'legacy-connection')")
            .bind(id).bind((id == "legacy-account").then_some("fixture"))
            .bind(serde_json::to_string(&binding).unwrap()).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO observations(account_id,kind,id,service_id,sort_key,observed_at,payload) VALUES('legacy-account','service','legacy-account-meter','legacy-account-meter','1970-01-01T00:00:00Z','2025-01-01T00:00:00Z',?)")
        .bind(r#"{"id":"legacy-account-meter","name":"Old observation label","kind":"electricity"}"#)
        .execute(&pool).await.unwrap();
    pool
}

async fn connection_identity(
    pool: &SqlitePool,
) -> (String, String, String, String, String, String) {
    sqlx::query_as(
        "SELECT id,utility_id,utility_name,progress,created_at,updated_at FROM utility_connections",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn account_identity(
    pool: &SqlitePool,
) -> Vec<(String, String, Option<String>, Option<String>)> {
    sqlx::query_as("SELECT id,binding_hash,deployment,connection_id FROM accounts ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn upgrade_removes_labels_and_preserves_canonical_identity_including_hidden_rows() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite3");
    let old = legacy_database(&path).await;
    let connection = connection_identity(&old).await;
    let accounts = account_identity(&old).await;
    old.close().await;
    let store = Store::open(&path).await.unwrap();
    let name_columns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('utility_connections') WHERE name='name'",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(
        name_columns, 0,
        "the obsolete connection label column survived the upgrade"
    );
    assert_eq!(connection_identity(store.pool()).await, connection);
    assert_eq!(account_identity(store.pool()).await, accounts);
    let labels: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM accounts WHERE json_type(binding,'$.name') IS NOT NULL OR EXISTS(SELECT 1 FROM json_each(binding,'$.services') AS service WHERE json_type(service.value,'$.name') IS NOT NULL)")
        .fetch_one(store.pool()).await.unwrap();
    assert_eq!(labels, 0);
    let service: (String, String) = sqlx::query_as("SELECT id,json_extract(payload,'$.kind') FROM observations WHERE kind='service' AND json_type(payload,'$.name') IS NULL")
        .fetch_one(store.pool()).await.unwrap();
    assert_eq!(
        service,
        ("legacy-account-meter".to_owned(), "electricity".to_owned())
    );
    let binding = store.binding("fixture", "legacy-account").await.unwrap();
    assert_eq!(binding.services[0].kind, ServiceKind::Electricity);
    let violations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pragma_foreign_key_check")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(violations, 0);
    store.pool().close().await;
}

#[tokio::test]
async fn upgrade_rejects_unclassified_legacy_services_atomically() {
    for statement in [
        "UPDATE accounts SET binding=json_set(binding,'$.services[0].kind','other') WHERE id='hidden-account'",
        "UPDATE observations SET payload=json_set(payload,'$.kind','other') WHERE kind='service'",
        "UPDATE utility_connections SET progress=json_set(progress,'$.accounts[0].meters[0].kind','other')",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unclassified.sqlite3");
        let old = legacy_database(&path).await;
        sqlx::query(statement).execute(&old).await.unwrap();
        let connection = connection_identity(&old).await;
        let accounts = account_identity(&old).await;
        assert!(
            Store::open(&path).await.is_err(),
            "an unclassified service was silently accepted"
        );
        assert_eq!(connection_identity(&old).await, connection);
        assert_eq!(account_identity(&old).await, accounts);
        let legacy_column: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('utility_connections') WHERE name='name'",
        )
        .fetch_one(&old)
        .await
        .unwrap();
        assert_eq!(legacy_column, 1);
        old.close().await;
    }
}

#[tokio::test]
async fn migration_plans_and_runtime_cover_representative_legacy_rows() {
    use sqlx::Row;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("migration-plans.sqlite3");
    let old = legacy_database(&path).await;
    let mut data = old.begin().await.unwrap();
    for index in 0..1000 {
        let id = format!("plan-account-{index}");
        let meter = format!("plan-meter-{index}");
        sqlx::query("INSERT INTO accounts(id,binding_hash,deployment,binding,connection_id) SELECT ?1,binding_hash,'fixture',json_set(binding,'$.id',?1,'$.provider_account',?1,'$.services[0].id',?2,'$.services[0].provider_service',?2),connection_id FROM accounts WHERE id='legacy-account'")
            .bind(&id).bind(&meter).execute(&mut *data).await.unwrap();
        sqlx::query("INSERT INTO observations(account_id,kind,id,service_id,sort_key,observed_at,payload) SELECT ?1,kind,?2,?2,sort_key,observed_at,json_set(payload,'$.id',?2) FROM observations WHERE id='legacy-account-meter'")
            .bind(&id).bind(&meter).execute(&mut *data).await.unwrap();
    }
    sqlx::query("WITH RECURSIVE number(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM number WHERE n<10000) INSERT INTO observations(account_id,kind,id,sort_key,observed_at,payload) SELECT 'legacy-account','bill','plan-bill-'||n,'2025-01-01','2025-01-01T00:00:00Z','{}' FROM number")
        .execute(&mut *data).await.unwrap();
    data.commit().await.unwrap();
    sqlx::query("ANALYZE").execute(&old).await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observations")
        .fetch_one(&old)
        .await
        .unwrap();
    let mut transaction = old.begin().await.unwrap();
    for statement in include_str!("../migrations/0007_typed_utility_identity.sql")
        .split(';')
        .map(str::trim)
        .filter(|sql| !sql.is_empty())
    {
        let query = format!("EXPLAIN QUERY PLAN {statement}");
        let plans = sqlx::query(sqlx::AssertSqlSafe(query))
            .fetch_all(&mut *transaction)
            .await
            .unwrap();
        let detail = plans
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect::<Vec<_>>();
        println!("{statement}\nPLAN: {}", detail.join("; "));
        if statement.starts_with("UPDATE observations") {
            assert!(
                detail
                    .iter()
                    .any(|plan| plan.contains("observations_kind_date"))
            );
            assert!(!detail.iter().any(|plan| plan == "SCAN observations"));
        }
        // Execute the same migration in a disposable transaction so later statements
        // see its temporary validation table and altered schema while being planned.
        sqlx::raw_sql(statement)
            .execute(&mut *transaction)
            .await
            .unwrap();
    }
    transaction.rollback().await.unwrap();
    old.close().await;
    let started = std::time::Instant::now();
    let upgraded = Store::open(&path).await.unwrap();
    println!(
        "Real migration of 1002 accounts, 1001 service rows, 10000 financial rows: {:?}",
        started.elapsed()
    );
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM observations")
        .fetch_one(upgraded.pool())
        .await
        .unwrap();
    assert_eq!(after, before);
    let labels: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM accounts WHERE json_type(binding,'$.name') IS NOT NULL",
    )
    .fetch_one(upgraded.pool())
    .await
    .unwrap();
    assert_eq!(labels, 0);
    upgraded.pool().close().await;
}
