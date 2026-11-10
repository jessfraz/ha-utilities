//! A single owned worker runs bounded read-only syncs and is joined before SQLite closes.
use crate::{
    config::Config,
    models::*,
    store::{Store, StoreError},
    utility::{
        SyncRequest, Utility, UtilityError, bayou::Bayou, waste_management::WasteManagement,
    },
};
use chrono::Utc;
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
    async fn sync(&self, request: SyncRequest) -> Result<crate::utility::SyncPage, UtilityError> {
        match self {
            Self::Wm(adapter) => adapter.sync(request).await,
            Self::Bayou(adapter) => adapter.sync(request).await,
        }
    }
    fn capabilities(&self) -> &'static [Capability] {
        match self {
            Self::Wm(adapter) => adapter.capabilities(),
            Self::Bayou(adapter) => adapter.capabilities(),
        }
    }
}

pub struct Providers {
    wm: Option<Adapter>,
    bayou: Option<Adapter>,
}
impl Providers {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            wm: config
                .wm
                .as_ref()
                .map(WasteManagement::new)
                .transpose()?
                .map(Adapter::Wm),
            bayou: config
                .bayou
                .as_ref()
                .map(Bayou::new)
                .transpose()?
                .map(Adapter::Bayou),
        })
    }
    fn get(&self, provider: ProviderId) -> Option<&Adapter> {
        match provider {
            ProviderId::WasteManagement => self.wm.as_ref(),
            ProviderId::Bayou => self.bayou.as_ref(),
        }
    }
    pub fn info(&self) -> Vec<ProviderInfo> {
        [ProviderId::WasteManagement, ProviderId::Bayou]
            .into_iter()
            .map(|id| ProviderInfo {
                id,
                name: match id {
                    ProviderId::WasteManagement => "Waste Management",
                    ProviderId::Bayou => "Bayou",
                }
                .to_owned(),
                capabilities: match id {
                    ProviderId::WasteManagement => vec![
                        Capability::Bills,
                        Capability::Balances,
                        Capability::Payments,
                        Capability::Services,
                        Capability::Collections,
                    ],
                    ProviderId::Bayou => vec![
                        Capability::Bills,
                        Capability::Balances,
                        Capability::Services,
                        Capability::Usage,
                    ],
                },
                configured: self.get(id).is_some(),
            })
            .collect()
    }
}

