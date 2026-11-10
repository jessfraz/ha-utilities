//! Explicit hosted onboarding, with durable intents and an owned background worker.
//!
//! Bayou documents unique external IDs and authoritative GET status. A private HA
//! installation therefore needs no public callback or unverified browser event.
//! https://docs.bayou.energy/docs/customer-lifecycle
//! https://docs.bayou.energy/reference/get_customers-id

use std::{collections::HashSet, sync::Arc, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, watch};
use url::Url;
use uuid::Uuid;

use crate::{
    config::{AccountBinding, Config, ServiceBinding, valid_id},
    models::{
        BindConnection, ConnectableUtility, Connection, ConnectionAuthStatus,
        ConnectionAuthorization, ConnectionIssue, ConnectionProgress, ConnectionState,
        CreateConnection, ProviderId, ServiceKind,
    },
    store::{Store, StoreError, connections::ConnectionRecord},
    sync::Providers,
    utility::{
        UtilityError,
        bayou::{Bayou, BayouConnection},
    },
};

#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("connection input is invalid")]
    InvalidInput,
    #[error("accept the Bayou workspace data and billing settings to continue")]
    ConsentRequired,
    #[error("Bayou credentials are not configured")]
    NotConfigured,
    #[error("connection worker is unavailable")]
    WorkerUnavailable,
    #[error("connection changed; refresh its status before continuing")]
    Changed,
    #[error("the utility account is not ready to be connected")]
    NotReady,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Provider(#[from] UtilityError),
}

struct ConnectionEngine {
    store: Store,
    config: Arc<Config>,
    providers: Arc<Providers>,
}

#[derive(Clone)]
pub struct ConnectionHandle {
    engine: Arc<ConnectionEngine>,
    wake: Arc<Notify>,
    running: watch::Receiver<bool>,
}

/// Main owns and joins this worker before closing SQLite. Requests only commit
/// intents; disconnecting a browser cannot abandon a provider POST halfway through.
pub struct ConnectionWorker {
    engine: Arc<ConnectionEngine>,
    wake: Arc<Notify>,
    running: watch::Sender<bool>,
}

impl ConnectionWorker {
    pub async fn new(
        store: Store,
        config: Arc<Config>,
        providers: Arc<Providers>,
    ) -> Result<(ConnectionHandle, Self), StoreError> {
        store.recover_connections(&config.deployment).await?;
        let engine = Arc::new(ConnectionEngine {
            store,
            config,
            providers,
        });
        let wake = Arc::new(Notify::new());
        let (running, receiver) = watch::channel(true);
        Ok((
            ConnectionHandle {
                engine: engine.clone(),
                wake: wake.clone(),
                running: receiver,
            },
            Self {
                engine,
                wake,
                running,
            },
        ))
    }

    pub async fn run(self, stop: watch::Receiver<bool>) -> Result<(), StoreError> {
        let result = self.run_loop(stop).await;
        // A stopped in-flight creation is uncertain, never automatically replayed.
        let recovery = self
            .engine
            .store
            .recover_connections(&self.engine.config.deployment)
            .await;
        self.running.send_replace(false);
        result.and(recovery)
    }

    async fn run_loop(&self, mut stop: watch::Receiver<bool>) -> Result<(), StoreError> {
        loop {
            if *stop.borrow() || stop.has_changed().is_err() {
                return Ok(());
            }
            let pending = self
                .engine
                .store
                .queued_connection(&self.engine.config.deployment)
                .await?;
            if let Some(record) = pending {
                let mut progress = record.connection.progress.clone();
                progress.state = ConnectionState::Creating;
                if !self
                    .engine
                    .store
                    .save_connection_progress(
                        &self.engine.config.deployment,
                        &record,
                        &progress,
                        false,
                    )
                    .await?
                {
                    continue;
                }
                let record = self.engine.record(&record.connection.id).await?;
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    result = self.engine.create(&record) => result?,
                }
                continue;
            }
            tokio::select! {
                _ = stop.changed() => return Ok(()),
                _ = self.wake.notified() => {},
                _ = tokio::time::sleep(Duration::from_secs(1)) => {},
            }
        }
    }
}

