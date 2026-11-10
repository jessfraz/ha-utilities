#![allow(dead_code)]
use chrono::{Duration, Utc};
use ha_utilities::{
    api,
    config::Config,
    models::*,
    store::Store,
    sync::{Providers, SyncWorker},
    utility::SyncPage,
};
use std::{path::Path, sync::Arc};

pub const TOKEN: &str = "fictional-test-credential-12345";
pub fn config() -> Config {
    toml::from_str(include_str!("../../config/example.toml")).unwrap()
}
pub async fn populated(path: &Path) -> Store {
    let config = config();
    let store = Store::open(path).await.unwrap();
    store.register(&config).await.unwrap();
    let now = Utc::now();
    let binding = &config.accounts[0];
    let observations = (0..5)
        .map(|index| {
            Observation::Bill(Bill {
                id: format!("demo-wm:bill:{index}"),
                account_id: binding.id.clone(),
                provider: binding.provider,
                issued_on: now.date_naive() - Duration::days(index),
                due_on: None,
                period_from: None,
                period_to: None,
                total: Money {
                    minor_units: 12500 + index,
                    currency: "USD".to_owned(),
                },
                outstanding_balance: None,
                source_status: None,
                observed_at: now,
            })
        })
        .collect();
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
    store
}
pub struct App {
    pub url: String,
    pub store: Store,
    pub config: Arc<Config>,
    server: Option<dropshot::HttpServer<Arc<api::Context>>>,
    stop: tokio::sync::watch::Sender<bool>,
    worker: Option<tokio::task::JoinHandle<Result<(), ha_utilities::store::StoreError>>>,
    pub directory: tempfile::TempDir,
}
impl App {
    pub async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = populated(&directory.path().join("test.sqlite3")).await;
        let config = Arc::new(config());
        let providers = Arc::new(Providers::new(&config).unwrap());
        let (sync, worker) = SyncWorker::new(store.clone(), config.clone(), providers.clone())
            .await
            .unwrap();
        let context = Arc::new(api::Context::new(
            store.clone(),
            config.clone(),
            providers,
            sync,
            TOKEN,
        ));
        let server = api::start("127.0.0.1:0".parse().unwrap(), context).unwrap();
        let url = format!("http://{}", server.local_addr());
        let (stop, rx) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(worker.run(rx));
        Self {
            url,
            store,
            config,
            server: Some(server),
            stop,
            worker: Some(worker),
            directory,
        }
    }
    pub async fn close(mut self) {
        if let Some(server) = self.server.take() {
            server.close().await.unwrap();
        }
        let _ = self.stop.send(true);
        if let Some(worker) = self.worker.take() {
            worker.await.unwrap().unwrap();
        }
        self.store.pool().close().await;
    }
}
impl Drop for App {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}
