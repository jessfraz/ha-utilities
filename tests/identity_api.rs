mod support;
use ha_utilities::models::{AccountSource, Page, ProviderId, ServiceKind};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountContract {
    id: String,
    provider: ProviderId,
    connection_id: Option<String>,
    services: Vec<ServiceContract>,
    source: AccountSource,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceContract {
    id: String,
    kind: ServiceKind,
}

#[derive(Serialize)]
struct RemovedConnectionLabel {
    provider: &'static str,
    request_id: String,
    utility_id: &'static str,
    currency: &'static str,
    accept_provider_settings: bool,
    name: &'static str,
}

#[derive(Serialize)]
struct RemovedRename {
    name: &'static str,
    expected_name: &'static str,
}

#[tokio::test]
async fn public_accounts_have_only_canonical_identifiers_and_typed_services() {
    let app = support::App::start().await;
    let page: Page<AccountContract> = reqwest::Client::new()
        .get(format!("{}/v1/accounts", app.url))
        .bearer_auth(support::TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    let account = page.items.iter().find(|item| item.id == "demo-wm").unwrap();
    assert_eq!(account.provider, ProviderId::WasteManagement);
    assert_eq!(account.connection_id, None);
    assert_eq!(
        account.source,
        AccountSource::Live(ha_utilities::models::LiveProvider::WasteManagement)
    );
    assert_eq!(account.services[0].id, "demo-waste");
    assert_eq!(account.services[0].kind, ServiceKind::Waste);
    app.close().await;
}

#[tokio::test]
async fn removed_connection_label_and_rename_route_are_rejected_at_the_http_boundary() {
    let app = support::App::start().await;
    let client = reqwest::Client::new();
    let result = client
        .post(format!("{}/v1/connections", app.url))
        .bearer_auth(support::TOKEN)
        .json(&RemovedConnectionLabel {
            provider: "bayou",
            request_id: uuid::Uuid::now_v7().to_string(),
            utility_id: "speculoos_power",
            currency: "USD",
            accept_provider_settings: true,
            name: "Obsolete custom label",
        })
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::BAD_REQUEST);
    let result = client
        .post(format!(
            "{}/v1/connections/fictional-connection/rename",
            app.url
        ))
        .json(&RemovedRename {
            name: "Obsolete new label",
            expected_name: "Obsolete old label",
        })
        .send()
        .await
        .unwrap();
    assert_eq!(result.status(), StatusCode::NOT_FOUND);
    app.close().await;
}