impl ConnectionHandle {
    fn ensure_running(&self) -> Result<(), ConnectionError> {
        if !*self.running.borrow() || self.running.has_changed().is_err() {
            Err(ConnectionError::WorkerUnavailable)
        } else {
            Ok(())
        }
    }

    pub async fn utilities(&self) -> Result<Vec<ConnectableUtility>, ConnectionError> {
        Ok(self.engine.bayou()?.connectable_utilities().await?)
    }

    pub async fn list(&self) -> Result<Vec<Connection>, ConnectionError> {
        Ok(self
            .engine
            .store
            .connections(&self.engine.config.deployment)
            .await?)
    }

    pub async fn get(&self, id: &str) -> Result<Connection, ConnectionError> {
        Ok(self.engine.record(id).await?.connection)
    }

    pub async fn start(&self, mut input: CreateConnection) -> Result<Connection, ConnectionError> {
        self.ensure_running()?;
        if !input.accept_provider_settings {
            return Err(ConnectionError::ConsentRequired);
        }
        let request =
            Uuid::parse_str(&input.request_id).map_err(|_| ConnectionError::InvalidInput)?;
        if request.is_nil() {
            return Err(ConnectionError::InvalidInput);
        }
        input.request_id = request.to_string();
        input.utility_id = input.utility_id.trim().to_owned();
        input.currency = input.currency.trim().to_ascii_uppercase();
        if !valid_id(&input.utility_id) || !matches!(input.currency.as_str(), "USD" | "CAD") {
            return Err(ConnectionError::InvalidInput);
        }
        if let Some(name) = input.name.as_mut() {
            *name = name.trim().to_owned();
            if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
                return Err(ConnectionError::InvalidInput);
            }
        }
        if let Some(record) = self
            .engine
            .store
            .connection_request(&self.engine.config.deployment, &input.request_id)
            .await?
        {
            let connection = record.connection;
            if connection.utility_id != input.utility_id
                || connection.currency != input.currency
                || connection.name != input.name.as_deref().unwrap_or(&connection.utility_name)
            {
                return Err(ConnectionError::Changed);
            }
            return Ok(connection);
        }
        let utility = self
            .utilities()
            .await?
            .into_iter()
            .find(|utility| utility.id == input.utility_id)
            .ok_or(ConnectionError::InvalidInput)?;
        let (record, _) = self
            .engine
            .store
            .create_connection(&self.engine.config.deployment, &input, &utility.name)
            .await?;
        self.wake.notify_one();
        Ok(record.connection)
    }

    pub async fn refresh(&self, id: &str) -> Result<Connection, ConnectionError> {
        let record = self.engine.record(id).await?;
        match self.engine.lookup(&record).await {
            Ok(Some(customer)) => Ok(self.engine.apply_customer(&record, &customer).await?),
            Ok(None)
                if matches!(
                    record.connection.progress.state,
                    ConnectionState::Queued | ConnectionState::Creating
                ) =>
            {
                Ok(record.connection)
            }
            Ok(None) => Ok(self
                .engine
                .note_issue(&record, ConnectionIssue::CustomerNotFound, true)
                .await?),
            Err(error) => Ok(self
                .engine
                .note_issue(&record, issue(&error), false)
                .await?),
        }
    }

    pub async fn resume(&self, id: &str) -> Result<Connection, ConnectionError> {
        self.ensure_running()?;
        let record = self.engine.record(id).await?;
        if matches!(
            record.connection.progress.state,
            ConnectionState::Queued | ConnectionState::Creating
        ) {
            return Ok(record.connection);
        }
        if let Some(customer) = self.engine.lookup(&record).await? {
            return Ok(self.engine.apply_customer(&record, &customer).await?);
        }
        if record.connection.progress.customer_id.is_some() {
            return Ok(self
                .engine
                .note_issue(&record, ConnectionIssue::CustomerNotFound, true)
                .await?);
        }
        let mut progress = record.connection.progress.clone();
        progress.state = ConnectionState::Queued;
        progress.issue = None;
        if !self
            .engine
            .store
            .save_connection_progress(&self.engine.config.deployment, &record, &progress, false)
            .await?
        {
            return Err(ConnectionError::Changed);
        }
        self.wake.notify_one();
        self.get(id).await
    }

    pub async fn authorization(
        &self,
        id: &str,
    ) -> Result<ConnectionAuthorization, ConnectionError> {
        let record = self.engine.record(id).await?;
        let customer = self
            .engine
            .lookup(&record)
            .await?
            .ok_or(ConnectionError::NotReady)?;
        self.engine.apply_customer(&record, &customer).await?;
        let link = customer.authorization.ok_or(ConnectionError::NotReady)?;
        authorization_link(&link)
    }

    pub async fn bind(
        &self,
        id: &str,
        input: BindConnection,
    ) -> Result<Connection, ConnectionError> {
        let mut record = self.engine.record(id).await?;
        let customer = self
            .engine
            .lookup(&record)
            .await?
            .ok_or(ConnectionError::NotReady)?;
        if !customer.authenticated
            || customer.auth_status != Some(ConnectionAuthStatus::Authenticated)
            || input.accounts.is_empty()
            || input.accounts.len() > 100
        {
            return Err(ConnectionError::NotReady);
        }
        record.connection.progress = progress(&record.connection.progress, &customer);
        let mut selected = HashSet::new();
        let mut bindings = Vec::new();
        for selection in input.accounts {
            if !selected.insert(selection.account.clone())
                || selection.meters.is_empty()
                || selection.meters.len() > 64
            {
                return Err(ConnectionError::InvalidInput);
            }
            let account = customer
                .accounts
                .iter()
                .find(|account| account.id == selection.account)
                .ok_or(ConnectionError::InvalidInput)?;
            let mut selected_meters = HashSet::new();
            let mut services = Vec::new();
            for meter_id in selection.meters {
                if !selected_meters.insert(meter_id.clone()) {
                    return Err(ConnectionError::InvalidInput);
                }
                let meter = account
                    .meters
                    .iter()
                    .find(|meter| meter.id == meter_id)
                    .ok_or(ConnectionError::InvalidInput)?;
                services.push(ServiceBinding {
                    id: local_id("meter", &[id, &account.id, &meter.id]),
                    name: {
                        let kind = match meter.kind {
                            ServiceKind::Electricity => "Electricity",
                            ServiceKind::Gas => "Gas",
                            _ => return Err(ConnectionError::InvalidInput),
                        };
                        if account.meters.len() > 1 {
                            format!("{kind} meter ending {}", ending(&meter.id))
                        } else {
                            kind.to_owned()
                        }
                    },
                    provider_service: meter.id.clone(),
                    kind: meter.kind,
                });
            }
            // Stable ordering makes an identical selection a true replay.
            services.sort_by(|left, right| left.id.cmp(&right.id));
            bindings.push(AccountBinding {
                id: local_id("bayou", &[id, &account.id]),
                name: if customer.accounts.len() > 1 {
                    format!(
                        "{} · account ending {}",
                        record.connection.name,
                        ending(&account.id)
                    )
                } else {
                    record.connection.name.clone()
                },
                deployment: self.engine.config.deployment.clone(),
                provider: ProviderId::Bayou,
                customer_id: customer.customer_id.clone(),
                provider_account: account.id.clone(),
                currency: record.connection.currency.clone(),
                services,
            });
        }
        let coverage = bindings
            .iter()
            .flat_map(|binding| self.engine.providers.initial_coverage(binding))
            .collect::<Vec<_>>();
        Ok(self
            .engine
            .store
            .bind_connection(&self.engine.config, &record, &bindings, &coverage)
            .await?)
    }
}

