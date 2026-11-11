use chrono::NaiveDate;
use clap::{Args, Parser, Subcommand};
use ha_utilities::{
    client::{Client, ClientError, Transport},
    config::read_secret,
    imports::{ImportReceipt, ImportRequest, MAX_DOCUMENT_BYTES, MAX_IMPORT_REQUEST_BYTES, digest},
    models::{
        BindConnection, ConnectionProvider, ConnectionSelection, CreateConnection, ListQuery,
        SyncInput,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
};

const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_BUNDLE_ENTRIES: usize = 1024;

#[derive(Parser)]
#[command(
    version,
    about = "Read utility data and import sourced records into a local Utilities service"
)]
struct Cli {
    #[command(flatten)]
    client: ClientOptions,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct ClientOptions {
    #[arg(
        long,
        help = "Origin. Defaults to UTILITIES_URL or HOME_ASSISTANT_URL for the selected transport",
        global = true
    )]
    url: Option<String>,
    #[arg(long, value_enum, default_value = "native", global = true)]
    transport: Transport,
    #[arg(
        long,
        global = true,
        help = "Credential file overriding the selected transport environment"
    )]
    token_file: Option<PathBuf>,
}
#[derive(Subcommand)]
enum Command {
    Health,
    System,
    Summary,
    Providers {
        #[command(subcommand)]
        command: ListOnly,
    },
    Accounts {
        #[command(subcommand)]
        command: ListOnly,
    },
    Bills {
        #[command(subcommand)]
        command: Bills,
    },
    Balances {
        #[command(subcommand)]
        command: Lists,
    },
    Collections {
        #[command(subcommand)]
        command: Lists,
    },
    Usage {
        #[command(subcommand)]
        command: Lists,
    },
    Sync {
        #[command(subcommand)]
        command: Sync,
    },
    /// Connect a utility through Bayou's hosted sign-in and select its meters.
    Connections {
        #[command(subcommand)]
        command: Connections,
    },
    /// Import typed utility records and their source documents from a bundle.
    Ingest {
        /// Directory containing manifest.json and its relative source paths.
        bundle: PathBuf,
        /// Validate the manifest and source hashes locally.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum Connections {
    Utilities,
    List,
    Get {
        id: String,
    },
    Start {
        #[arg(long)]
        utility: String,
        #[arg(long, default_value = "USD")]
        currency: String,
        /// Accept this Bayou workspace's data-unlocking and billing settings.
        ///
        /// Defaults unlock 12 billing periods, future bills, and both meter types.
        /// Above the 10-meter free tier, billing requires setup and applies to all
        /// unlocked data, including the first 10. Local meter selection happens
        /// afterward and does not limit charges.
        /// https://docs.bayou.energy/docs/pricing-overview
        #[arg(long, required = true)]
        accept_provider_settings: bool,
        /// Reuse the request ID from an interrupted start instead of creating another intent.
        #[arg(long)]
        request_id: Option<uuid::Uuid>,
    },
    Refresh {
        id: String,
    },
    Resume {
        id: String,
    },
    /// Request a fresh, private hosted sign-in link.
    Authorize {
        id: String,
    },
    Bind {
        id: String,
        #[arg(long)]
        account: String,
        #[arg(long = "meter", required = true)]
        meters: Vec<String>,
    },
}
#[derive(Subcommand)]
enum ListOnly {
    List,
}
#[derive(Subcommand)]
enum Lists {
    List(Filters),
}
#[derive(Subcommand)]
enum Bills {
    List(Filters),
    Get { id: String },
}
#[derive(Subcommand)]
enum Sync {
    Request {
        #[arg(long)]
        account: String,
        #[arg(long)]
        from: NaiveDate,
        #[arg(long)]
        to: NaiveDate,
    },
    /// Read all history available from this account's provider.
    All {
        #[arg(long)]
        account: String,
    },
    /// Show the last sync for each bound account.
    List,
    Get {
        id: String,
    },
}
#[derive(Args)]
struct Filters {
    #[arg(long)]
    account: Option<String>,
    #[arg(long)]
    service: Option<String>,
    #[arg(long)]
    from: Option<NaiveDate>,
    #[arg(long)]
    to: Option<NaiveDate>,
    #[arg(long,default_value="50",value_parser=clap::value_parser!(u32).range(1..=200))]
    limit: u32,
    #[arg(long, help = "Continue a page using the same filters and limit")]
    page: Option<String>,
}
impl From<Filters> for ListQuery {
    fn from(v: Filters) -> Self {
        Self {
            account: v.account,
            import_provider: None,
            service: v.service,
            from: v.from,
            to: v.to,
            limit: Some(v.limit),
            page: v.page,
        }
    }
}

fn print<T: Serialize>(value: &T, json: bool) -> Result<(), ClientError> {
    let result = if json {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    }
    .map_err(|_| ClientError {
        error: "output_error",
        message: "output could not be serialized".to_owned(),
        status: None,
        request_id: None,
    })?;
    println!("{result}");
    Ok(())
}
fn variable(name: &str) -> Result<Option<String>, ClientError> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(ClientError::new("invalid_configuration", None))
        }
    }
}

