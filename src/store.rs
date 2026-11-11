//! Reconciled observations, indexed keyset reads, and durable sync lifecycle.
pub(crate) mod connections;
mod imports;
use crate::{
    config::{AccountBinding, Config},
    models::*,
    utility::SyncPage,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use sqlx::{
    QueryBuilder, Row, Sqlite, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow},
};
use std::{path::Path, time::Duration};

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("stored data is invalid")]
    Serialization(#[from] serde_json::Error),
    #[error("invalid query or continuation")]
    InvalidQuery,
    #[error("account binding changed; explicit data migration is required")]
    BindingChanged,
    #[error("observation is outside the configured binding or interval")]
    InvalidObservation,
    #[error("a sync is already running for this account")]
    Busy,
    #[error("record not found")]
    NotFound,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Cursor {
    kind: String,
    query: String,
    sort_key: String,
    account_id: String,
    id: String,
}

fn enum_key<T: Serialize>(value: &T) -> Result<String, StoreError> {
    Ok(serde_json::to_value(value)?
        .as_str()
        .ok_or(StoreError::InvalidObservation)?
        .to_owned())
}

impl Store {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;
        sqlx::migrate!().run(&pool).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn register(&self, config: &Config) -> anyhow::Result<()> {
        config.validate()?;
        let mut tx = self.pool.begin().await?;
        // Configuration still owns manual bindings. Retain their observations,
        // but stop exposing a removed manual account. UI connections stay owned
        // by the database and are not removed by a configuration reload.
        sqlx::query(connections::queries::CLEAR_MANUAL)
            .bind(&config.deployment)
            .execute(&mut *tx)
            .await?;
        for binding in &config.accounts {
            if self.imported_binding_exists(&mut tx, binding).await? {
                return Err(StoreError::BindingChanged.into());
            }
            Self::register_binding(&mut tx, binding, None).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn recover_interrupted(&self) -> Result<(), StoreError> {
        sqlx::query(queries::RECOVER).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn coverage(&self) -> Result<Vec<Coverage>, StoreError> {
        sqlx::query_scalar::<_, String>(queries::COVERAGE)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .collect()
    }

    pub async fn set_coverage(&self, coverage: &[Coverage]) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        for item in coverage {
            Self::write_coverage(&mut tx, item).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn write_coverage(
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        coverage: &Coverage,
    ) -> Result<(), StoreError> {
        sqlx::query(queries::UPSERT_COVERAGE)
            .bind(&coverage.account_id)
            .bind(enum_key(&coverage.capability)?)
            .bind(serde_json::to_string(coverage)?)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    pub async fn ingest(
        &self,
        binding: &AccountBinding,
        page: &SyncPage,
    ) -> Result<(), StoreError> {
        self.ingest_page(binding, page, None).await
    }

    pub async fn ingest_job(
        &self,
        binding: &AccountBinding,
        page: &SyncPage,
        job_id: &str,
    ) -> Result<(), StoreError> {
        self.ingest_page(binding, page, Some(job_id)).await
    }

    async fn ingest_page(
        &self,
        binding: &AccountBinding,
        page: &SyncPage,
        job_id: Option<&str>,
    ) -> Result<(), StoreError> {
        // Reserve the SQLite writer before overlap reads. A deferred transaction
        // cannot upgrade a WAL snapshot after concurrent sync admission commits.
        let mut tx = self.pool.begin_with(queries::BEGIN_INGEST).await?;
        for observation in &page.observations {
            let row = ObservationRow::new(binding, observation)?;
            if row.kind == "usage" {
                // A revised same-ID interval is reconciled; overlapping distinct intervals are rejected.
                let overlap: bool = sqlx::query_scalar(queries::OVERLAP)
                    .bind(&binding.id)
                    .bind(row.service_id)
                    .bind(row.id)
                    .bind(&row.sort_key)
                    .bind(&row.end_key)
                    .fetch_one(&mut *tx)
                    .await?;
                if overlap {
                    return Err(StoreError::InvalidObservation);
                }
            }
            sqlx::query(queries::UPSERT_OBSERVATION)
                .bind(&binding.id)
                .bind(row.kind)
                .bind(row.id)
                .bind(row.service_id)
                .bind(row.sort_key)
                .bind(row.end_key)
                .bind(row.observed_at.to_rfc3339())
                .bind(row.payload)
                .execute(&mut *tx)
                .await?;
        }
        for coverage in &page.coverage {
            if coverage.account_id != binding.id {
                return Err(StoreError::InvalidObservation);
            }
            Self::write_coverage(&mut tx, coverage).await?;
        }
        if let Some(job_id) = job_id {
            let updated = sqlx::query(queries::ADVANCE_JOB)
                .bind(
                    i64::try_from(page.observations.len())
                        .map_err(|_| StoreError::InvalidObservation)?,
                )
                .bind(job_id)
                .bind(&binding.id)
                .execute(&mut *tx)
                .await?;
            if updated.rows_affected() != 1 {
                return Err(StoreError::InvalidObservation);
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn list<T: DeserializeOwned>(
        &self,
        kind: &str,
        query: &ListQuery,
        accounts: &[String],
    ) -> Result<Page<T>, StoreError> {
        let (mut sql, fingerprint) = list_sql(kind, query, accounts)?;
        let limit = query.limit.unwrap_or(50);
        sql.push(" LIMIT ").push_bind(i64::from(limit) + 1);
        let rows = sql.build().fetch_all(&self.pool).await?;
        let more = rows.len() > limit as usize;
        let mut items = Vec::new();
        let mut next_page = None;
        for row in rows.into_iter().take(limit as usize) {
            items.push(serde_json::from_str::<T>(row.try_get("payload")?)?);
            if more {
                next_page = Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Cursor {
                    kind: kind.to_owned(),
                    query: fingerprint.clone(),
                    sort_key: row.try_get("sort_key")?,
                    account_id: row.try_get("account_id")?,
                    id: row.try_get("id")?,
                })?));
            }
        }
        Ok(Page {
            items,
            next_page,
            coverage: self
                .coverage()
                .await?
                .into_iter()
                .filter(|c| accounts.contains(&c.account_id))
                .collect(),
        })
    }

    pub async fn bill(&self, id: &str, accounts: &[String]) -> Result<Bill, StoreError> {
        let mut query = QueryBuilder::<Sqlite>::new(queries::BILL_PREFIX);
        query.push_bind(id).push(" AND account_id IN (");
        bind_accounts(&mut query, accounts);
        query.push(") LIMIT 1");
        let payload = query
            .build_query_scalar::<String>()
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        Ok(serde_json::from_str(&payload)?)
    }

    pub async fn summary(&self, accounts: &[String]) -> Result<Summary, StoreError> {
        let mut query = summary_sql(accounts);
        let mut summary = Summary {
            accounts: accounts.len(),
            bills: 0,
            collections: 0,
            usage_intervals: 0,
            coverage: self
                .coverage()
                .await?
                .into_iter()
                .filter(|c| accounts.contains(&c.account_id))
                .collect(),
            last_synced_at: None,
            last_imported_at: None,
            import_only_accounts: 0,
            stale: true,
        };
        for row in query.build().fetch_all(&self.pool).await? {
            let count: i64 = row.try_get("count")?;
            match row.try_get::<&str, _>("kind")? {
                "bill" => {
                    summary.bills = count
                        .try_into()
                        .map_err(|_| StoreError::InvalidObservation)?
                }
                "collection" => {
                    summary.collections = count
                        .try_into()
                        .map_err(|_| StoreError::InvalidObservation)?
                }
                "usage" => {
                    summary.usage_intervals = count
                        .try_into()
                        .map_err(|_| StoreError::InvalidObservation)?
                }
                _ => (),
            }
        }
        let states = self.import_states(accounts).await?;
        let native = states
            .iter()
            .filter(|state| !state.import_only)
            .map(|state| state.id.clone())
            .collect::<Vec<_>>();
        summary.last_imported_at = states.iter().filter_map(|state| state.imported_at).max();
        summary.import_only_accounts = states.iter().filter(|state| state.import_only).count();
        let (last, stale) = if native.is_empty() {
            (None, summary.last_imported_at.is_none())
        } else {
            self.freshness(&native).await?
        };
        summary.last_synced_at = last;
        summary.stale = stale;
        Ok(summary)
    }

    async fn freshness(
        &self,
        accounts: &[String],
    ) -> Result<(Option<DateTime<Utc>>, bool), StoreError> {
        let mut query = freshness_sql(accounts);
        let row = query.build().fetch_one(&self.pool).await?;
        let parse = |value: Option<String>| {
            value
                .map(|s| {
                    DateTime::parse_from_rfc3339(&s)
                        .map(|d| d.with_timezone(&Utc))
                        .map_err(|_| StoreError::InvalidObservation)
                })
                .transpose()
        };
        let newest = parse(row.try_get("newest")?)?;
        let oldest = parse(row.try_get("oldest")?)?;
        let count: i64 = row.try_get("accounts")?;
        let stale = count as usize != accounts.len()
            || oldest.is_none_or(|time| Utc::now().signed_duration_since(time).num_hours() >= 24);
        Ok((newest, stale))
    }

    pub async fn start_job(&self, account: &str) -> Result<SyncJob, StoreError> {
        let id = uuid::Uuid::now_v7().to_string();
        let result = sqlx::query(queries::START_JOB)
            .bind(&id)
            .bind(account)
            .execute(&self.pool)
            .await;
        match result {
            Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
                return Err(StoreError::Busy);
            }
            Err(error) => return Err(error.into()),
            Ok(_) => (),
        }
        self.job(&id).await
    }

    pub async fn finish_job(
        &self,
        id: &str,
        state: SyncState,
        observations: u64,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(queries::FINISH_JOB)
            .bind(enum_key(&state)?)
            .bind(i64::try_from(observations).map_err(|_| StoreError::InvalidObservation)?)
            .bind(error)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn job(&self, id: &str) -> Result<SyncJob, StoreError> {
        let row = sqlx::query(queries::JOB)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(StoreError::NotFound)?;
        Self::decode_job(row)
    }

    pub async fn latest_jobs(&self, deployment: &str) -> Result<Vec<SyncJob>, StoreError> {
        sqlx::query(queries::LATEST_JOBS)
            .bind(deployment)
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(Self::decode_job)
            .collect()
    }

    fn decode_job(row: SqliteRow) -> Result<SyncJob, StoreError> {
        let started: String = row.try_get("started_at")?;
        let finished: Option<String> = row.try_get("finished_at")?;
        let state: String = row.try_get("state")?;
        Ok(SyncJob {
            id: row.try_get("id")?,
            account_id: row.try_get("account_id")?,
            state: serde_json::from_str(&serde_json::to_string(&state)?)?,
            started_at: DateTime::parse_from_rfc3339(&started)
                .map_err(|_| StoreError::InvalidObservation)?
                .with_timezone(&Utc),
            finished_at: finished
                .map(|s| {
                    DateTime::parse_from_rfc3339(&s)
                        .map(|d| d.with_timezone(&Utc))
                        .map_err(|_| StoreError::InvalidObservation)
                })
                .transpose()?,
            observations: u64::try_from(row.try_get::<i64, _>("observations")?)
                .map_err(|_| StoreError::InvalidObservation)?,
            error: row.try_get("error")?,
        })
    }
}

/// The same builder is exercised by the query-plan test, with real bound parameters.
pub fn list_sql<'a>(
    kind: &str,
    query: &'a ListQuery,
    accounts: &'a [String],
) -> Result<(QueryBuilder<Sqlite>, String), StoreError> {
    if !["bill", "balance", "collection", "usage"].contains(&kind)
        || !(1..=200).contains(&query.limit.unwrap_or(50))
        || query.from.zip(query.to).is_some_and(|(from, to)| from > to)
        || query
            .account
            .as_ref()
            .is_some_and(|a| !accounts.contains(a))
    {
        return Err(StoreError::InvalidQuery);
    }
    let mut canonical = query.clone();
    canonical.page = None;
    let fingerprint = URL_SAFE_NO_PAD.encode(Sha256::digest(serde_json::to_vec(&(
        kind, &canonical, accounts,
    ))?));
    let mut sql = QueryBuilder::<Sqlite>::new(queries::LIST_PREFIX);
    sql.push_bind(kind.to_owned()).push(" AND account_id IN (");
    {
        let mut list = sql.separated(",");
        for account in accounts {
            list.push_bind(account);
        }
    }
    sql.push(")");
    if let Some(account) = &query.account {
        sql.push(" AND account_id=").push_bind(account);
    }
    if let Some(service) = &query.service {
        sql.push(" AND service_id=").push_bind(service);
    }
    if let Some(from) = query.from {
        sql.push(" AND sort_key>=").push_bind(from.to_string());
    }
    if let Some(to) = query.to {
        let end = to.succ_opt().ok_or(StoreError::InvalidQuery)?;
        sql.push(" AND sort_key<").push_bind(end.to_string());
    }
    if let Some(token) = &query.page {
        if token.len() > 2048 {
            return Err(StoreError::InvalidQuery);
        }
        let cursor: Cursor = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(token)
                .map_err(|_| StoreError::InvalidQuery)?,
        )
        .map_err(|_| StoreError::InvalidQuery)?;
        if cursor.kind != kind || cursor.query != fingerprint {
            return Err(StoreError::InvalidQuery);
        }
        sql.push(if kind == "bill" {
            " AND (sort_key,account_id,id)<("
        } else {
            " AND (sort_key,account_id,id)>("
        })
        .push_bind(cursor.sort_key)
        .push(",")
        .push_bind(cursor.account_id)
        .push(",")
        .push_bind(cursor.id)
        .push(")");
    }
    sql.push(if kind == "bill" {
        " ORDER BY sort_key DESC,account_id DESC,id DESC"
    } else {
        " ORDER BY sort_key,account_id,id"
    });
    Ok((sql, fingerprint))
}

struct ObservationRow<'a> {
    kind: &'static str,
    id: &'a str,
    service_id: Option<&'a str>,
    sort_key: String,
    end_key: Option<String>,
    observed_at: DateTime<Utc>,
    payload: String,
}

impl<'a> ObservationRow<'a> {
    fn new(binding: &AccountBinding, observation: &'a Observation) -> Result<Self, StoreError> {
        let (kind, id, account, service, date, end, observed, payload) = match observation {
            Observation::Bill(v) => {
                if v.provider != binding.provider
                    || v.total.currency.len() != 3
                    || v.period_from.zip(v.period_to).is_some_and(|(a, b)| a > b)
                {
                    return Err(StoreError::InvalidObservation);
                }
                (
                    "bill",
                    v.id.as_str(),
                    v.account_id.as_str(),
                    None,
                    v.issued_on.to_string(),
                    None,
                    v.observed_at,
                    serde_json::to_string(v)?,
                )
            }
            Observation::Balance(v) => (
                "balance",
                v.id.as_str(),
                v.account_id.as_str(),
                None,
                v.as_of.to_rfc3339(),
                None,
                v.observed_at,
                serde_json::to_string(v)?,
            ),
            Observation::Payment(v) => (
                "payment",
                v.id.as_str(),
                v.account_id.as_str(),
                None,
                v.paid_on.to_string(),
                None,
                v.observed_at,
                serde_json::to_string(v)?,
            ),
            Observation::Service(v) => (
                "service",
                v.id.as_str(),
                binding.id.as_str(),
                Some(v.id.as_str()),
                String::new(),
                None,
                Utc::now(),
                serde_json::to_string(v)?,
            ),
            Observation::Collection(v) => (
                "collection",
                v.id.as_str(),
                v.account_id.as_str(),
                Some(v.service_id.as_str()),
                v.scheduled_date.to_string(),
                None,
                v.observed_at,
                serde_json::to_string(v)?,
            ),
            Observation::Usage(v) => {
                if v.start >= v.end || v.quantity.is_sign_negative() {
                    return Err(StoreError::InvalidObservation);
                }
                (
                    "usage",
                    v.id.as_str(),
                    v.account_id.as_str(),
                    Some(v.service_id.as_str()),
                    v.start.to_rfc3339(),
                    Some(v.end.to_rfc3339()),
                    v.observed_at,
                    serde_json::to_string(v)?,
                )
            }
        };
        if account != binding.id
            || id.is_empty()
            || id.len() > 500
            || service.is_some_and(|s| !binding.services.iter().any(|bound| bound.id == s))
        {
            return Err(StoreError::InvalidObservation);
        }
        Ok(Self {
            kind,
            id,
            service_id: service,
            sort_key: date,
            end_key: end,
            observed_at: observed,
            payload,
        })
    }
}

mod queries {
    pub const BEGIN_INGEST: &str = "BEGIN IMMEDIATE";
    pub const ADVANCE_JOB: &str = "UPDATE sync_jobs SET observations=observations+? WHERE id=? AND account_id=? AND state='running'";
    pub const REGISTER: &str =
        "INSERT INTO accounts(id,binding_hash) VALUES(?,?) ON CONFLICT(id) DO NOTHING";
    pub const BINDING: &str = "SELECT binding_hash FROM accounts WHERE id=?";
    pub const RECOVER: &str = "UPDATE sync_jobs SET state='interrupted',finished_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),error='process_interrupted' WHERE state='running'";
    pub const COVERAGE: &str = "SELECT payload FROM coverage ORDER BY account_id,capability";
    pub const UPSERT_COVERAGE: &str = "INSERT INTO coverage(account_id,capability,payload) VALUES(?,?,?) ON CONFLICT(account_id,capability) DO UPDATE SET payload=excluded.payload";
    pub const OVERLAP: &str = "SELECT EXISTS(SELECT 1 FROM (SELECT sort_key FROM observations WHERE account_id=? AND kind='usage' AND service_id=? AND id<>? AND end_key>? ORDER BY end_key LIMIT 1) WHERE sort_key<?)";
    pub const UPSERT_OBSERVATION: &str = "INSERT INTO observations(account_id,kind,id,service_id,sort_key,end_key,observed_at,payload) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(account_id,kind,id) DO UPDATE SET service_id=excluded.service_id,sort_key=excluded.sort_key,end_key=excluded.end_key,observed_at=excluded.observed_at,payload=excluded.payload WHERE excluded.observed_at >= observations.observed_at";
    pub const BILL_PREFIX: &str = "SELECT payload FROM observations WHERE kind='bill' AND id=";
    pub const SUMMARY_PREFIX: &str =
        "SELECT kind,count(*) AS count FROM observations WHERE account_id IN (";
    pub const FRESHNESS_PREFIX: &str = "SELECT count(*) AS accounts,max(finished_at) AS newest,min(finished_at) AS oldest FROM (SELECT account_id,max(finished_at) AS finished_at FROM sync_jobs WHERE state IN ('complete','partial') AND account_id IN (";
    pub const START_JOB: &str = "INSERT INTO sync_jobs(id,account_id,state,started_at) VALUES(?,?,'running',strftime('%Y-%m-%dT%H:%M:%fZ','now'))";
    pub const LATEST_JOBS: &str = "SELECT job.id,job.account_id,job.state,job.started_at,job.finished_at,job.observations,job.error
        FROM accounts AS account
        JOIN sync_jobs AS job ON job.id=(
            SELECT id FROM sync_jobs WHERE account_id=account.id ORDER BY started_at DESC,id DESC LIMIT 1
        )
        WHERE account.deployment=? AND account.binding IS NOT NULL
        ORDER BY account.id LIMIT 100";
    pub const FINISH_JOB: &str = "UPDATE sync_jobs SET state=?,finished_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),observations=?,error=? WHERE id=? AND state='running'";
    pub const JOB: &str = "SELECT id,account_id,state,started_at,finished_at,observations,error FROM sync_jobs WHERE id=?";
    pub const LIST_PREFIX: &str =
        "SELECT account_id,id,sort_key,payload FROM observations WHERE kind=";
}

fn bind_accounts(query: &mut QueryBuilder<Sqlite>, accounts: &[String]) {
    let mut list = query.separated(",");
    for account in accounts {
        list.push_bind(account.clone());
    }
}
fn summary_sql(accounts: &[String]) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new(queries::SUMMARY_PREFIX);
    bind_accounts(&mut query, accounts);
    query.push(") GROUP BY kind");
    query
}
fn freshness_sql(accounts: &[String]) -> QueryBuilder<Sqlite> {
    let mut query = QueryBuilder::new(queries::FRESHNESS_PREFIX);
    bind_accounts(&mut query, accounts);
    query.push(") GROUP BY account_id)");
    query
}

#[cfg(test)]
mod tests {
    use crate::{
        config::Config,
        models::*,
        store::{Store, bind_accounts, freshness_sql, list_sql, queries, summary_sql},
        utility::SyncPage,
    };
    use chrono::{Duration, Utc};
    use rust_decimal::Decimal;
    use sqlx::{Execute, QueryBuilder, Row, Sqlite, sqlite::SqliteArguments};

    async fn explain(
        name: &str,
        store: &Store,
        mut query: sqlx::query::Query<'_, Sqlite, SqliteArguments>,
    ) -> Vec<String> {
        let arguments = query.take_arguments().unwrap().unwrap_or_default();
        let statement = query.sql().as_str().to_owned();
        let rows = sqlx::query_with(
            sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {statement}")),
            arguments,
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        let plans: Vec<String> = rows
            .iter()
            .map(|row| row.get::<String, _>("detail"))
            .collect();
        println!("{name}: {}", plans.join("; "));
        plans
    }
    async fn explain_write(
        name: &str,
        store: &Store,
        mut query: sqlx::query::Query<'_, Sqlite, SqliteArguments>,
    ) {
        let arguments = query.take_arguments().unwrap().unwrap_or_default();
        let statement = query.sql().as_str().to_owned();
        let rows = sqlx::query_with(
            sqlx::AssertSqlSafe(format!("EXPLAIN {statement}")),
            arguments,
        )
        .fetch_all(store.pool())
        .await
        .unwrap();
        assert!(!rows.is_empty());
        println!("{name}: {} SQLite VM operations", rows.len());
    }

    #[tokio::test]
    async fn query_plans_on_representative_data() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("plans.sqlite3"))
            .await
            .unwrap();
        let config: Config = toml::from_str(include_str!("../config/example.toml")).unwrap();
        store.register(&config).await.unwrap();
        let now = Utc::now();
        let start = (now - Duration::days(110))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        for binding in &config.accounts {
            let mut observations = Vec::new();
            for index in 0..2000 {
                observations.push(Observation::Bill(Bill {
                    id: format!("{}:bill:{index:05}", binding.id),
                    account_id: binding.id.clone(),
                    provider: binding.provider,
                    issued_on: now.date_naive() - Duration::days(index % 365),
                    due_on: None,
                    period_from: None,
                    period_to: None,
                    total: Money {
                        minor_units: 1234,
                        currency: "USD".to_owned(),
                    },
                    new_charges: None,
                    reported_quantities: vec![],
                    outstanding_balance: None,
                    source_status: None,
                    observed_at: now,
                }));
            }
            if binding.provider == ProviderId::Bayou {
                for index in 0..10_000 {
                    let at = start + Duration::minutes(index * 15);
                    observations.push(Observation::Usage(Usage {
                        id: format!("demo-energy:usage:{index:05}"),
                        account_id: binding.id.clone(),
                        service_id: "demo-electric".to_owned(),
                        start: at,
                        end: at + Duration::minutes(15),
                        quantity: Decimal::from(20),
                        unit: UsageUnit::Wh,
                        observed_at: now,
                    }));
                }
            }
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
            for _ in 0..100 {
                let job = store.start_job(&binding.id).await.unwrap();
                store
                    .finish_job(&job.id, SyncState::Complete, 1, None)
                    .await
                    .unwrap();
            }
        }
        sqlx::query("ANALYZE").execute(store.pool()).await.unwrap();
        let accounts = config
            .accounts
            .iter()
            .map(|a| a.id.clone())
            .collect::<Vec<_>>();
        for (kind, query) in [
            (
                "bill",
                ListQuery {
                    from: Some(now.date_naive() - Duration::days(30)),
                    to: Some(now.date_naive()),
                    limit: Some(25),
                    ..Default::default()
                },
            ),
            (
                "bill",
                ListQuery {
                    account: Some("demo-wm".to_owned()),
                    limit: Some(25),
                    ..Default::default()
                },
            ),
            (
                "usage",
                ListQuery {
                    service: Some("demo-electric".to_owned()),
                    from: Some(start.date_naive() + Duration::days(90)),
                    to: Some(start.date_naive() + Duration::days(95)),
                    limit: Some(25),
                    ..Default::default()
                },
            ),
        ] {
            let page: Page<Bill> = if kind == "bill" {
                store.list(kind, &query, &accounts).await.unwrap()
            } else {
                Page {
                    items: vec![],
                    next_page: None,
                    coverage: vec![],
                }
            };
            let (mut sql, _) = list_sql(kind, &query, &accounts).unwrap();
            sql.push(" LIMIT ").push_bind(26_i64);
            let plans = explain(kind, &store, sql.build()).await;
            assert!(plans.iter().any(|p| p.contains("INDEX")));
            assert!(!plans.iter().any(|p| p == "SCAN observations"));
            if let Some(cursor) = page.next_page {
                let mut next = query.clone();
                next.page = Some(cursor);
                let (mut sql, _) = list_sql(kind, &next, &accounts).unwrap();
                sql.push(" LIMIT ").push_bind(26_i64);
                explain("keyset_page", &store, sql.build()).await;
            }
        }
        let mut summary = summary_sql(&accounts);
        explain("summary", &store, summary.build()).await;
        let mut freshness = freshness_sql(&accounts);
        explain("freshness", &store, freshness.build()).await;
        let mut detail = QueryBuilder::<Sqlite>::new(queries::BILL_PREFIX);
        detail
            .push_bind("demo-wm:bill:00001")
            .push(" AND account_id IN (");
        bind_accounts(&mut detail, &accounts);
        detail.push(") LIMIT 1");
        explain("bill_detail", &store, detail.build()).await;
        explain(
            "binding",
            &store,
            sqlx::query(queries::BINDING).bind("demo-wm"),
        )
        .await;
        explain("coverage", &store, sqlx::query(queries::COVERAGE)).await;
        explain(
            "overlap",
            &store,
            sqlx::query(queries::OVERLAP)
                .bind("demo-energy")
                .bind("demo-electric")
                .bind("candidate")
                .bind((start + Duration::hours(2)).to_rfc3339())
                .bind(start.to_rfc3339()),
        )
        .await;
        explain(
            "job",
            &store,
            sqlx::query(queries::JOB).bind("fictional-job"),
        )
        .await;
        explain(
            "latest_jobs",
            &store,
            sqlx::query(queries::LATEST_JOBS).bind(&config.deployment),
        )
        .await;
        let started = std::time::Instant::now();
        let jobs = store.latest_jobs(&config.deployment).await.unwrap();
        println!(
            "Latest sync per account in 200 jobs: {:?}",
            started.elapsed()
        );
        assert_eq!(jobs.len(), config.accounts.len());
        assert!(
            store
                .latest_jobs("another-deployment")
                .await
                .unwrap()
                .is_empty()
        );
        explain_write(
            "register",
            &store,
            sqlx::query(queries::REGISTER)
                .bind("demo-new")
                .bind("fictional-hash"),
        )
        .await;
        explain_write("begin_ingest", &store, sqlx::query(queries::BEGIN_INGEST)).await;
        explain_write(
            "advance_job",
            &store,
            sqlx::query(queries::ADVANCE_JOB)
                .bind(25_i64)
                .bind("fictional-job")
                .bind("demo-energy"),
        )
        .await;
        explain_write("recover", &store, sqlx::query(queries::RECOVER)).await;
        explain_write(
            "upsert_coverage",
            &store,
            sqlx::query(queries::UPSERT_COVERAGE)
                .bind("demo-wm")
                .bind("bills")
                .bind("{}"),
        )
        .await;
        explain_write(
            "upsert_observation",
            &store,
            sqlx::query(queries::UPSERT_OBSERVATION)
                .bind("demo-wm")
                .bind("bill")
                .bind("fictional-id")
                .bind(Option::<String>::None)
                .bind("2026-01-01")
                .bind(Option::<String>::None)
                .bind(now.to_rfc3339())
                .bind("{}"),
        )
        .await;
        explain_write(
            "start_job",
            &store,
            sqlx::query(queries::START_JOB)
                .bind("fictional-job")
                .bind("demo-wm"),
        )
        .await;
        explain_write(
            "finish_job",
            &store,
            sqlx::query(queries::FINISH_JOB)
                .bind("complete")
                .bind(1_i64)
                .bind(Option::<String>::None)
                .bind("fictional-job"),
        )
        .await;
        let started = std::time::Instant::now();
        let page = store
            .list::<Usage>(
                "usage",
                &ListQuery {
                    service: Some("demo-electric".to_owned()),
                    limit: Some(25),
                    ..Default::default()
                },
                &accounts,
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 25);
        println!(
            "14,000 observations, 200 sync jobs; bounded usage page runtime: {:?}",
            started.elapsed()
        );
    }
}