impl ConnectionEngine {
    fn bayou(&self) -> Result<&Bayou, ConnectionError> {
        self.providers.bayou().ok_or(ConnectionError::NotConfigured)
    }

    async fn record(&self, id: &str) -> Result<ConnectionRecord, StoreError> {
        if Uuid::parse_str(id).is_err() {
            return Err(StoreError::NotFound);
        }
        self.store.connection(&self.config.deployment, id).await
    }

    async fn lookup(
        &self,
        record: &ConnectionRecord,
    ) -> Result<Option<BayouConnection>, UtilityError> {
        self.providers
            .bayou()
            .ok_or(UtilityError::NotConfigured)?
            .connection(
                &record.external_id,
                &record.connection.utility_id,
                record.connection.progress.customer_id.as_deref(),
            )
            .await
    }

    async fn create(&self, record: &ConnectionRecord) -> Result<(), StoreError> {
        // A queued intent may have survived a lost response or a restart.
        // Only an authoritative absence can precede a new create request.
        match self.lookup(record).await {
            Ok(Some(customer)) => {
                self.apply_customer(record, &customer).await?;
                return Ok(());
            }
            Err(error) => {
                self.note_issue(record, issue(&error), false).await?;
                return Ok(());
            }
            Ok(None) if record.connection.progress.customer_id.is_some() => {
                self.note_issue(record, ConnectionIssue::CustomerNotFound, true)
                    .await?;
                return Ok(());
            }
            Ok(None) => {}
        }
        let created = match self.providers.bayou() {
            Some(bayou) => {
                bayou
                    .create_connection(&record.external_id, &record.connection.utility_id)
                    .await
            }
            None => Err(UtilityError::NotConfigured),
        };
        // Always reconcile by the saved external ID, including a lost POST response.
        match self.lookup(record).await {
            Ok(Some(customer)) => {
                self.apply_customer(record, &customer).await?;
            }
            Ok(None) => {
                self.note_issue(
                    record,
                    created
                        .err()
                        .as_ref()
                        .map(issue)
                        .unwrap_or(ConnectionIssue::CustomerNotFound),
                    true,
                )
                .await?;
            }
            Err(error) => {
                self.note_issue(record, issue(&error), false).await?;
            }
        }
        Ok(())
    }

