mod support;
use ha_utilities::{
    client::{Client, Transport},
    imports::{
        BillInput, BillReference, BillTotals, ImportAccount, ImportReceipt, ImportRecord,
        ImportRequest, SourceRef, SourceRole, digest,
    },
    models::{Bill, ListQuery, Page, Service, ServiceKind},
};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    process::{Command, Output},
};

fn binary() -> String {
    std::env::var("HA_UTILITIES_TEST_BINARY")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_ha-utilities").to_owned())
}

#[derive(Deserialize)]
struct CliError {
    error: String,
    status: Option<u16>,
}

#[tokio::test]
async fn actual_cli_outputs_typed_pages_and_nonzero_redacted_errors() {
    let app = support::App::start().await;
    let token = app.directory.path().join("token");
    std::fs::write(&token, support::TOKEN).unwrap();
    let binary = binary();
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
    let error: CliError = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error.error, "authentication_required");
    assert_eq!(error.status, Some(401));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fictional-wrong"));
    app.close().await;
}

#[derive(Serialize)]
struct Bundle<'a> {
    documents: [Document<'a>; 2],
    imports: [ImportRequest; 1],
}

#[derive(Serialize)]
struct Document<'a> {
    sha256: &'a str,
    path: &'a str,
}

#[derive(Deserialize)]
struct IngestResult {
    check_only: bool,
    documents: usize,
    requests: usize,
    receipts: Vec<ImportReceipt>,
}

async fn ingest_cli(bundle: &Path, options: &[&str], check: bool) -> Output {
    let mut command = Command::new(binary());
    command.args(options).args(["--json", "ingest"]).arg(bundle);
    if check {
        command.arg("--check");
    }
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

fn ingest_result(output: Output) -> IngestResult {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn actual_ingest_cli_checks_every_source_then_replays_without_duplicates() {
    let app = support::App::start().await;
    let token = app.directory.path().join("token");
    std::fs::write(&token, support::TOKEN).unwrap();
    let bundle = app.directory.path().join("bundle");
    let supporting = bundle.join("supporting");
    std::fs::create_dir_all(&supporting).unwrap();
    let invoice_bytes = b"Fictional water invoice";
    let statement_bytes = b"Fictional supporting statement";
    let invoice = bundle.join("invoice.txt");
    let statement = supporting.join("statement.txt");
    std::fs::write(&invoice, invoice_bytes).unwrap();
    std::fs::write(&statement, statement_bytes).unwrap();
    let invoice_sha = digest(invoice_bytes);
    let statement_sha = digest(statement_bytes);
    let manifest = Bundle {
        documents: [
            Document {
                sha256: &invoice_sha,
                path: "invoice.txt",
            },
            Document {
                sha256: &statement_sha,
                path: "supporting/statement.txt",
            },
        ],
        imports: [ImportRequest {
            account: ImportAccount {
                id: "example-import-water".into(),
                provider: ha_utilities::imports::ImportProvider::Lvmwd,
                start_on: "2025-01-01".parse().unwrap(),
                provider_account_sha256: None,
                services: vec![Service {
                    id: "example-import-water-meter".into(),
                    kind: ServiceKind::Water,
                }],
            },
            currency: "USD".into(),
            records: vec![ImportRecord::Bill(BillInput {
                reference: BillReference::InvoiceNumber("BILL-1".into()),
                scope: Default::default(),
                issued_on: "2025-02-01".parse().unwrap(),
                due_on: None,
                period: None,
                totals: BillTotals {
                    new_charges_minor: Some(8462),
                    ..BillTotals::default()
                },
                services: vec![],
                charges: vec![],
                sources: vec![
                    SourceRef {
                        sha256: invoice_sha.clone(),
                        role: SourceRole::Invoice,
                        pages: vec![],
                    },
                    SourceRef {
                        sha256: statement_sha.clone(),
                        role: SourceRole::Supporting,
                        pages: vec![],
                    },
                ],
                replaces_revision: None,
            })],
        }],
    };
    std::fs::write(
        bundle.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let invalid_origin = ["--url", "invalid-origin"];
    let check = ingest_result(ingest_cli(&bundle, &invalid_origin, true).await);
    assert!(check.check_only);
    assert_eq!((check.documents, check.requests), (2, 1));
    assert!(check.receipts.is_empty());

    std::fs::write(&statement, b"Changed since the manifest was written").unwrap();
    let output = ingest_cli(&bundle, &invalid_origin, false).await;
    assert!(!output.status.success());
    let error: CliError = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error.error, "invalid_document_digest");
    std::fs::write(&statement, statement_bytes).unwrap();

    #[cfg(unix)]
    {
        let outside = app.directory.path().join("outside");
        std::fs::rename(&supporting, &outside).unwrap();
        std::os::unix::fs::symlink(&outside, &supporting).unwrap();
        let output = ingest_cli(&bundle, &invalid_origin, true).await;
        assert!(!output.status.success());
        let error: CliError = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error.error, "invalid_bundle_path");
        std::fs::remove_file(&supporting).unwrap();
        std::fs::rename(&outside, &supporting).unwrap();
    }

    let options = ["--url", &app.url, "--token-file", token.to_str().unwrap()];
    let imported = ingest_result(ingest_cli(&bundle, &options, false).await);
    assert!(!imported.check_only);
    assert_eq!(imported.receipts.len(), 1);
    assert_eq!(imported.receipts[0].inserted, 1);
    let replayed = ingest_result(ingest_cli(&bundle, &options, false).await);
    assert_eq!(replayed.receipts.len(), 1);
    assert!(replayed.receipts[0].replayed);

    let client = Client::new(&app.url, Transport::Native, support::TOKEN).unwrap();
    let page = client
        .bills(&ListQuery {
            account: Some("example-import-water".into()),
            ..ListQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].total.minor_units, 8462);
    app.close().await;
}
