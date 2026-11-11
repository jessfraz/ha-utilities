mod support;

use ha_utilities::{
    imports::{
        BillInput, BillReference, BillScope, BillTotals, ImportAccount, ImportRecord,
        ImportRequest, SourceRef, SourceRole, digest,
    },
    models::{AccountSource, LiveProvider, Service, ServiceKind},
};

#[tokio::test]
async fn accounts_expose_durable_sources_and_resolve_optional_device_labels() {
    let app = support::App::start().await;
    let client = ha_utilities::client::Client::new(
        &app.url,
        ha_utilities::client::Transport::Native,
        support::TOKEN,
    )
    .unwrap();
    let initial = client.accounts().await.unwrap();
    assert_eq!(
        initial.items[0].source,
        AccountSource::Live(LiveProvider::Bayou)
    );
    assert_eq!(
        initial.items[1].source,
        AccountSource::Live(LiveProvider::WasteManagement)
    );

    let document = b"Fictional internet statement";
    let sha256 = digest(document);
    app.store
        .put_document("demo", &sha256, document)
        .await
        .unwrap();
    app.store
        .import(
            "demo",
            ImportRequest {
                account: ImportAccount {
                    id: "fictional-internet".to_owned(),
                    name: "Internet".to_owned(),
                    provider: ha_utilities::imports::ImportProvider::Starlink,
                    start_on: "2025-01-01".parse().unwrap(),
                    provider_account_sha256: None,
                    services: vec![
                        Service {
                            id: "dish-one".to_owned(),
                            name: String::new(),
                            kind: ServiceKind::Internet,
                        },
                        Service {
                            id: "dish-two".to_owned(),
                            name: "Fictional Mini".to_owned(),
                            kind: ServiceKind::Internet,
                        },
                    ],
                },
                currency: "USD".to_owned(),
                records: vec![ImportRecord::Bill(BillInput {
                    reference: BillReference::InvoiceNumber("EXAMPLE-1".to_owned()),
                    scope: BillScope::Account,
                    issued_on: "2025-01-02".parse().unwrap(),
                    due_on: None,
                    period: None,
                    totals: BillTotals {
                        new_charges_minor: Some(1000),
                        ..BillTotals::default()
                    },
                    services: Vec::new(),
                    charges: Vec::new(),
                    sources: vec![SourceRef {
                        sha256,
                        role: SourceRole::Statement,
                        pages: Vec::new(),
                    }],
                    replaces_revision: None,
                })],
            },
        )
        .await
        .unwrap();

    let accounts = client.accounts().await.unwrap();
    let manual = accounts
        .items
        .iter()
        .find(|account| account.id == "fictional-internet")
        .unwrap();
    assert_eq!(manual.source, AccountSource::ManualImport);
    assert_eq!(manual.services[0].name, "Internet");
    assert_eq!(manual.services[1].name, "Fictional Mini");
    let binding = app.store.binding("demo", &manual.id).await.unwrap();
    assert_eq!(binding.services[0].name, None);
    assert_eq!(binding.services[1].name.as_deref(), Some("Fictional Mini"));

    let providers = client.providers().await.unwrap();
    assert_eq!(providers.items.len(), 2);
    assert!(providers.items.iter().all(|provider| matches!(
        provider.id,
        LiveProvider::Bayou | LiveProvider::WasteManagement
    )));
    app.close().await;
}