    async fn apply_customer(
        &self,
        record: &ConnectionRecord,
        customer: &BayouConnection,
    ) -> Result<Connection, StoreError> {
        let progress = progress(&record.connection.progress, customer);
        // A delayed response must not overwrite newer status or a confirmed binding.
        self.store
            .save_connection_progress(&self.config.deployment, record, &progress, true)
            .await?;
        let current = self.record(&record.connection.id).await?.connection;
        if current
            .progress
            .customer_id
            .as_deref()
            .is_some_and(|id| id != customer.customer_id)
        {
            return Err(StoreError::BindingChanged);
        }
        Ok(current)
    }

    async fn note_issue(
        &self,
        record: &ConnectionRecord,
        issue: ConnectionIssue,
        checked: bool,
    ) -> Result<Connection, StoreError> {
        let mut progress = record.connection.progress.clone();
        progress.state =
            if progress.customer_id.is_none() || issue == ConnectionIssue::CustomerNotFound {
                ConnectionState::Unconfirmed
            } else {
                ConnectionState::Unavailable
            };
        progress.issue = Some(issue);
        self.store
            .save_connection_progress(&self.config.deployment, record, &progress, checked)
            .await?;
        Ok(self.record(&record.connection.id).await?.connection)
    }
}

fn authorization_link(link: &str) -> Result<ConnectionAuthorization, ConnectionError> {
    if link.len() > 8192 || link.chars().any(char::is_control) {
        return Err(ConnectionError::Provider(UtilityError::InvalidResponse));
    }
    let url =
        Url::parse(link).map_err(|_| ConnectionError::Provider(UtilityError::InvalidResponse))?;
    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    let keys = url.query_pairs().collect::<Vec<_>>();
    if url.scheme() != "https"
        || url.host_str() != Some("bayou.energy")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || segments.len() != 3
        || segments[0] != "companies"
        || segments[1].is_empty()
        || !segments[1].bytes().all(|byte| byte.is_ascii_digit())
        || segments[2] != "sign_up"
        || keys.len() != 1
        || keys[0].0 != "key"
        || keys[0].1.is_empty()
    {
        return Err(ConnectionError::Provider(UtilityError::InvalidResponse));
    }
    Ok(ConnectionAuthorization {
        url: url.to_string(),
    })
}

