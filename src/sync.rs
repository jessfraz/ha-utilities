//! A single owned worker runs bounded read-only syncs and is joined before SQLite closes.
use crate::{
    config::Config,
    models::*,
    store::{Store, StoreError, require_mode},
    utility::{
        HistoryRequest, SyncRequest, SyncStage, Utility, UtilityError, bayou::Bayou,
        report_failure, report_terminal_failure, waste_management::WasteManagement,
    },
};
use chrono::{Days, NaiveDate, Utc};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};

const ALL: &[Capability] = &[
    Capability::Bills,
    Capability::Balances,
    Capability::Payments,
    Capability::Services,
    Capability::Collections,
    Capability::Usage,
];

enum Adapter {
    Wm(WasteManagement),
    Bayou(Bayou),
}
impl Adapter {
    fn provider(&self) -> LiveProvider {
        match self {
            Self::Wm(adapter) => adapter.provider(),
            Self::Bayou(adapter) => adapter.provider(),
        }
    }
    async fn sync(&self, request: SyncRequest) -> Result<crate::utility::SyncPage, UtilityError> {
        match self {
            Self::Wm(adapter) => adapter.sync(request).await,
            Self::Bayou(adapter) => adapter.sync(request).await,
        }
    }

    async fn sync_history(
        &self,
        request: HistoryRequest,
    ) -> Result<crate::utility::SyncPage, UtilityError> {
        match self {
            Self::Wm(adapter) => adapter.sync_history(request).await,
            Self::Bayou(adapter) => adapter.sync_history(request).await,
        }
    }
}

pub struct Providers {
    wm: Option<Adapter>,
    bayou: Option<Adapter>,
}
impl Providers {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        Ok(Self::with_clients(
            config.wm.as_ref().map(WasteManagement::new).transpose()?,
            config.bayou.as_ref().map(Bayou::new).transpose()?,
        ))
    }
    pub fn with_clients(wm: Option<WasteManagement>, bayou: Option<Bayou>) -> Self {
        Self {
            wm: wm.map(Adapter::Wm),
            bayou: bayou.map(Adapter::Bayou),
        }
    }
    fn get(&self, provider: LiveProvider) -> Option<&Adapter> {
        let adapter = match provider {
            LiveProvider::WasteManagement => self.wm.as_ref(),
            LiveProvider::Bayou => self.bayou.as_ref(),
        };
        adapter.filter(|adapter| adapter.provider() == provider)
    }
    pub(crate) fn configured(&self, provider: ProviderId) -> bool {
        LiveProvider::try_from(provider)
            .ok()
            .and_then(|provider| self.get(provider))
            .is_some()
    }
    pub(crate) fn bayou(&self) -> Option<&Bayou> {
        match self.bayou.as_ref() {
            Some(Adapter::Bayou(provider)) => Some(provider),
            _ => None,
        }
    }
    pub(crate) fn initial_coverage(
        &self,
        binding: &crate::config::AccountBinding,
    ) -> Vec<Coverage> {
        let adapter = LiveProvider::try_from(binding.provider)
            .ok()
            .and_then(|provider| self.get(provider));
        let info = self.info();
        ALL.iter()
            .map(|capability| {
                let supported = info
                    .iter()
                    .find(|provider| provider.id.issuer() == binding.provider)
                    .is_some_and(|provider| provider.capabilities.contains(capability));
                Coverage {
                    account_id: binding.id.clone(),
                    capability: *capability,
                    state: if !supported {
                        CoverageState::Unsupported
                    } else if adapter.is_none() {
                        CoverageState::NotConfigured
                    } else {
                        CoverageState::Unavailable
                    },
                    reason: Some(
                        if !supported {
                            "provider_not_supported"
                        } else if adapter.is_none() {
                            "credentials_not_configured"
                        } else {
                            "not_synced"
                        }
                        .to_owned(),
                    ),
                    checked_at: Utc::now(),
                    from: None,
                    to: None,
                }
            })
            .collect()
    }
    pub fn info(&self) -> Vec<ProviderInfo> {
        [LiveProvider::WasteManagement, LiveProvider::Bayou]
            .into_iter()
            .map(|id| ProviderInfo {
                id,
                name: id.name().to_owned(),
                capabilities: match id {
                    LiveProvider::WasteManagement => crate::utility::waste_management::CAPABILITIES,
                    LiveProvider::Bayou => crate::utility::bayou::CAPABILITIES,
                }
                .to_vec(),
                configured: self.get(id).is_some(),
            })
            .collect()
    }
}

