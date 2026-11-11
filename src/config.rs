//! Manual deployment/account/service bindings and private provider credentials.
use crate::models::{LiveProvider, ProviderId, ServiceKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Supervisor owns this input. Credentials are never serializable or exposed by
/// the app API; adapters receive only private runtime-file references.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AppOptions {
    config_file: Option<PathBuf>,
    deployment: Option<String>,
    accounts: Option<Vec<AccountBinding>>,
    wm_token: Option<String>,
    wm_client_id: Option<String>,
    bayou_api_key: Option<String>,
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub provider_service: String,
    pub kind: ServiceKind,
}

impl ServiceBinding {
    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or(self.kind.name())
    }
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
    pub fn load(native_path: &Path, options_path: Option<&Path>) -> anyhow::Result<Self> {
        let Some(options_path) = options_path else {
            return Self::load_native(native_path);
        };
        let metadata = std::fs::symlink_metadata(options_path)
            .map_err(|_| anyhow::anyhow!("app options could not be read"))?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "app options must be a regular file"
        );
        let mut file = std::fs::File::open(options_path)
            .map_err(|_| anyhow::anyhow!("app options could not be read"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|_| anyhow::anyhow!("app options permissions could not be set"))?;
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| anyhow::anyhow!("app options could not be read"))?;
        let options: AppOptions = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("app options are invalid"))?;
        let selects_inline = options.deployment.is_some()
            || options.accounts.is_some()
            || options.wm_token.is_some()
            || options.wm_client_id.is_some()
            || options.bayou_api_key.is_some();
        let wm_token = option_secret(options.wm_token)?;
        let wm_client_id = option_secret(options.wm_client_id)?;
        let bayou_key = option_secret(options.bayou_api_key)?;
        let has_inline = options
            .deployment
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            || options
                .accounts
                .as_ref()
                .is_some_and(|accounts| !accounts.is_empty())
            || wm_token.is_some()
            || wm_client_id.is_some()
            || bayou_key.is_some();
        if let Some(path) = options
            .config_file
            .filter(|path| !path.as_os_str().is_empty())
        {
            anyhow::ensure!(
                !has_inline,
                "choose Supervisor options or a native configuration file, not both"
            );
            return Self::load_native(&path);
        }
        if !selects_inline {
            return Self::load_native(native_path);
        }
        anyhow::ensure!(
            wm_token.is_some() == wm_client_id.is_some(),
            "WM token and client ID must be configured together"
        );
        anyhow::ensure!(
            bayou_key.as_ref().is_none_or(|key| !key.contains(':')),
            "Bayou API key is invalid"
        );
        let parent = options_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("app options have no runtime directory"))?;
        let secrets = parent.join("credentials");
        let deployment = options.deployment.unwrap_or_default();
        let accounts = options.accounts.unwrap_or_default();
        let config = Self {
            deployment: if deployment.trim().is_empty() && accounts.is_empty() {
                "unconfigured".to_owned()
            } else {
                deployment
            },
            accounts,
            wm: wm_token.as_ref().map(|_| WmConfig {
                token_file: secrets.join("wm-token"),
                client_id_file: secrets.join("wm-client-id"),
            }),
            bayou: bayou_key.as_ref().map(|_| BayouConfig {
                api_key_file: secrets.join("bayou-api-key"),
            }),
        };
        // Reject invalid or ambiguous bindings before writing any credential.
        config.validate_static_accounts()?;
        private_directory(&secrets)?;
        for (name, value) in [
            ("wm-token", wm_token),
            ("wm-client-id", wm_client_id),
            ("bayou-api-key", bayou_key),
        ] {
            let path = secrets.join(name);
            if let Some(value) = value {
                write_private(&path, value.as_bytes())?;
            } else {
                match std::fs::remove_file(&path) {
                    Ok(()) => (),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(_) => anyhow::bail!("unused runtime credential could not be removed"),
                }
            }
        }
        Ok(config)
    }

    fn load_native(path: &Path) -> anyhow::Result<Self> {
        let config = if path.exists() {
            toml::from_str(
                &std::fs::read_to_string(path)
                    .map_err(|_| anyhow::anyhow!("configuration could not be read"))?,
            )
            .map_err(|_| anyhow::anyhow!("configuration is invalid"))?
        } else {
            Self {
                deployment: "unconfigured".to_owned(),
                accounts: Vec::new(),
                wm: None,
                bayou: None,
            }
        };
        config.validate_static_accounts()?;
        Ok(config)
    }

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
                    match account.provider {
                        ProviderId::WasteManagement => matches!(
                            service.kind,
                            ServiceKind::Waste | ServiceKind::Recycling | ServiceKind::Other
                        ),
                        ProviderId::Bayou =>
                            matches!(service.kind, ServiceKind::Electricity | ServiceKind::Gas),
                        ProviderId::Lvmwd => service.kind == ServiceKind::Water,
                        ProviderId::SuperiorPropane => service.kind == ServiceKind::Propane,
                        ProviderId::Starlink | ProviderId::Spectrum =>
                            service.kind == ServiceKind::Internet,
                    },
                    "service kind does not match its utility provider"
                );
                anyhow::ensure!(
                    valid_id(&service.id) && services.insert(&service.id),
                    "invalid or duplicate local service"
                );
                anyhow::ensure!(
                    !service.provider_service.trim().is_empty()
                        && service
                            .name
                            .as_ref()
                            .is_none_or(|name| !name.trim().is_empty())
                        && provider_services.insert(&service.provider_service),
                    "service binding must be explicit and unique"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn validate_static_accounts(&self) -> anyhow::Result<()> {
        self.validate()?;
        anyhow::ensure!(
            self.accounts
                .iter()
                .all(|account| LiveProvider::try_from(account.provider).is_ok()),
            "static account bindings require a live API provider; manual accounts must be imported"
        );
        Ok(())
    }
}

fn option_secret(value: Option<String>) -> anyhow::Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        value.len() <= 8192 && value.is_ascii() && !value.chars().any(char::is_control),
        "provider credential is invalid"
    );
    Ok(Some(value.to_owned()))
}

fn private_directory(path: &Path) -> anyhow::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(_) => anyhow::bail!("runtime credential directory could not be created"),
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| anyhow::anyhow!("runtime credential directory is unavailable"))?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "runtime credential directory must be a real directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| anyhow::anyhow!("runtime credential permissions could not be set"))?;
    }
    Ok(())
}

fn write_private(path: &Path, value: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_file_name(format!(".credential-{}", uuid::Uuid::now_v7()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(value)?;
        file.sync_all()?;
        // Rename replaces a stale link itself; it never follows its target.
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(|_| anyhow::anyhow!("runtime credential could not be saved"))
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