fn progress(previous: &ConnectionProgress, customer: &BayouConnection) -> ConnectionProgress {
    let mut next = previous.clone();
    next.customer_id = Some(customer.customer_id.clone());
    next.auth_status = customer.auth_status;
    next.has_filled_credentials = customer.has_filled_credentials;
    next.bills_ready = customer.bills_ready;
    next.intervals_ready = customer.intervals_ready;
    next.accounts = customer.accounts.clone();
    next.issue = None;
    next.state = if customer.authenticated
        && customer.auth_status == Some(ConnectionAuthStatus::Authenticated)
    {
        if !next.linked_account_ids.is_empty() {
            ConnectionState::Connected
        } else if next
            .accounts
            .iter()
            .any(|account| !account.meters.is_empty())
        {
            ConnectionState::Ready
        } else {
            ConnectionState::Discovering
        }
    } else {
        match customer.auth_status {
            Some(
                ConnectionAuthStatus::OnboardingCredentialsNeeded
                | ConnectionAuthStatus::OnboardingMfaNeeded
                | ConnectionAuthStatus::OnboardingReauthNeeded
                | ConnectionAuthStatus::OnboardingCustomerActionRequired,
            ) => ConnectionState::AwaitingAuthorization,
            Some(
                ConnectionAuthStatus::RefreshMfaNeeded
                | ConnectionAuthStatus::RefreshReauthNeeded
                | ConnectionAuthStatus::RefreshCustomerActionRequired,
            ) => ConnectionState::NeedsAuthentication,
            Some(
                ConnectionAuthStatus::OnboardingAuthIssue
                | ConnectionAuthStatus::RefreshAuthIssue
                | ConnectionAuthStatus::Unknown,
            ) => {
                next.issue = Some(ConnectionIssue::ProviderUnavailable);
                ConnectionState::Unavailable
            }
            _ if customer.has_filled_credentials => ConnectionState::Authenticating,
            _ => ConnectionState::AwaitingAuthorization,
        }
    };
    next
}

fn issue(error: &UtilityError) -> ConnectionIssue {
    match error {
        UtilityError::NotConfigured | UtilityError::Authentication => {
            ConnectionIssue::AuthenticationRequired
        }
        UtilityError::AccessDenied => ConnectionIssue::AccessDenied,
        UtilityError::RateLimited => ConnectionIssue::RateLimited,
        UtilityError::Unavailable => ConnectionIssue::ProviderUnavailable,
        _ => ConnectionIssue::InvalidResponse,
    }
}

