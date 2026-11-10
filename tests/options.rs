//! The production executable provisions Supervisor options, not a test loader.
use ha_utilities::{
    config::{AccountBinding, Config},
    models::{Account, Page, ProviderId, ProviderInfo},
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

const WM_TOKEN: &str = "fictional-wm-token-for-options";
const WM_CLIENT: &str = "fictional-wm-client-for-options";
const BAYOU_KEY: &str = "fictional-bayou-key-for-options";

#[derive(Serialize)]
struct Settings {
    deployment: String,
    accounts: Vec<AccountBinding>,
    wm_token: String,
    wm_client_id: String,
    bayou_api_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    config_file: Option<PathBuf>,
}
impl Default for Settings {
    fn default() -> Self {
        let config: Config = toml::from_str(include_str!("../config/example.toml")).unwrap();
        Self {
            deployment: config.deployment,
            accounts: config.accounts,
            wm_token: WM_TOKEN.to_owned(),
            wm_client_id: WM_CLIENT.to_owned(),
            bayou_api_key: BAYOU_KEY.to_owned(),
            config_file: None,
        }
    }
}
fn write_settings(directory: &Path, settings: &Settings) {
    std::fs::write(
        directory.join("options.json"),
        serde_json::to_vec(settings).unwrap(),
    )
    .unwrap();
}
fn command(directory: &Path, port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_utilities"));
    command
        .args(["--options"])
        .arg(directory.join("options.json"))
        .arg("--config")
        .arg(directory.join("native.toml"))
        .arg("--database")
        .arg(directory.join("utilities.sqlite3"))
        .arg("--token-file")
        .arg(directory.join("api-token"))
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .env_remove("SUPERVISOR_TOKEN");
    let log = std::fs::File::create(directory.join("service.log")).unwrap();
    command
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    command
}
struct Running {
    child: Child,
    origin: String,
    token: String,
}
impl Running {
    async fn start(directory: &Path) -> Self {
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        let child = command(directory, port).spawn().unwrap();
        let mut running = Self {
            child,
            origin: format!("http://127.0.0.1:{port}"),
            token: String::new(),
        };
        for _ in 0..200 {
            if let Some(status) = running.child.try_wait().unwrap() {
                panic!(
                    "service exited {status}: {}",
                    std::fs::read_to_string(directory.join("service.log")).unwrap()
                );
            }
            if let Ok(token) = std::fs::read_to_string(directory.join("api-token"))
                && reqwest::Client::new()
                    .get(format!("{}/v1/system", running.origin))
                    .bearer_auth(token.trim())
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success())
            {
                running.token = token.trim().to_owned();
                return running;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("service did not become ready");
    }
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        reqwest::Client::new()
            .get(format!("{}{path}", self.origin))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn supervisor_options_provision_explicit_accounts_and_private_credentials() {
    let directory = tempfile::tempdir().unwrap();
    write_settings(directory.path(), &Settings::default());
    let running = Running::start(directory.path()).await;
    let accounts: Page<Account> = running.get("/v1/accounts").await;
    assert_eq!(accounts.items.len(), 2);
    let providers: Page<ProviderInfo> = running.get("/v1/providers").await;
    assert!(providers.items.iter().all(|p| p.configured));
    assert!(providers.items.iter().any(|p| p.id == ProviderId::Bayou));
    for (name, value) in [
        ("wm-token", WM_TOKEN),
        ("wm-client-id", WM_CLIENT),
        ("bayou-api-key", BAYOU_KEY),
    ] {
        let path = directory.path().join("credentials").join(name);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), value);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(directory.path().join("credentials"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(directory.path().join("options.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let public =
        serde_json::to_string(&accounts).unwrap() + &serde_json::to_string(&providers).unwrap();
    let logs = std::fs::read_to_string(directory.path().join("service.log")).unwrap();
    for secret in [WM_TOKEN, WM_CLIENT, BAYOU_KEY] {
        assert!(!public.contains(secret));
        assert!(!logs.contains(secret));
    }
    assert!(!directory.path().join("native.toml").exists());
    drop(running);
}

async fn rejected(directory: &Path) -> String {
    let mut process = command(directory, 0).spawn().unwrap();
    for _ in 0..200 {
        if let Some(status) = process.try_wait().unwrap() {
            assert!(!status.success());
            let error = std::fs::read_to_string(directory.join("service.log")).unwrap();
            for secret in [WM_TOKEN, WM_CLIENT, BAYOU_KEY] {
                assert!(!error.contains(secret));
            }
            return error;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = process.kill();
    let _ = process.wait();
    panic!("invalid options started the service");
}

#[tokio::test]
async fn invalid_ambiguous_and_cross_provider_bindings_never_save_credentials() {
    for variant in ["deployment", "duplicate", "pair", "kind", "mixed"] {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        match variant {
            "deployment" => settings.accounts[0].deployment = "other".to_owned(),
            "duplicate" => settings.accounts.push(settings.accounts[0].clone()),
            "pair" => settings.wm_client_id.clear(),
            "kind" => {
                settings.accounts[0].services[0].kind =
                    ha_utilities::models::ServiceKind::Electricity
            }
            "mixed" => settings.config_file = Some(directory.path().join("native.toml")),
            _ => unreachable!(),
        }
        write_settings(directory.path(), &settings);
        rejected(directory.path()).await;
        assert!(!directory.path().join("credentials").exists());
    }
}

#[tokio::test]
async fn explicit_native_file_options_remain_supported() {
    #[derive(Serialize)]
    struct FileOptions {
        config_file: PathBuf,
    }
    let directory = tempfile::tempdir().unwrap();
    let native = directory.path().join("native.toml");
    std::fs::write(&native, include_str!("../config/example.toml")).unwrap();
    std::fs::write(
        directory.path().join("options.json"),
        serde_json::to_vec(&FileOptions {
            config_file: native,
        })
        .unwrap(),
    )
    .unwrap();
    let running = Running::start(directory.path()).await;
    let accounts: Page<Account> = running.get("/v1/accounts").await;
    assert_eq!(accounts.items.len(), 2);
    let providers: Page<ProviderInfo> = running.get("/v1/providers").await;
    assert!(providers.items.iter().all(|p| !p.configured));
    assert!(!directory.path().join("credentials").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn managed_secret_replacement_does_not_follow_links_or_credential_directory_links() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir().unwrap();
    write_settings(directory.path(), &Settings::default());
    drop(Running::start(directory.path()).await);
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    std::fs::write(&sentinel, "fictional-outside-state").unwrap();
    std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o600)).unwrap();
    let credential = directory.path().join("credentials/wm-token");
    std::fs::remove_file(&credential).unwrap();
    symlink(&sentinel, &credential).unwrap();
    drop(Running::start(directory.path()).await);
    assert!(
        !std::fs::symlink_metadata(&credential)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(&sentinel).unwrap(),
        "fictional-outside-state"
    );
    std::fs::remove_dir_all(directory.path().join("credentials")).unwrap();
    symlink(outside.path(), directory.path().join("credentials")).unwrap();
    let mode = std::fs::metadata(outside.path())
        .unwrap()
        .permissions()
        .mode();
    rejected(directory.path()).await;
    assert_eq!(
        std::fs::metadata(outside.path())
            .unwrap()
            .permissions()
            .mode(),
        mode
    );
    assert!(!outside.path().join("wm-token").exists());
    assert_eq!(
        std::fs::read_to_string(sentinel).unwrap(),
        "fictional-outside-state"
    );
}

#[tokio::test]
async fn cleared_options_credentials_are_no_longer_configured_or_retained() {
    for reset_all in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut settings = Settings::default();
        write_settings(directory.path(), &settings);
        let original = Running::start(directory.path()).await;
        let token = original.token.clone();
        drop(original);
        settings.wm_token.clear();
        settings.wm_client_id.clear();
        settings.bayou_api_key.clear();
        if reset_all {
            settings.deployment.clear();
            settings.accounts.clear();
        }
        write_settings(directory.path(), &settings);
        let running = Running::start(directory.path()).await;
        assert_eq!(running.token, token);
        let providers: Page<ProviderInfo> = running.get("/v1/providers").await;
        assert!(providers.items.iter().all(|p| !p.configured));
        assert_eq!(
            std::fs::read_dir(directory.path().join("credentials"))
                .unwrap()
                .count(),
            0
        );
    }
}
