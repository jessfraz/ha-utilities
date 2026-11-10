//! Run the real connection API against a fictional, loopback-only Bayou server.
//! Browser tests own both this process and the temporary database.
use ha_utilities::{
    api,
    config::{BayouConfig, Config},
    connect::ConnectionWorker,
    store::Store,
    sync::{Providers, SyncWorker},
    utility::bayou::Bayou,
};
use std::{path::PathBuf, sync::Arc};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    anyhow::ensure!(
        args.len() == 4,
        "expected database, key file, bind address, fixture URL"
    );
    let endpoint = url::Url::parse(&args[3])?;
    anyhow::ensure!(
        endpoint.scheme() == "http" && endpoint.host_str() == Some("127.0.0.1"),
        "fixture endpoint must be loopback"
    );
    let bind: std::net::SocketAddr = args[2].parse()?;
    anyhow::ensure!(bind.ip().is_loopback(), "fixture bind must be loopback");
    let config = Arc::new(Config {
        deployment: "fictional-connection-fixture".into(),
        accounts: Vec::new(),
        wm: None,
        bayou: Some(BayouConfig {
            api_key_file: PathBuf::from(&args[1]),
        }),
    });
    let store = Store::open(std::path::Path::new(&args[0])).await?;
    store.register(&config).await?;
    let bayou = config
        .bayou
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing fixture key"))?;
    let providers = Arc::new(Providers::with_clients(
        None,
        Some(Bayou::with_endpoint(bayou, endpoint)?),
    ));
    let (sync, worker) = SyncWorker::new(store.clone(), config.clone(), providers.clone()).await?;
    let (connections, connection_worker) =
        ConnectionWorker::new(store.clone(), config.clone(), providers.clone()).await?;
    let server = api::start(
        bind,
        Arc::new(api::Context::new(
            store.clone(),
            config,
            providers,
            sync,
            connections,
            "fictional-utilities-browser-token-not-a-real-credential",
        )),
    )?;
    let (stop, rx) = tokio::sync::watch::channel(false);
    let sync_task = tokio::spawn(worker.run(rx.clone()));
    let connection_task = tokio::spawn(connection_worker.run(rx));
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    server.close().await.map_err(anyhow::Error::msg)?;
    stop.send_replace(true);
    sync_task.await??;
    connection_task.await??;
    store.pool().close().await;
    Ok(())
}