#[derive(Clone)]
pub struct SyncHandle {
    store: Store,
    config: Arc<Config>,
    sender: mpsc::Sender<Work>,
}

#[derive(Clone, Copy)]
enum SyncRange {
    Dates { from: NaiveDate, to: NaiveDate },
    AllHistory,
}

struct Work {
    job: SyncJob,
    range: SyncRange,
}

impl SyncHandle {
    pub async fn request(&self, input: SyncInput) -> Result<SyncJob, StoreError> {
        if input.from > input.to || input.to.signed_duration_since(input.from).num_days() > 366 {
            return Err(StoreError::InvalidQuery);
        }
        self.request_range(
            &input.account,
            SyncRange::Dates {
                from: input.from,
                to: input.to,
            },
        )
        .await
    }

    pub async fn request_history(&self, input: HistorySyncInput) -> Result<SyncJob, StoreError> {
        self.request_range(&input.account, SyncRange::AllHistory)
            .await
    }

    async fn request_range(&self, account: &str, range: SyncRange) -> Result<SyncJob, StoreError> {
        self.store
            .binding(&self.config.deployment, account)
            .await
            .map_err(|error| {
                if matches!(error, StoreError::NotFound) {
                    StoreError::InvalidQuery
                } else {
                    error
                }
            })?;
        require_mode(
            self.store
                .account_ingestion(&self.config.deployment, account)
                .await?,
            IngestionMode::Live,
        )
        .map_err(|_| StoreError::InvalidQuery)?;
        let job = self.store.start_job(account).await?;
        self.enqueue(job, range).await
    }

    pub(crate) async fn enqueue_first_syncs(&self, jobs: Vec<SyncJob>) -> Result<(), StoreError> {
        let mut result = Ok(());
        for job in jobs {
            // Binding and job creation already committed. A full queue records
            // a failed job; it cannot undo the account or hide the failure.
            if let Err(error) = self.enqueue(job, SyncRange::AllHistory).await
                && !matches!(error, StoreError::Busy)
            {
                result = Err(error);
            }
        }
        result
    }

    async fn enqueue(&self, job: SyncJob, range: SyncRange) -> Result<SyncJob, StoreError> {
        if self
            .sender
            .try_send(Work {
                job: job.clone(),
                range,
            })
            .is_err()
        {
            self.store
                .finish_job(
                    &job.id,
                    SyncState::Failed,
                    0,
                    Some("sync_queue_unavailable"),
                )
                .await?;
            return Err(StoreError::Busy);
        }
        Ok(job)
    }
}

pub struct SyncWorker {
    store: Store,
    config: Arc<Config>,
    providers: Arc<Providers>,
    receiver: mpsc::Receiver<Work>,
    automatic: bool,
}
impl SyncWorker {
    pub async fn new(
        store: Store,
        config: Arc<Config>,
        providers: Arc<Providers>,
    ) -> Result<(SyncHandle, Self), StoreError> {
        store.recover_interrupted().await?;
        let existing = store.coverage().await?;
        let mut initial = Vec::new();
        let bindings = store.bindings(&config.deployment).await?;
        for account in &bindings {
            if store
                .account_ingestion(&config.deployment, &account.id)
                .await?
                != IngestionMode::Live
            {
                continue;
            }
            for coverage in providers.initial_coverage(account) {
                if matches!(
                    coverage.state,
                    CoverageState::Unsupported | CoverageState::NotConfigured
                ) || !existing
                    .iter()
                    .any(|c| c.account_id == account.id && c.capability == coverage.capability)
                {
                    initial.push(coverage);
                }
            }
        }
        store.set_coverage(&initial).await?;
        // One accepted job per bound account, including a multi-account bind.
        let (sender, receiver) = mpsc::channel(100);
        Ok((
            SyncHandle {
                store: store.clone(),
                config: config.clone(),
                sender,
            },
            Self {
                store,
                config,
                providers,
                receiver,
                automatic: false,
            },
        ))
    }

    pub fn with_automatic_sync(mut self) -> Self {
        self.automatic = true;
        self
    }

