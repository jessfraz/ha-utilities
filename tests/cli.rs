mod support;
use ha_utilities::models::{Bill, Page};

#[tokio::test]
async fn actual_cli_outputs_typed_pages_and_nonzero_redacted_errors() {
    let app = support::App::start().await;
    let token = app.directory.path().join("token");
    std::fs::write(&token, support::TOKEN).unwrap();
    let binary = std::env::var("HA_UTILITIES_TEST_BINARY")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_ha-utilities").to_owned());
    let first_binary = binary.clone();
    let url = app.url.clone();
    let token_path = token.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(first_binary)
            .args(["--url", &url, "--token-file"])
            .arg(token_path)
            .args(["--json", "bills", "list", "--limit", "2"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let page: Page<Bill> = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(page.next_page.is_some());
    std::fs::write(&token, "fictional-wrong-token-12345").unwrap();
    let url = app.url.clone();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(binary)
            .args(["--url", &url, "--token-file"])
            .arg(token)
            .args(["--json", "summary"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    #[derive(serde::Deserialize)]
    struct Error {
        error: String,
        status: Option<u16>,
    }
    let error: Error = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error.error, "authentication_required");
    assert_eq!(error.status, Some(401));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fictional-wrong"));
    app.close().await;
}