fn local_id(prefix: &str, values: &[&str]) -> String {
    let mut hash = Sha256::new();
    for value in values {
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    format!("{prefix}-{}", URL_SAFE_NO_PAD.encode(hash.finalize()))
}

fn ending(value: &str) -> String {
    value
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::{
        config::Config,
        connect::{ConnectionEngine, ConnectionWorker, authorization_link},
        models::*,
        store::Store,
        sync::Providers,
        utility::bayou::BayouConnection,
    };
    use std::sync::Arc;

    #[test]
    fn hosted_authorization_accepts_only_the_documented_bayou_origin_and_path() {
        assert!(
            authorization_link("https://bayou.energy/companies/12345/sign_up?key=fictional")
                .is_ok()
        );
        for link in [
            "http://bayou.energy/companies/12345/sign_up?key=x",
            "https://bayou.energy.evil.invalid/companies/12345/sign_up?key=x",
            "https://user@bayou.energy/companies/12345/sign_up?key=x",
            "https://bayou.energy:444/companies/12345/sign_up?key=x",
            "https://bayou.energy/companies/12345/sign_up?key=x#secret",
            "https://bayou.energy/companies/12345/sign_up?key=x&redirect=https://evil.invalid",
            "https://bayou.energy/companies/a/sign_up?key=x",
            "https://bayou.energy/companies/12345/sign_up?key=",
            "https://bayou.energy/companies/12345/sign_up?key=x\n",
        ] {
            assert!(authorization_link(link).is_err(), "accepted {link}");
        }
    }

    #[tokio::test]
    async fn old_provider_success_and_failure_cannot_overwrite_newer_status() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("status.sqlite3"))
            .await
            .unwrap();
        let config: Config = toml::from_str(include_str!("../config/example.toml")).unwrap();
        let input = CreateConnection {
            provider: ConnectionProvider::Bayou,
            request_id: uuid::Uuid::now_v7().to_string(),
            utility_id: "speculoos_power".into(),
            name: None,
            currency: "USD".into(),
            accept_provider_settings: true,
        };
        let (record, _) = store
            .create_connection(&config.deployment, &input, "Speculoos Power")
            .await
            .unwrap();
        let engine = ConnectionEngine {
            store: store.clone(),
            config: Arc::new(config),
            providers: Arc::new(Providers::with_clients(None, None)),
        };
        let mut customer = BayouConnection {
            customer_id: "71001".into(),
            auth_status: Some(ConnectionAuthStatus::Authenticated),
            has_filled_credentials: true,
            authenticated: true,
            bills_ready: false,
            intervals_ready: false,
            accounts: vec![ConnectionAccount {
                id: "example-account".into(),
                meters: vec![ConnectionMeter {
                    id: "example-meter".into(),
                    kind: ServiceKind::Electricity,
                }],
            }],
            authorization: None,
        };
        let success = engine.apply_customer(&record, &customer).await.unwrap();
        assert_eq!(success.progress.state, ConnectionState::Ready);
        let failed = engine
            .note_issue(&record, ConnectionIssue::ProviderUnavailable, false)
            .await
            .unwrap();
        assert_eq!(failed, success);
        customer.authenticated = false;
        customer.auth_status = Some(ConnectionAuthStatus::OnboardingCredentialsNeeded);
        assert_eq!(
            engine.apply_customer(&record, &customer).await.unwrap(),
            success
        );
        store.pool().close().await;
    }

    #[tokio::test]
    async fn worker_startup_recovers_creating_without_queuing_another_post() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("restart.sqlite3"))
            .await
            .unwrap();
        let config: Config = toml::from_str(include_str!("../config/example.toml")).unwrap();
        let input = CreateConnection {
            provider: ConnectionProvider::Bayou,
            request_id: uuid::Uuid::now_v7().to_string(),
            utility_id: "speculoos_power".into(),
            name: None,
            currency: "USD".into(),
            accept_provider_settings: true,
        };
        let (record, _) = store
            .create_connection(&config.deployment, &input, "Speculoos Power")
            .await
            .unwrap();
        let mut progress = record.connection.progress.clone();
        progress.state = ConnectionState::Creating;
        store
            .save_connection_progress(&config.deployment, &record, &progress, false)
            .await
            .unwrap();
        let deployment = config.deployment.clone();
        let (handle, worker) = ConnectionWorker::new(
            store.clone(),
            Arc::new(config),
            Arc::new(Providers::with_clients(None, None)),
        )
        .await
        .unwrap();
        assert_eq!(
            handle
                .get(&record.connection.id)
                .await
                .unwrap()
                .progress
                .state,
            ConnectionState::Unconfirmed
        );
        assert!(
            store
                .queued_connection(&deployment)
                .await
                .unwrap()
                .is_none()
        );
        drop(worker);
        assert!(matches!(
            handle.ensure_running(),
            Err(crate::connect::ConnectionError::WorkerUnavailable)
        ));
        store.pool().close().await;
    }
}