    async fn next_automatic(&self, today: NaiveDate) -> Result<Option<Work>, StoreError> {
        let candidates = self
            .store
            .automatic_candidates(&self.config.deployment, today)
            .await?;
        for (account, first) in candidates {
            if !self.providers.configured(account.provider) {
                continue;
            }
            let range = if first {
                SyncRange::AllHistory
            } else {
                let from = today
                    .checked_sub_days(Days::new(7))
                    .ok_or(StoreError::InvalidQuery)?;
                let to = today.pred_opt().ok_or(StoreError::InvalidQuery)?;
                SyncRange::Dates { from, to }
            };
            match self
                .store
                .start_automatic_job(&account.id, today, first)
                .await
            {
                Ok(job) => return Ok(Some(Work { job, range })),
                // A manual request or another automatic admission won the race.
                Err(StoreError::Busy) => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(None)
    }

    pub async fn run(mut self, mut stop: watch::Receiver<bool>) -> Result<(), StoreError> {
        let mut schedule = tokio::time::interval(Duration::from_secs(60));
        schedule.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let next = tokio::select! {
                biased;
                _=stop.changed()=>None,
                next=self.receiver.recv()=>next,
                _=schedule.tick(), if self.automatic => {
                    let Some(work) = self.next_automatic(Utc::now().date_naive()).await? else { continue };
                    Some(work)
                },
            };
            let Some(Work { job, range }) = next else {
                break;
            };
            let result = tokio::select! {
                _=stop.changed()=> Err("process_interrupted"),
                result=tokio::time::timeout(Duration::from_secs(600),self.execute(&job, range))=>match result {Ok(result)=>result,Err(_)=>Err("sync_timeout")},
            };
            if let Err(reason) = result {
                let interrupted = reason == "process_interrupted";
                self.fail(&job, reason, interrupted).await?;
                if interrupted {
                    break;
                }
            }
        }
        // Accepted requests that were queued still have a durable terminal outcome.
        while let Ok(Work { job, .. }) = self.receiver.try_recv() {
            self.fail(&job, "process_interrupted", true).await?;
        }
        Ok(())
    }

    async fn execute(&self, job: &SyncJob, range: SyncRange) -> Result<(), &'static str> {
        let binding = self
            .store
            .binding(&self.config.deployment, &job.account_id)
            .await
            .map_err(|_| "binding_mismatch")?;
        require_mode(
            self.store
                .account_ingestion(&self.config.deployment, &binding.id)
                .await
                .map_err(|_| "binding_mismatch")?,
            IngestionMode::Live,
        )
        .map_err(|_| "binding_mismatch")?;
        let provider = LiveProvider::try_from(binding.provider).map_err(|_| "binding_mismatch")?;
        let stage = match range {
            SyncRange::Dates { .. } => SyncStage::DateRange,
            SyncRange::AllHistory => SyncStage::History,
        };
        let adapter = self.providers.get(provider).ok_or_else(|| {
            report_failure(provider, stage, &UtilityError::NotConfigured);
            "credentials_not_configured"
        })?;
        let mut continuation = None;
        let mut seen = std::collections::HashSet::new();
        let observed_at = Utc::now();
        let mut partial = false;
        // A year of daily pages for each bounded service plus billing pages.
        for _ in 0..25_000 {
            let mut page = match range {
                SyncRange::Dates { from, to } => {
                    adapter
                        .sync(SyncRequest {
                            binding: binding.clone(),
                            from,
                            to,
                            continuation,
                            observed_at,
                        })
                        .await
                }
                SyncRange::AllHistory => {
                    adapter
                        .sync_history(HistoryRequest {
                            binding: binding.clone(),
                            continuation,
                            observed_at,
                        })
                        .await
                }
            }
            .map_err(|error| {
                report_failure(provider, stage, &error);
                error.code()
            })?;
            if page.observations.len() > 20_000 {
                return Err("invalid_response");
            }
            for coverage in &mut page.coverage {
                if page.continuation.is_some() && coverage.state == CoverageState::Complete {
                    coverage.state = CoverageState::Partial;
                    coverage.reason = Some("sync_in_progress".to_owned());
                }
            }
            partial |= page.coverage.iter().any(|c| {
                matches!(
                    c.state,
                    CoverageState::Partial
                        | CoverageState::Unavailable
                        | CoverageState::NotConfigured
                ) && c.reason.as_deref() != Some("sync_in_progress")
            });
            self.store
                .ingest_job(&binding, &page, &job.id)
                .await
                .map_err(|_| "ingest_rejected")?;
            match page.continuation {
                None => {
                    let progress = self
                        .store
                        .job(&job.id)
                        .await
                        .map_err(|_| "database_error")?;
                    self.store
                        .finish_job(
                            &job.id,
                            if partial {
                                SyncState::Partial
                            } else {
                                SyncState::Complete
                            },
                            progress.observations,
                            None,
                        )
                        .await
                        .map_err(|_| "database_error")?;
                    return Ok(());
                }
                Some(cursor) => {
                    if !seen.insert(cursor.0.clone()) {
                        return Err("invalid_continuation");
                    }
                    continuation = Some(cursor);
                }
            }
        }
        Err("sync_page_budget_exceeded")
    }

