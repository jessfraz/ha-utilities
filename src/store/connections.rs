//! Connection intents and confirmed bindings survive process and browser restarts.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, Transaction};
use uuid::Uuid;

use crate::{
    config::{AccountBinding, Config},
    models::{
        Connection, ConnectionProgress, ConnectionProvider, ConnectionState, Coverage,
        CreateConnection,
    },
    store::{Store, StoreError},
};

pub(crate) struct ConnectionRecord {
    pub connection: Connection,
    pub external_id: String,
    pub revision: i64,
}

#[derive(sqlx::FromRow)]
struct ConnectionRow {
    id: String,
    request_id: String,
    external_id: String,
    utility_id: String,
    utility_name: String,
    name: String,
    currency: String,
    progress: String,
    revision: i64,
    provider_settings_accepted_at: String,
    created_at: String,
    updated_at: String,
    checked_at: Option<String>,
}

impl ConnectionRow {
    fn decode(self) -> Result<ConnectionRecord, StoreError> {
        fn date(value: &str) -> Result<DateTime<Utc>, StoreError> {
            DateTime::parse_from_rfc3339(value)
                .map(|value| value.with_timezone(&Utc))
                .map_err(|_| StoreError::InvalidObservation)
        }
        Ok(ConnectionRecord {
            external_id: self.external_id,
            revision: self.revision,
            connection: Connection {
                id: self.id,
                provider: ConnectionProvider::Bayou,
                request_id: self.request_id,
                utility_id: self.utility_id,
                utility_name: self.utility_name,
                name: self.name,
                currency: self.currency,
                progress: serde_json::from_str(&self.progress)?,
                provider_settings_accepted_at: date(&self.provider_settings_accepted_at)?,
                created_at: date(&self.created_at)?,
                updated_at: date(&self.updated_at)?,
                checked_at: self.checked_at.as_deref().map(date).transpose()?,
            },
        })
    }
}

impl Store {
    pub async fn bindings(&self, deployment: &str) -> Result<Vec<AccountBinding>, StoreError> {
        let values: Vec<String> = sqlx::query_scalar(queries::BINDINGS)
            .bind(deployment)
            .fetch_all(&self.pool)
            .await?;
        if values.len() > 100 {
            return Err(StoreError::InvalidQuery);
        }
        values
            .into_iter()
            .map(|value| serde_json::from_str(&value).map_err(Into::into))
            .collect()
    }

    pub async fn binding(&self, deployment: &str, id: &str) -> Result<AccountBinding, StoreError> {
        let value: String = sqlx::query_scalar(queries::BINDING)
            .bind(deployment)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        serde_json::from_str(&value).map_err(Into::into)
    }

    pub(crate) async fn register_binding(
        tx: &mut Transaction<'_, Sqlite>,
        binding: &AccountBinding,
        connection: Option<&str>,
    ) -> Result<(), StoreError> {
        // This is the original immutable identity. Labels may still change.
        let identity = (
            &binding.deployment,
            &binding.provider,
            &binding.customer_id,
            &binding.provider_account,
            &binding.currency,
            binding
                .services
                .iter()
                .map(|service| (&service.id, &service.provider_service, service.kind))
                .collect::<Vec<_>>(),
        );
        let hash = URL_SAFE_NO_PAD.encode(Sha256::digest(serde_json::to_vec(&identity)?));
        sqlx::query(crate::store::queries::REGISTER)
            .bind(&binding.id)
            .bind(&hash)
            .execute(&mut **tx)
            .await?;
        let stored: String = sqlx::query_scalar(crate::store::queries::BINDING)
            .bind(&binding.id)
            .fetch_one(&mut **tx)
            .await?;
        if stored != hash {
            return Err(StoreError::BindingChanged);
        }
        sqlx::query(queries::SAVE_BINDING)
            .bind(&binding.deployment)
            .bind(serde_json::to_string(binding)?)
            .bind(connection)
            .bind(&binding.id)
            .execute(&mut **tx)
            .await
            .map_err(|error| {
                if error
                    .as_database_error()
                    .is_some_and(|error| error.is_unique_violation())
                {
                    StoreError::BindingChanged
                } else {
                    StoreError::Database(error)
                }
            })?;
        Ok(())
    }