fn configured_client(cli: &ClientOptions) -> Result<Client, ClientError> {
    let url = match &cli.url {
        Some(url) => url.clone(),
        None => match cli.transport {
            Transport::Native => {
                variable("UTILITIES_URL")?.unwrap_or_else(|| "http://127.0.0.1:8080".to_owned())
            }
            Transport::Ha => variable("HOME_ASSISTANT_URL")?
                .ok_or_else(|| ClientError::new("url_required", None))?,
        },
    };
    let token_file = cli.token_file.clone().or_else(|| match cli.transport {
        Transport::Native => std::env::var_os("UTILITIES_TOKEN_FILE").map(PathBuf::from),
        Transport::Ha => None,
    });
    let token = if let Some(path) = token_file {
        read_secret(&path).map_err(|_| ClientError::new("credential_file_unavailable", None))?
    } else {
        match cli.transport {
            Transport::Native => return Err(ClientError::new("credential_file_required", None)),
            Transport::Ha => variable("HOME_ASSISTANT_TOKEN")?
                .ok_or_else(|| ClientError::new("credential_required", None))?,
        }
    };
    Client::new(&url, cli.transport, &token)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleManifest {
    documents: Vec<BundleDocument>,
    imports: Vec<ImportRequest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleDocument {
    sha256: String,
    path: PathBuf,
}

#[derive(Serialize)]
struct BundleResult {
    check_only: bool,
    documents: usize,
    requests: usize,
    receipts: Vec<ImportReceipt>,
}

fn bundle_file(root: &Path, relative: &Path, limit: usize) -> Result<Vec<u8>, ClientError> {
    let invalid_path = || ClientError::new("invalid_bundle_path", None);
    let mut path = root.to_path_buf();
    let mut components = relative.components().peekable();
    if !fs::symlink_metadata(root).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(invalid_path());
    }
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(invalid_path());
        };
        path.push(name);
        let expected = fs::symlink_metadata(&path).map_err(|_| invalid_path())?;
        if components.peek().is_some() {
            if !expected.is_dir() {
                return Err(invalid_path());
            }
            continue;
        }
        if !expected.is_file() {
            return Err(invalid_path());
        }
        let canonical = path.canonicalize().map_err(|_| invalid_path())?;
        if !canonical.starts_with(root) {
            return Err(invalid_path());
        }
        let file = File::open(canonical).map_err(|_| invalid_path())?;
        let actual = file.metadata().map_err(|_| invalid_path())?;
        if !actual.is_file() {
            return Err(invalid_path());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
                return Err(invalid_path());
            }
        }
        let mut bytes = Vec::new();
        file.take((limit as u64) + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ClientError::new("bundle_read_failed", None))?;
        if bytes.len() > limit {
            return Err(ClientError::new("bundle_file_too_large", None));
        }
        return Ok(bytes);
    }
    Err(invalid_path())
}

fn document_bytes(root: &Path, document: &BundleDocument) -> Result<Vec<u8>, ClientError> {
    let bytes = bundle_file(root, &document.path, MAX_DOCUMENT_BYTES)?;
    if bytes.is_empty() || digest(&bytes) != document.sha256 {
        return Err(ClientError::new("invalid_document_digest", None));
    }
    Ok(bytes)
}