    async fn fail(
        &self,
        job: &SyncJob,
        reason: &'static str,
        interrupted: bool,
    ) -> Result<(), StoreError> {
        let progress = self
            .store
            .job(&job.id)
            .await
            .inspect_err(|_| report_terminal_failure(None, reason))?;
        // Shutdown can win the select after the final transaction committed.
        if progress.state != SyncState::Running {
            return Ok(());
        }
        let account = self
            .store
            .binding(&self.config.deployment, &job.account_id)
            .await;
        let provider = account
            .as_ref()
            .ok()
            .and_then(|binding| LiveProvider::try_from(binding.provider).ok());
        report_terminal_failure(provider, reason);
        let account = account?;
        let capabilities = self
            .providers
            .info()
            .into_iter()
            .find(|p| p.id.issuer() == account.provider)
            .ok_or(StoreError::InvalidObservation)?
            .capabilities;
        self.store
            .set_coverage(
                &capabilities
                    .into_iter()
                    .map(|capability| Coverage {
                        account_id: account.id.clone(),
                        capability,
                        state: if reason == "credentials_not_configured" {
                            CoverageState::NotConfigured
                        } else {
                            CoverageState::Unavailable
                        },
                        reason: Some(reason.to_owned()),
                        checked_at: Utc::now(),
                        from: None,
                        to: None,
                    })
                    .collect::<Vec<_>>(),
            )
            .await?;
        self.store
            .finish_job(
                &job.id,
                if interrupted {
                    SyncState::Interrupted
                } else {
                    SyncState::Failed
                },
                progress.observations,
                Some(reason),
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use chrono::{Days, NaiveDate, Utc};
    use serde::Serialize;
    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{oneshot, watch},
        task::JoinHandle,
    };

    use crate::{
        config::{AccountBinding, BayouConfig, Config, ServiceBinding},
        models::{Bill, ListQuery, ProviderId, ServiceKind, SyncInput, SyncJob, SyncState},
        store::{Store, StoreError},
        sync::{Adapter, Providers, SyncHandle, SyncRange, SyncWorker},
        utility::bayou::Bayou,
    };

    const ACCOUNT: &str = "demo-energy";
    const DEPLOYMENT: &str = "fictional-installation";
    const PROVIDER_ACCOUNT: &str = "fictional-account";
    const METER: &str = "fictional-electric-meter";

    #[derive(Clone, Copy)]
    enum LaterPage {
        AuthenticationFailure,
        Pending,
        Success,
    }

    #[derive(Serialize)]
    struct CustomerResponse {
        id: u64,
        is_currently_authenticated: bool,
        auth_status: &'static str,
        bills_are_ready: bool,
        utility_has_intervals: bool,
        account_has_intervals: bool,
        intervals_are_ready: bool,
        account_numbers: Vec<AccountResponse>,
    }

    #[derive(Serialize)]
    struct AccountResponse {
        id: &'static str,
        meters: Vec<MeterResponse>,
    }

    #[derive(Serialize)]
    struct MeterResponse {
        id: &'static str,
        #[serde(rename = "type")]
        kind: &'static str,
    }

    #[derive(Serialize)]
    struct BillResponse {
        id: u64,
        customer_id: u64,
        account_number: &'static str,
        status: &'static str,
        billed_on: &'static str,
        billing_period_from: &'static str,
        billing_period_to: &'static str,
        total_amount: i64,
        outstanding_balance: i64,
        meters: Vec<MeterResponse>,
    }

    fn meter() -> MeterResponse {
        MeterResponse {
            id: METER,
            kind: "electric",
        }
    }

    fn customer_body() -> String {
        serde_json::to_string(&CustomerResponse {
            id: 71001,
            is_currently_authenticated: true,
            auth_status: "authenticated",
            bills_are_ready: true,
            utility_has_intervals: false,
            account_has_intervals: false,
            intervals_are_ready: false,
            account_numbers: vec![AccountResponse {
                id: PROVIDER_ACCOUNT,
                meters: vec![meter()],
            }],
        })
        .unwrap()
    }

    fn bill_body() -> String {
        serde_json::to_string(&vec![BillResponse {
            id: 83001,
            customer_id: 71001,
            account_number: PROVIDER_ACCOUNT,
            status: "unlocked",
            billed_on: "2026-10-01",
            billing_period_from: "2026-09-01",
            billing_period_to: "2026-09-30",
            total_amount: 12050,
            outstanding_balance: 4500,
            meters: vec![meter()],
        }])
        .unwrap()
    }

    struct WorkerFixture {
        files: TempDir,
        store: Store,
        config: Arc<Config>,
        providers: Arc<Providers>,
        handle: SyncHandle,
        stop: watch::Sender<bool>,
        worker: Option<JoinHandle<Result<(), StoreError>>>,
        http: JoinHandle<()>,
        later_page: Option<oneshot::Receiver<()>>,
    }

    impl WorkerFixture {
        async fn start(later: LaterPage, hosted: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = url::Url::parse(&format!(
                "http://{}/api/v2/",
                listener.local_addr().unwrap()
            ))
            .unwrap();
            let (later_tx, later_rx) = oneshot::channel();
            let http = tokio::spawn(async move {
                let mut customer_reads = 0;
                let mut later_tx = Some(later_tx);
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                        let read = stream.read(&mut chunk).await.unwrap();
                        assert_ne!(read, 0);
                        bytes.extend_from_slice(&chunk[..read]);
                        assert!(bytes.len() < 16 * 1024);
                    }
                    let request = String::from_utf8(bytes).unwrap();
                    let first = request.lines().next().unwrap();
                    assert!(first.starts_with("GET "));
                    let path = first.split_whitespace().nth(1).unwrap();
                    let (status, body) = if path == "/api/v2/customers/71001" {
                        customer_reads += 1;
                        if customer_reads > 1 {
                            // This request cannot start until the worker's first
                            // page transaction has committed. Hold the actual
                            // upstream response while shutdown/restart is tested.
                            if let Some(sender) = later_tx.take() {
                                let _ = sender.send(());
                            }
                            match later {
                                LaterPage::AuthenticationFailure => (401, String::new()),
                                LaterPage::Pending => {
                                    std::future::pending::<()>().await;
                                    unreachable!()
                                }
                                LaterPage::Success => (200, customer_body()),
                            }
                        } else {
                            (200, customer_body())
                        }
                    } else if path.starts_with("/api/v2/customers/71001/bills?") {
                        if path.contains("start=2026-10-01") {
                            (200, bill_body())
                        } else {
                            (200, "[]".to_owned())
                        }
                    } else {
                        panic!("unexpected provider fixture path: {path}");
                    };
                    let response = format!(
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                }
            });
            let files = tempfile::tempdir().unwrap();
            let key = files.path().join("api-key");
            std::fs::write(&key, "fictional-sync-test-key").unwrap();
            let bayou_config = BayouConfig { api_key_file: key };
            let config = Arc::new(Config {
                deployment: DEPLOYMENT.to_owned(),
                accounts: vec![AccountBinding {
                    id: ACCOUNT.to_owned(),
                    deployment: DEPLOYMENT.to_owned(),
                    provider: ProviderId::Bayou,
                    currency: "USD".to_owned(),
                    customer_id: "71001".to_owned(),
                    provider_account: PROVIDER_ACCOUNT.to_owned(),
                    services: vec![ServiceBinding {
                        id: "demo-electric".to_owned(),
                        provider_service: METER.to_owned(),
                        kind: ServiceKind::Electricity,
                    }],
                }],
                wm: None,
                bayou: Some(bayou_config.clone()),
            });
            let providers = Arc::new(Providers {
                wm: None,
                bayou: Some(Adapter::Bayou(
                    Bayou::with_endpoint(&bayou_config, endpoint).unwrap(),
                )),
            });
            let store = Store::open(&files.path().join("utilities.sqlite3"))
                .await
                .unwrap();
            store.register(&config).await.unwrap();
            let config = if hosted {
                let mut current = (*config).clone();
                let binding = current.accounts.remove(0);
                store.register(&current).await.unwrap();
                let input = crate::models::CreateConnection {
                    provider: crate::models::ConnectionProvider::Bayou,
                    request_id: uuid::Uuid::now_v7().to_string(),
                    utility_id: "speculoos_power".into(),
                    currency: "USD".into(),
                    accept_provider_settings: true,
                };
                let (record, _) = store
                    .create_connection(DEPLOYMENT, &input, "Speculoos Power")
                    .await
                    .unwrap();
                let mut progress = record.connection.progress.clone();
                progress.customer_id = Some("71001".into());
                progress.state = crate::models::ConnectionState::Ready;
                store
                    .save_connection_progress(DEPLOYMENT, &record, &progress, true)
                    .await
                    .unwrap();
                let record = store
                    .connection(DEPLOYMENT, &record.connection.id)
                    .await
                    .unwrap();
                store
                    .bind_connection(&current, &record, &[binding], &[])
                    .await
                    .unwrap();
                Arc::new(current)
            } else {
                config
            };
            let (handle, worker) =
                SyncWorker::new(store.clone(), config.clone(), providers.clone())
                    .await
                    .unwrap();
            let (stop, rx) = watch::channel(false);
            let worker = Some(tokio::spawn(worker.run(rx)));
            Self {
                files,
                store,
                config,
                providers,
                handle,
                stop,
                worker,
                http,
                later_page: Some(later_rx),
            }
        }

        async fn request(&self) -> SyncJob {
            self.handle
                .request(SyncInput {
                    account: ACCOUNT.to_owned(),
                    from: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                    to: NaiveDate::from_ymd_opt(2026, 10, 2).unwrap(),
                })
                .await
                .unwrap()
        }

        async fn await_later_page(&mut self) {
            tokio::time::timeout(Duration::from_secs(5), self.later_page.take().unwrap())
                .await
                .unwrap()
                .unwrap();
        }

        async fn terminal_job(&self, id: &str) -> SyncJob {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let job = self.store.job(id).await.unwrap();
                    if job.state != SyncState::Running {
                        return job;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap()
        }

        async fn assert_saved_bill(&self) {
            let page = self
                .store
                .list::<Bill>("bill", &ListQuery::default(), &[ACCOUNT.to_owned()])
                .await
                .unwrap();
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].total.minor_units, 12050);
        }

