use clap::Parser;
use ha_utilities::{
    api,
    config::{Config, read_secret},
    store::Store,
    sync::{Providers, SyncWorker},
};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};

#[derive(Parser)]
#[command(
    version,
    about = "Utilities local service. Provider access is read-only."
)]
struct Options {
    #[arg(long)]
    options: Option<PathBuf>,
    #[arg(long, env = "UTILITIES_CONFIG", default_value = "/data/config.toml")]
    config: PathBuf,
    #[arg(
        long,
        env = "UTILITIES_DATABASE",
        default_value = "/data/utilities.sqlite3"
    )]
    database: PathBuf,
    #[arg(long, env = "UTILITIES_TOKEN_FILE", default_value = "/data/api-token")]
    token_file: PathBuf,
    #[arg(long, env = "UTILITIES_BIND", default_value = "127.0.0.1:8080")]
    bind: SocketAddr,
    #[arg(long)]
    ingress_bind: Option<SocketAddr>,
    #[arg(long, default_value = "172.30.32.2")]
    ingress_peer: IpAddr,
    #[arg(long)]
    openapi: Option<PathBuf>,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = Options::parse();
    if let Some(path) = options.openapi {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&api::openapi()?)?)?;
        return Ok(());
    }
    let config = Config::load(&options.config, options.options.as_deref())?;
    initialize_token(&options.token_file)?;
    let token = read_secret(&options.token_file)?;
    anyhow::ensure!(
        token.len() >= 16
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b)),
        "API token must contain at least 16 cookie-safe characters"
    );
    let config = Arc::new(config);
    let providers = Arc::new(Providers::new(&config)?);
    let store = Store::open(&options.database).await?;
    store.register(&config).await?;
    let (handle, worker) =
        SyncWorker::new(store.clone(), config.clone(), providers.clone()).await?;
    let context = api::Context::new(store.clone(), config, providers, handle, &token);
    let server = api::start(options.bind, Arc::new(context.clone()))?;
    let ingress = options
        .ingress_bind
        .map(|bind| api::start(bind, Arc::new(context.for_ingress(options.ingress_peer))))
        .transpose()?;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let worker = tokio::spawn(worker.run(stop_rx.clone()));
    let discovery = if let Some(supervisor) =
        ha_utilities::supervisor::Supervisor::from_environment()?
    {
        let mut stop = stop_rx;
        let credential = token;
        Some(tokio::spawn(async move {
            loop {
                tokio::select! {_=stop.changed()=>break,_=supervisor.announce(&credential)=>{}}
                tokio::select! {_=stop.changed()=>break,_=tokio::time::sleep(std::time::Duration::from_secs(60))=>{}}
            }
        }))
    } else {
        None
    };
    shutdown().await?;
    let api_closed = server.close().await;
    let ingress_closed = if let Some(server) = ingress {
        server
            .close()
            .await
            .map_err(|_| anyhow::anyhow!("ingress shutdown failed"))
    } else {
        Ok(())
    };
    let _ = stop_tx.send(true);
    let worker_result = worker.await;
    let discovery_result = if let Some(task) = discovery {
        task.await
    } else {
        Ok(())
    };
    store.pool().close().await;
    api_closed.map_err(|_| anyhow::anyhow!("API shutdown failed"))?;
    ingress_closed?;
    worker_result??;
    discovery_result?;
    Ok(())
}

fn initialize_token(path: &std::path::Path) -> anyhow::Result<()> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use std::io::Write;
    if path.exists() {
        return Ok(());
    }
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("credential generation failed"))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(URL_SAFE_NO_PAD.encode(bytes).as_bytes())?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(_) => anyhow::bail!("API credential file could not be created"),
    }
    Ok(())
}
async fn shutdown() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {result=tokio::signal::ctrl_c()=>result?,_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
