//! Explicit deployment/account/service bindings. No account discovery or enrollment.
use crate::models::{Account, ProviderId, Service, ServiceKind};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::PathBuf};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub deployment: String,
    #[serde(default)]
    pub accounts: Vec<AccountBinding>,
    pub wm: Option<WmConfig>,
    pub bayou: Option<BayouConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccountBinding {
    pub id: String,
    pub name: String,
    pub deployment: String,
    pub provider: ProviderId,
    pub customer_id: String,
    pub provider_account: String,
    pub currency: String,
    pub services: Vec<ServiceBinding>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceBinding {
    pub id: String,
    pub name: String,
    pub provider_service: String,
    pub kind: ServiceKind,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WmConfig {
    pub token_file: PathBuf,
    pub client_id_file: PathBuf,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BayouConfig {
    pub api_key_file: PathBuf,
}

impl Config {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.deployment.trim().is_empty(),
            "deployment must be explicit"
        );
        anyhow::ensure!(self.accounts.len() <= 100, "too many account bindings");
        let mut accounts = HashSet::new();
        let mut remote = HashSet::new();
        let mut services = HashSet::new();
        for account in &self.accounts {
            anyhow::ensure!(
                account.currency.len() == 3
                    && account.currency.bytes().all(|b| b.is_ascii_uppercase()),
                "account currency must be an explicit ISO currency code"
            );
            anyhow::ensure!(
                valid_id(&account.id) && accounts.insert(&account.id),
                "invalid or duplicate local account"
            );
            anyhow::ensure!(
                account.deployment == self.deployment,
                "account deployment does not match"
            );
            anyhow::ensure!(
                !account.name.trim().is_empty()
                    && !account.customer_id.trim().is_empty()
                    && !account.provider_account.trim().is_empty(),
                "account binding must be explicit"
            );
            anyhow::ensure!(
                remote.insert((
                    format!("{:?}", account.provider),
                    &account.customer_id,
                    &account.provider_account
                )),
                "duplicate provider account binding"
            );
            anyhow::ensure!(account.services.len() <= 64, "too many service bindings");
            let mut provider_services = HashSet::new();
            for service in &account.services {
                anyhow::ensure!(
                    valid_id(&service.id) && services.insert(&service.id),
                    "invalid or duplicate local service"
                );
                anyhow::ensure!(
                    !service.provider_service.trim().is_empty()
                        && !service.name.trim().is_empty()
                        && provider_services.insert(&service.provider_service),
                    "service binding must be explicit and unique"
                );
            }
        }
        Ok(())
    }

    pub fn public_accounts(&self) -> Vec<Account> {
        self.accounts
            .iter()
            .map(|a| Account {
                id: a.id.clone(),
                name: a.name.clone(),
                provider: a.provider,
                services: a
                    .services
                    .iter()
                    .map(|s| Service {
                        id: s.id.clone(),
                        name: s.name.clone(),
                        kind: s.kind,
                    })
                    .collect(),
            })
            .collect()
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-.:".contains(&c))
        && id != "."
        && id != ".."
}

pub fn read_secret(path: &std::path::Path) -> anyhow::Result<String> {
    let value = std::fs::read_to_string(path)
        .map_err(|_| anyhow::anyhow!("credential file could not be read"))?;
    let value = value.trim();
    anyhow::ensure!(
        !value.is_empty() && value.len() <= 8192 && !value.chars().any(char::is_control),
        "credential file is invalid"
    );
    Ok(value.to_owned())
}