        async fn shutdown(&mut self) {
            self.stop.send(true).unwrap();
            tokio::time::timeout(Duration::from_secs(5), self.worker.take().unwrap())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
    }

    impl Drop for WorkerFixture {
        fn drop(&mut self) {
            if let Some(worker) = self.worker.take() {
                worker.abort();
            }
            self.http.abort();
        }
    }

    #[tokio::test]
    async fn committed_page_count_survives_later_authentication_failure() {
        let mut fixture = WorkerFixture::start(LaterPage::AuthenticationFailure, false).await;
        let job = fixture.request().await;
        let result = fixture.terminal_job(&job.id).await;
        fixture.assert_saved_bill().await;
        assert_eq!(result.state, SyncState::Failed);
        assert_eq!(result.error.as_deref(), Some("authentication_required"));
        // One Bill and its validated Service were processed in the first page.
        assert_eq!(result.observations, 2);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn graceful_shutdown_preserves_committed_page_progress() {
        let mut fixture = WorkerFixture::start(LaterPage::Pending, false).await;
        let job = fixture.request().await;
        fixture.await_later_page().await;
        fixture.assert_saved_bill().await;
        fixture.shutdown().await;
        let result = fixture.store.job(&job.id).await.unwrap();
        assert_eq!(result.state, SyncState::Interrupted);
        assert_eq!(result.error.as_deref(), Some("process_interrupted"));
        assert_eq!(result.observations, 2);
    }

    #[tokio::test]
    async fn restart_recovers_initial_history_committed_before_enqueue() {
        // The hosted fixture binds the account before starting the worker.
        // No in-memory work survives, which is the bind/queue crash window.
        let mut fixture = WorkerFixture::start(LaterPage::Success, true).await;
        let interrupted = fixture
            .store
            .latest_jobs(DEPLOYMENT)
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(interrupted.state, SyncState::Interrupted);
        assert_eq!(interrupted.observations, 0);
        fixture.shutdown().await;
        let (_handle, worker) = SyncWorker::new(
            fixture.store.clone(),
            fixture.config.clone(),
            fixture.providers.clone(),
        )
        .await
        .unwrap();
        let today = Utc::now().date_naive();
        let work = worker
            .next_automatic(today)
            .await
            .unwrap()
            .expect("restart lost the pending first history sync");
        assert_ne!(work.job.id, interrupted.id);
        assert!(matches!(work.range, SyncRange::AllHistory));
        worker.execute(&work.job, work.range).await.unwrap();
        let job = fixture.store.job(&work.job.id).await.unwrap();
        assert!(matches!(
            job.state,
            SyncState::Complete | SyncState::Partial
        ));
        let coverage = fixture.store.coverage().await.unwrap();
        worker
            .fail(&job, "process_interrupted", true)
            .await
            .unwrap();
        assert_eq!(fixture.store.job(&job.id).await.unwrap(), job);
        assert_eq!(fixture.store.coverage().await.unwrap(), coverage);
        assert!(worker.next_automatic(today).await.unwrap().is_none());
        let tomorrow = today.succ_opt().unwrap();
        let daily = worker.next_automatic(tomorrow).await.unwrap().unwrap();
        assert!(
            matches!(daily.range, SyncRange::Dates { from, to } if from == tomorrow - Days::new(7) && to == today)
        );
    }

    #[tokio::test]
    async fn restart_recovers_committed_page_progress_after_worker_loss() {
        let mut fixture = WorkerFixture::start(LaterPage::Pending, false).await;
        let job = fixture.request().await;
        fixture.await_later_page().await;
        fixture.assert_saved_bill().await;
        let worker = fixture.worker.take().unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        fixture.store.pool().close().await;

        let reopened = Store::open(&fixture.files.path().join("utilities.sqlite3"))
            .await
            .unwrap();
        let (_handle, _worker) = SyncWorker::new(
            reopened.clone(),
            fixture.config.clone(),
            fixture.providers.clone(),
        )
        .await
        .unwrap();
        let result = reopened.job(&job.id).await.unwrap();
        assert_eq!(result.state, SyncState::Interrupted);
        assert_eq!(result.error.as_deref(), Some("process_interrupted"));
        assert_eq!(result.observations, 2);
        reopened.pool().close().await;
    }

    #[tokio::test]
    async fn successful_paged_sync_finishes_with_durable_processed_count() {
        let mut fixture = WorkerFixture::start(LaterPage::Success, false).await;
        let job = fixture.request().await;
        let result = fixture.terminal_job(&job.id).await;
        fixture.assert_saved_bill().await;
        assert_eq!(result.state, SyncState::Partial);
        assert!(result.error.is_none());
        // The second, empty billing page still validates its Service.
        assert_eq!(result.observations, 3);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn hosted_account_failure_finishes_durably_and_keeps_accepting_jobs() {
        let mut fixture = WorkerFixture::start(LaterPage::AuthenticationFailure, true).await;
        assert!(fixture.config.accounts.is_empty());
        assert_eq!(fixture.store.bindings(DEPLOYMENT).await.unwrap().len(), 1);
        let job = fixture.request().await;
        let failed = fixture.terminal_job(&job.id).await;
        assert_eq!(failed.state, SyncState::Failed);
        assert_eq!(failed.error.as_deref(), Some("authentication_required"));
        assert_eq!(failed.observations, 2);
        fixture.assert_saved_bill().await;
        let coverage = fixture.store.coverage().await.unwrap();
        assert!(coverage.iter().any(|item| item.account_id == ACCOUNT
            && item.reason.as_deref() == Some("authentication_required")));
        let next = fixture.request().await;
        assert_ne!(job.id, next.id);
        let next = fixture.terminal_job(&next.id).await;
        assert_eq!(next.state, SyncState::Failed);
        assert_eq!(next.error.as_deref(), Some("authentication_required"));
        fixture.shutdown().await;
    }
}