    pub(crate) async fn create_connection(
        &self,
        deployment: &str,
        input: &CreateConnection,
        utility_name: &str,
    ) -> Result<(ConnectionRecord, bool), StoreError> {
        if !input.accept_provider_settings {
            return Err(StoreError::InvalidQuery);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(existing) = sqlx::query_as::<_, ConnectionRow>(queries::BY_REQUEST)
            .bind(deployment)
            .bind(&input.request_id)
            .fetch_optional(&mut *tx)
            .await?
        {
            let existing = existing.decode()?;
            let expected_name = input
                .name
                .as_deref()
                .unwrap_or(&existing.connection.utility_name);
            if existing.connection.utility_id != input.utility_id
                || existing.connection.currency != input.currency
                || existing.connection.name != expected_name
            {
                return Err(StoreError::BindingChanged);
            }
            tx.commit().await?;
            return Ok((existing, false));
        }
        let count: i64 = sqlx::query_scalar(queries::COUNT)
            .bind(deployment)
            .fetch_one(&mut *tx)
            .await?;
        if count >= 100 {
            return Err(StoreError::InvalidQuery);
        }
        let id = Uuid::now_v7().to_string();
        // Send only a random opaque identifier to Bayou, never a deployment label.
        let external_id = format!("ha_utilities_{}", Uuid::now_v7().simple());
        sqlx::query(queries::CREATE)
            .bind(&id)
            .bind(deployment)
            .bind(&input.request_id)
            .bind(&external_id)
            .bind(&input.utility_id)
            .bind(utility_name)
            .bind(input.name.as_deref().unwrap_or(utility_name))
            .bind(&input.currency)
            .bind(serde_json::to_string(&ConnectionProgress::default())?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok((self.connection(deployment, &id).await?, true))
    }

    pub(crate) async fn connection_request(
        &self,
        deployment: &str,
        request: &str,
    ) -> Result<Option<ConnectionRecord>, StoreError> {
        sqlx::query_as::<_, ConnectionRow>(queries::BY_REQUEST)
            .bind(deployment)
            .bind(request)
            .fetch_optional(&self.pool)
            .await?
            .map(ConnectionRow::decode)
            .transpose()
    }

    pub(crate) async fn queued_connection(
        &self,
        deployment: &str,
    ) -> Result<Option<ConnectionRecord>, StoreError> {
        sqlx::query_as::<_, ConnectionRow>(queries::QUEUED)
            .bind(deployment)
            .fetch_optional(&self.pool)
            .await?
            .map(ConnectionRow::decode)
            .transpose()
    }

    pub(crate) async fn connection(
        &self,
        deployment: &str,
        id: &str,
    ) -> Result<ConnectionRecord, StoreError> {
        sqlx::query_as::<_, ConnectionRow>(queries::GET)
            .bind(deployment)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?
            .decode()
    }

    pub async fn connections(&self, deployment: &str) -> Result<Vec<Connection>, StoreError> {
        sqlx::query_as::<_, ConnectionRow>(queries::LIST)
            .bind(deployment)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| row.decode().map(|record| record.connection))
            .collect()
    }

    pub(crate) async fn save_connection_progress(
        &self,
        deployment: &str,
        record: &ConnectionRecord,
        progress: &ConnectionProgress,
        checked: bool,
    ) -> Result<bool, StoreError> {
        Ok(sqlx::query(queries::SAVE_PROGRESS)
            .bind(serde_json::to_string(progress)?)
            .bind(checked)
            .bind(deployment)
            .bind(&record.connection.id)
            .bind(record.revision)
            .execute(&self.pool)
            .await?
            .rows_affected()
            == 1)
    }

    pub async fn recover_connections(&self, deployment: &str) -> Result<(), StoreError> {
        sqlx::query(queries::RECOVER)
            .bind(deployment)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub(crate) async fn bind_connection(
        &self,
        config: &Config,
        record: &ConnectionRecord,
        bindings: &[AccountBinding],
        coverage: &[Coverage],
    ) -> Result<Connection, StoreError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = sqlx::query_as::<_, ConnectionRow>(queries::GET)
            .bind(&config.deployment)
            .bind(&record.connection.id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(StoreError::NotFound)?
            .decode()?;
        if current.revision != record.revision {
            return Err(StoreError::Busy);
        }
        let existing: Vec<String> = sqlx::query_scalar(queries::BINDINGS)
            .bind(&config.deployment)
            .fetch_all(&mut *tx)
            .await?;
        let mut candidate = config.clone();
        candidate.accounts = existing
            .iter()
            .map(|value| serde_json::from_str(value))
            .collect::<Result<Vec<_>, _>>()?;
        for binding in bindings {
            let new = !candidate
                .accounts
                .iter()
                .any(|account| account.id == binding.id);
            if new {
                candidate.accounts.push(binding.clone());
            }
            Self::register_binding(&mut tx, binding, Some(&record.connection.id)).await?;
            if new {
                for item in coverage.iter().filter(|item| item.account_id == binding.id) {
                    Self::write_coverage(&mut tx, item).await?;
                }
            }
        }
        candidate.validate().map_err(|_| StoreError::InvalidQuery)?;
        let mut progress = record.connection.progress.clone();
        progress.state = ConnectionState::Connected;
        progress.issue = None;
        for binding in bindings {
            if !progress.linked_account_ids.contains(&binding.id) {
                progress.linked_account_ids.push(binding.id.clone());
            }
        }
        sqlx::query(queries::SAVE_PROGRESS)
            .bind(serde_json::to_string(&progress)?)
            .bind(true)
            .bind(&config.deployment)
            .bind(&record.connection.id)
            .bind(record.revision)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(self
            .connection(&config.deployment, &record.connection.id)
            .await?
            .connection)
    }
}

pub(crate) mod queries {
    pub const CLEAR_MANUAL: &str = "UPDATE accounts SET deployment=NULL,binding=NULL WHERE deployment=? AND connection_id IS NULL";
    pub const BINDINGS: &str = "SELECT binding FROM accounts WHERE deployment=? AND binding IS NOT NULL ORDER BY id LIMIT 101";
    pub const BINDING: &str =
        "SELECT binding FROM accounts WHERE deployment=? AND id=? AND binding IS NOT NULL";
    pub const SAVE_BINDING: &str = "UPDATE accounts SET deployment=?, binding=?, connection_id=COALESCE(connection_id,?) WHERE id=?";
    pub const GET: &str = "SELECT id,request_id,external_id,utility_id,utility_name,name,currency,progress,revision,provider_settings_accepted_at,created_at,updated_at,checked_at FROM utility_connections WHERE deployment=? AND id=?";
    pub const BY_REQUEST: &str = "SELECT id,request_id,external_id,utility_id,utility_name,name,currency,progress,revision,provider_settings_accepted_at,created_at,updated_at,checked_at FROM utility_connections WHERE deployment=? AND request_id=?";
    pub const LIST: &str = "SELECT id,request_id,external_id,utility_id,utility_name,name,currency,progress,revision,provider_settings_accepted_at,created_at,updated_at,checked_at FROM utility_connections WHERE deployment=? ORDER BY created_at DESC,id DESC LIMIT 100";
    pub const QUEUED: &str = "SELECT id,request_id,external_id,utility_id,utility_name,name,currency,progress,revision,provider_settings_accepted_at,created_at,updated_at,checked_at FROM utility_connections WHERE deployment=? AND json_extract(progress,'$.state')='queued' ORDER BY created_at,id LIMIT 1";
    pub const COUNT: &str = "SELECT COUNT(*) FROM utility_connections WHERE deployment=?";
    pub const CREATE: &str = "INSERT INTO utility_connections(id,deployment,request_id,external_id,utility_id,utility_name,name,currency,progress) VALUES(?,?,?,?,?,?,?,?,?)";
    pub const SAVE_PROGRESS: &str = "UPDATE utility_connections SET progress=?, revision=revision+1, updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'), checked_at=CASE WHEN ? THEN strftime('%Y-%m-%dT%H:%M:%fZ','now') ELSE checked_at END WHERE deployment=? AND id=? AND revision=?";
    pub const RECOVER: &str = "UPDATE utility_connections SET progress=json_set(progress,'$.state','unconfirmed','$.issue','process_interrupted'),revision=revision+1,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE deployment=? AND json_extract(progress,'$.state')='creating'";
}

#[cfg(test)]
mod tests {
    use crate::{
        config::Config,
        models::*,
        store::{Store, StoreError, connections::queries},
    };
    use sqlx::{Execute, Row, Sqlite, sqlite::SqliteArguments};
    use uuid::Uuid;

    fn input() -> CreateConnection {
        CreateConnection {
            provider: ConnectionProvider::Bayou,
            request_id: Uuid::now_v7().to_string(),
            utility_id: "speculoos_power".into(),
            name: None,
            currency: "USD".into(),
            accept_provider_settings: true,
        }
    }

    #[tokio::test]
    async fn durable_intent_is_scoped_idempotent_and_rejects_stale_updates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connections.sqlite3");
        let store = Store::open(&path).await.unwrap();
        let input = input();
        let (a, b) = tokio::join!(
            store.create_connection("demo", &input, "Speculoos Power"),
            store.create_connection("demo", &input, "Speculoos Power")
        );
        let (a, created_a) = a.unwrap();
        let (b, created_b) = b.unwrap();
        assert_ne!(created_a, created_b);
        assert_eq!(a.connection.id, b.connection.id);
        assert_eq!(a.external_id, b.external_id);
        assert!(!a.external_id.contains("demo"));
        assert!(matches!(
            store.connection("other", &a.connection.id).await,
            Err(StoreError::NotFound)
        ));
        let mut changed = input.clone();
        changed.currency = "CAD".into();
        assert!(matches!(
            store
                .create_connection("demo", &changed, "Speculoos Power")
                .await,
            Err(StoreError::BindingChanged)
        ));
        let mut progress = a.connection.progress.clone();
        progress.state = ConnectionState::Creating;
        assert!(
            store
                .save_connection_progress("demo", &a, &progress, false)
                .await
                .unwrap()
        );
        assert!(
            !store
                .save_connection_progress("demo", &b, &ConnectionProgress::default(), true)
                .await
                .unwrap()
        );
        store.pool().close().await;
        let store = Store::open(&path).await.unwrap();
        store.recover_connections("demo").await.unwrap();
        let recovered = store.connection("demo", &a.connection.id).await.unwrap();
        assert_eq!(
            recovered.connection.progress.state,
            ConnectionState::Unconfirmed
        );
        assert_eq!(recovered.external_id, a.external_id);
        assert_eq!(
            recovered.connection.progress.issue,
            Some(ConnectionIssue::ProcessInterrupted)
        );
        assert!(store.queued_connection("demo").await.unwrap().is_none());
        assert!(recovered.connection.checked_at.is_none());
        store.pool().close().await;
    }

    #[tokio::test]
    async fn binding_transaction_replays_without_erasing_data_and_survives_config_reload() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("bindings.sqlite3"))
            .await
            .unwrap();
        let mut config: Config = toml::from_str(include_str!("../../config/example.toml")).unwrap();
        store.register(&config).await.unwrap();
        let manual = config.accounts[0].id.clone();
        let (record, _) = store
            .create_connection(&config.deployment, &input(), "Speculoos Power")
            .await
            .unwrap();
        let mut binding = config.accounts[1].clone();
        binding.id = "hosted-account".into();
        binding.customer_id = "71888".into();
        binding.provider_account = "fictional-hosted-account".into();
        binding.services[0].id = "hosted-meter".into();
        let bound = store
            .bind_connection(&config, &record, std::slice::from_ref(&binding), &[])
            .await
            .unwrap();
        assert_eq!(bound.progress.linked_account_ids, vec!["hosted-account"]);
        let now = chrono::Utc::now();
        let balance = Balance {
            id: "hosted-account:balance".into(),
            account_id: binding.id.clone(),
            amount: Money {
                minor_units: 12500,
                currency: "USD".into(),
            },
            as_of: now,
            observed_at: now,
        };
        let coverage = Coverage {
            account_id: binding.id.clone(),
            capability: Capability::Balances,
            state: CoverageState::Complete,
            reason: None,
            checked_at: now,
            from: None,
            to: None,
        };
        store
            .ingest(
                &binding,
                &crate::utility::SyncPage {
                    observations: vec![Observation::Balance(balance.clone())],
                    coverage: vec![coverage.clone()],
                    continuation: None,
                },
            )
            .await
            .unwrap();
        let mut initial_coverage = coverage.clone();
        initial_coverage.state = CoverageState::Unavailable;
        initial_coverage.reason = Some("not_synced".into());
        let current = store
            .connection(&config.deployment, &record.connection.id)
            .await
            .unwrap();
        let replay = store
            .bind_connection(
                &config,
                &current,
                std::slice::from_ref(&binding),
                &[initial_coverage],
            )
            .await
            .unwrap();
        assert_eq!(replay.progress.linked_account_ids, vec!["hosted-account"]);
        let page: Page<Balance> = store
            .list(
                "balance",
                &ListQuery::default(),
                std::slice::from_ref(&binding.id),
            )
            .await
            .unwrap();
        assert_eq!(page.items, vec![balance]);
        assert_eq!(page.coverage, vec![coverage]);
        let mut different = binding.clone();
        different.services[0].provider_service = "different-meter".into();
        let current = store
            .connection(&config.deployment, &record.connection.id)
            .await
            .unwrap();
        assert!(matches!(
            store
                .bind_connection(&config, &current, &[different], &[])
                .await,
            Err(StoreError::BindingChanged)
        ));
        config.accounts.clear();
        store.register(&config).await.unwrap();
        assert!(matches!(
            store.binding(&config.deployment, &manual).await,
            Err(StoreError::NotFound)
        ));
        assert_eq!(store.bindings(&config.deployment).await.unwrap().len(), 1);
        assert_eq!(
            store
                .binding(&config.deployment, &binding.id)
                .await
                .unwrap()
                .provider_account,
            binding.provider_account
        );
        store.pool().close().await;
    }

    async fn explain(
        store: &Store,
        name: &str,
        mut query: sqlx::query::Query<'_, Sqlite, SqliteArguments>,
        write: bool,
    ) -> Vec<String> {
        let args = query.take_arguments().unwrap().unwrap_or_default();
        let statement = query.sql().as_str().to_owned();
        let prefix = if write {
            "EXPLAIN"
        } else {
            "EXPLAIN QUERY PLAN"
        };
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(format!("{prefix} {statement}")), args)
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert!(!rows.is_empty());
        let plans = if write {
            vec![format!("{} VM operations", rows.len())]
        } else {
            rows.iter()
                .map(|row| row.get::<String, _>("detail"))
                .collect()
        };
        println!("{name}: {}", plans.join("; "));
        plans
    }

    #[tokio::test]
    async fn connection_query_plans_on_representative_sqlite_data() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("plans.sqlite3"))
            .await
            .unwrap();
        for deployment in 0..25 {
            let deployment = format!("fixture-{deployment}");
            for _ in 0..40 {
                store
                    .create_connection(&deployment, &input(), "Speculoos Power")
                    .await
                    .unwrap();
            }
            let mut config: Config =
                toml::from_str(include_str!("../../config/example.toml")).unwrap();
            config.deployment = deployment.clone();
            for account in &mut config.accounts {
                account.id = format!("{deployment}-{}", account.id);
                account.deployment = deployment.clone();
            }
            store.register(&config).await.unwrap();
        }
        let record = store.queued_connection("fixture-0").await.unwrap().unwrap();
        for (name, query) in [
            (
                "scoped bindings",
                sqlx::query(queries::BINDINGS).bind("fixture-0"),
            ),
            (
                "scoped binding",
                sqlx::query(queries::BINDING)
                    .bind("fixture-0")
                    .bind("fixture-0-demo-wm"),
            ),
            (
                "connection",
                sqlx::query(queries::GET)
                    .bind("fixture-0")
                    .bind(&record.connection.id),
            ),
            (
                "idempotency request",
                sqlx::query(queries::BY_REQUEST)
                    .bind("fixture-0")
                    .bind(&record.connection.request_id),
            ),
            (
                "connection list",
                sqlx::query(queries::LIST).bind("fixture-0"),
            ),
            (
                "queued intent",
                sqlx::query(queries::QUEUED).bind("fixture-0"),
            ),
            (
                "scoped count",
                sqlx::query(queries::COUNT).bind("fixture-0"),
            ),
        ] {
            let plan = explain(&store, name, query, false).await;
            assert!(
                plan.iter().any(|part| part.contains("INDEX")),
                "{name}: {plan:?}"
            );
            assert!(
                !plan.iter().any(|part| part.contains("TEMP B-TREE")),
                "{name}: {plan:?}"
            );
        }
        let progress = serde_json::to_string(&record.connection.progress).unwrap();
        explain(
            &store,
            "create intent",
            sqlx::query(queries::CREATE)
                .bind("new-id")
                .bind("fixture-0")
                .bind("new-request")
                .bind("new-external")
                .bind("speculoos_power")
                .bind("Speculoos Power")
                .bind("Example")
                .bind("USD")
                .bind(&progress),
            true,
        )
        .await;
        explain(
            &store,
            "revision update",
            sqlx::query(queries::SAVE_PROGRESS)
                .bind(&progress)
                .bind(true)
                .bind("fixture-0")
                .bind(&record.connection.id)
                .bind(record.revision),
            true,
        )
        .await;
        explain(
            &store,
            "recover uncertain",
            sqlx::query(queries::RECOVER).bind("fixture-0"),
            true,
        )
        .await;
        explain(
            &store,
            "save binding",
            sqlx::query(queries::SAVE_BINDING)
                .bind("fixture-0")
                .bind("{}")
                .bind(&record.connection.id)
                .bind("fixture-0-demo-wm"),
            true,
        )
        .await;
        explain(
            &store,
            "remove manual bindings",
            sqlx::query(queries::CLEAR_MANUAL).bind("fixture-0"),
            true,
        )
        .await;
        store.pool().close().await;
    }
}