#[derive(Clone)]
pub struct SyncHandle {
    store: Store,
    config: Arc<Config>,
    sender: mpsc::Sender<(SyncJob, SyncInput)>,
}
impl SyncHandle {
    pub async fn request(&self, input: SyncInput) -> Result<SyncJob, StoreError> {
        if input.from > input.to
            || input.to.signed_duration_since(input.from).num_days() > 366
            || !self.config.accounts.iter().any(|a| a.id == input.account)
        {
            return Err(StoreError::InvalidQuery);
        }
        let job = self.store.start_job(&input.account).await?;
        if self.sender.try_send((job.clone(), input)).is_err() {
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
    receiver: mpsc::Receiver<(SyncJob, SyncInput)>,
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
        for account in &config.accounts {
            for capability in ALL {
                let adapter = providers.get(account.provider);
                let supported = adapter
                    .map(|a| a.capabilities().contains(capability))
                    .unwrap_or_else(|| {
                        providers
                            .info()
                            .iter()
                            .find(|p| p.id == account.provider)
                            .is_some_and(|p| p.capabilities.contains(capability))
                    });
                if !supported
                    || adapter.is_none()
                    || !existing
                        .iter()
                        .any(|c| c.account_id == account.id && c.capability == *capability)
                {
                    initial.push(Coverage {
                        account_id: account.id.clone(),
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
                    });
                }
            }
        }
        store.set_coverage(&initial).await?;
        let (sender, receiver) = mpsc::channel(32);
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
            },
        ))
    }

    pub async fn run(mut self, mut stop: watch::Receiver<bool>) -> Result<(), StoreError> {
        loop {
            let next = tokio::select! { _=stop.changed()=>None, next=self.receiver.recv()=>next };
            let Some((job, input)) = next else { break };
            let result = tokio::select! {
                _=stop.changed()=> Err("process_interrupted"),
                result=tokio::time::timeout(Duration::from_secs(600),self.execute(&job,&input))=>match result {Ok(result)=>result,Err(_)=>Err("sync_timeout")},
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
        while let Ok((job, _)) = self.receiver.try_recv() {
            self.fail(&job, "process_interrupted", true).await?;
        }
        Ok(())
    }

    async fn execute(&self, job: &SyncJob, input: &SyncInput) -> Result<(), &'static str> {
        let binding = self
            .config
            .accounts
            .iter()
            .find(|a| a.id == input.account)
            .ok_or("binding_mismatch")?;
        let adapter = self
            .providers
            .get(binding.provider)
            .ok_or("credentials_not_configured")?;
        let mut continuation = None;
        let mut seen = std::collections::HashSet::new();
        let observed_at = Utc::now();
        let mut partial = false;
        // A year of daily pages for each bounded service plus billing pages.
        for _ in 0..25_000 {
            let mut page = adapter
                .sync(SyncRequest {
                    binding: binding.clone(),
                    from: input.from,
                    to: input.to,
                    continuation,
                    observed_at,
                })
                .await
                .map_err(error_code)?;
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
                .ingest_job(binding, &page, &job.id)
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

    async fn fail(&self, job: &SyncJob, reason: &str, interrupted: bool) -> Result<(), StoreError> {
        let account = self
            .config
            .accounts
            .iter()
            .find(|a| a.id == job.account_id)
            .ok_or(StoreError::InvalidObservation)?;
        let capabilities = self
            .providers
            .info()
            .into_iter()
            .find(|p| p.id == account.provider)
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
        let progress = self.store.job(&job.id).await?;
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

fn error_code(error: UtilityError) -> &'static str {
    match error {
        UtilityError::NotConfigured => "credentials_not_configured",
        UtilityError::Authentication => "authentication_required",
        UtilityError::AccessDenied => "access_denied",
        UtilityError::RateLimited => "rate_limited",
        UtilityError::Unavailable => "provider_unavailable",
        UtilityError::InvalidResponse => "invalid_response",
        UtilityError::BindingMismatch => "binding_mismatch",
        UtilityError::InvalidContinuation => "invalid_continuation",
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use chrono::NaiveDate;
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
        sync::{Adapter, Providers, SyncHandle, SyncWorker},
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
        async fn start(later: LaterPage) -> Self {
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
                    name: "Example energy".to_owned(),
                    deployment: DEPLOYMENT.to_owned(),
                    provider: ProviderId::Bayou,
                    currency: "USD".to_owned(),
                    customer_id: "71001".to_owned(),
                    provider_account: PROVIDER_ACCOUNT.to_owned(),
                    services: vec![ServiceBinding {
                        id: "demo-electric".to_owned(),
                        name: "Electricity".to_owned(),
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
        let mut fixture = WorkerFixture::start(LaterPage::AuthenticationFailure).await;
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
        let mut fixture = WorkerFixture::start(LaterPage::Pending).await;
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
    async fn restart_recovers_committed_page_progress_after_worker_loss() {
        let mut fixture = WorkerFixture::start(LaterPage::Pending).await;
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
        let mut fixture = WorkerFixture::start(LaterPage::Success).await;
        let job = fixture.request().await;
        let result = fixture.terminal_job(&job.id).await;
        fixture.assert_saved_bill().await;
        assert_eq!(result.state, SyncState::Partial);
        assert!(result.error.is_none());
        // The second, empty billing page still validates its Service.
        assert_eq!(result.observations, 3);
        fixture.shutdown().await;
    }
}
