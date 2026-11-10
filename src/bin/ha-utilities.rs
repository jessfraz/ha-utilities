use chrono::NaiveDate;
use clap::{Args, Parser, Subcommand};
use ha_utilities::{
    client::{Client, ClientError, Transport},
    config::read_secret,
    models::{ListQuery, SyncInput},
};
use serde::Serialize;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Read utility bills, collections, and usage from a local Utilities service"
)]
struct Cli {
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
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
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

fn configured_client(cli: &Cli) -> Result<Client, ClientError> {
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

async fn run(cli: Cli) -> Result<(), ClientError> {
    let client = configured_client(&cli)?;
    match cli.command {
        Command::Health => print(&client.health().await?, cli.json),
        Command::System => print(&client.system().await?, cli.json),
        Command::Summary => print(&client.summary().await?, cli.json),
        Command::Providers { .. } => print(&client.providers().await?, cli.json),
        Command::Accounts { .. } => print(&client.accounts().await?, cli.json),
        Command::Bills {
            command: Bills::List(filters),
        } => print(&client.bills(&filters.into()).await?, cli.json),
        Command::Bills {
            command: Bills::Get { id },
        } => print(&client.bill(&id).await?, cli.json),
        Command::Balances {
            command: Lists::List(filters),
        } => print(&client.balances(&filters.into()).await?, cli.json),
        Command::Collections {
            command: Lists::List(filters),
        } => print(&client.collections(&filters.into()).await?, cli.json),
        Command::Usage {
            command: Lists::List(filters),
        } => print(&client.usage(&filters.into()).await?, cli.json),
        Command::Sync {
            command: Sync::Request { account, from, to },
        } => print(
            &client
                .sync_request(&SyncInput { account, from, to })
                .await?,
            cli.json,
        ),
        Command::Sync {
            command: Sync::Get { id },
        } => print(&client.sync_get(&id).await?, cli.json),
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