async fn ingest(
    bundle: &Path,
    check_only: bool,
    options: &ClientOptions,
    json: bool,
) -> Result<(), ClientError> {
    let bundle: PathBuf = bundle.components().collect();
    if !fs::symlink_metadata(&bundle).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(ClientError::new("invalid_bundle_path", None));
    }
    let root = bundle
        .canonicalize()
        .map_err(|_| ClientError::new("invalid_bundle_path", None))?;
    let manifest: BundleManifest = serde_json::from_slice(&bundle_file(
        &root,
        Path::new("manifest.json"),
        MAX_MANIFEST_BYTES,
    )?)
    .map_err(|_| ClientError::new("invalid_bundle_manifest", None))?;
    if manifest.imports.is_empty()
        || manifest.imports.len() > MAX_BUNDLE_ENTRIES
        || manifest.documents.len() > MAX_BUNDLE_ENTRIES
    {
        return Err(ClientError::new("invalid_bundle_manifest", None));
    }
    let mut referenced = BTreeSet::new();
    for input in &manifest.imports {
        input
            .validate()
            .map_err(|_| ClientError::new("invalid_import", None))?;
        let body =
            serde_json::to_vec(input).map_err(|_| ClientError::new("invalid_import", None))?;
        if body.len() > MAX_IMPORT_REQUEST_BYTES {
            return Err(ClientError::new("request_too_large", None));
        }
        referenced.extend(input.document_digests());
    }
    let declared: BTreeSet<_> = manifest
        .documents
        .iter()
        .map(|document| document.sha256.as_str())
        .collect();
    if declared.len() != manifest.documents.len() || declared != referenced {
        return Err(ClientError::new("invalid_bundle_documents", None));
    }
    for document in &manifest.documents {
        document_bytes(&root, document)?;
    }
    let mut result = BundleResult {
        check_only,
        documents: manifest.documents.len(),
        requests: manifest.imports.len(),
        receipts: Vec::new(),
    };
    if !check_only {
        let client = configured_client(options)?;
        for document in &manifest.documents {
            // Recheck bytes after preflight so a changed bundle cannot send stale evidence.
            let bytes = document_bytes(&root, document)?;
            client.upload_document(&document.sha256, bytes).await?;
        }
        for input in &manifest.imports {
            result.receipts.push(client.import(input).await?);
        }
    }
    print(&result, json)
}

async fn run(cli: Cli) -> Result<(), ClientError> {
    let client = || configured_client(&cli.client);
    match cli.command {
        Command::Ingest { bundle, check } => ingest(&bundle, check, &cli.client, cli.json).await,
        Command::Health => print(&client()?.health().await?, cli.json),
        Command::System => print(&client()?.system().await?, cli.json),
        Command::Summary => print(&client()?.summary().await?, cli.json),
        Command::Providers { .. } => print(&client()?.providers().await?, cli.json),
        Command::Accounts { .. } => print(&client()?.accounts().await?, cli.json),
        Command::Bills {
            command: Bills::List(filters),
        } => print(&client()?.bills(&filters.into()).await?, cli.json),
        Command::Bills {
            command: Bills::Get { id },
        } => print(&client()?.bill(&id).await?, cli.json),
        Command::Balances {
            command: Lists::List(filters),
        } => print(&client()?.balances(&filters.into()).await?, cli.json),
        Command::Collections {
            command: Lists::List(filters),
        } => print(&client()?.collections(&filters.into()).await?, cli.json),
        Command::Usage {
            command: Lists::List(filters),
        } => print(&client()?.usage(&filters.into()).await?, cli.json),
        Command::Sync {
            command: Sync::Request { account, from, to },
        } => print(
            &client()?
                .sync_request(&SyncInput { account, from, to })
                .await?,
            cli.json,
        ),
        Command::Sync {
            command: Sync::Get { id },
        } => print(&client()?.sync_get(&id).await?, cli.json),
        Command::Sync {
            command: Sync::All { account },
        } => print(&client()?.sync_history(account).await?, cli.json),
        Command::Sync {
            command: Sync::List,
        } => print(&client()?.sync_list().await?, cli.json),
        Command::Connections { command } => {
            let client = client()?;
            match command {
                Connections::Utilities => print(&client.connection_utilities().await?, cli.json),
                Connections::List => print(&client.connections().await?, cli.json),
                Connections::Get { id } => print(&client.connection(&id).await?, cli.json),
                Connections::Start {
                    utility,
                    currency,
                    accept_provider_settings,
                    request_id,
                } => print(
                    &client
                        .connection_start(&CreateConnection {
                            provider: ConnectionProvider::Bayou,
                            request_id: request_id.unwrap_or_else(uuid::Uuid::now_v7).to_string(),
                            utility_id: utility,
                            currency,
                            accept_provider_settings,
                        })
                        .await?,
                    cli.json,
                ),
                Connections::Refresh { id } => {
                    print(&client.connection_refresh(&id).await?, cli.json)
                }
                Connections::Resume { id } => {
                    print(&client.connection_resume(&id).await?, cli.json)
                }
                Connections::Authorize { id } => {
                    print(&client.connection_authorization(&id).await?, cli.json)
                }
                Connections::Bind {
                    id,
                    account,
                    meters,
                } => print(
                    &client
                        .connection_bind(
                            &id,
                            &BindConnection {
                                accounts: vec![ConnectionSelection { account, meters }],
                            },
                        )
                        .await?,
                    cli.json,
                ),
            }
        }
    }
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    if let Err(error) = run(Cli::parse()).await {
        match serde_json::to_string(&error) {
            Ok(value) => eprintln!("{value}"),
            Err(_) => eprintln!("{{\"error\":\"output_error\"}}"),
        };
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
