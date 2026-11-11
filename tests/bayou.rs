//! Fictional provider responses exercise the actual HTTP adapter, not copied mapping logic.
use std::sync::{Arc, Mutex};

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, NaiveDate, Utc};
use ha_utilities::{
    config::{AccountBinding, BayouConfig, ServiceBinding},
    models::{Capability, CoverageState, Observation, ProviderId, ServiceKind, UsageUnit},
    utility::{
        Continuation, HistoryRequest, SyncPage, SyncRequest, Utility, UtilityError, bayou::Bayou,
    },
};
use rust_decimal::Decimal;
use serde::Serialize;
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use url::Url;

const KEY: &str = "fictional-bayou-key";
const CUSTOMER_ID: &str = "71001";
const ACCOUNT_ID: &str = "fictional-account-a";
const METER_ID: &str = "fictional-electric-meter";
const SCE: &str = "southern_california_edison";
const SCE_REGISTERED_METER: &str = "10101-20202";
const SCE_INTERVAL_METER: &str = "1010120202";

#[derive(Clone, Debug)]
struct Request {
    method: String,
    url: Url,
    authorization: Option<String>,
}

struct Reply {
    status: u16,
    body: String,
    headers: Vec<(String, String)>,
}

impl Reply {
    fn ok(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            body: body.into(),
            headers: Vec::new(),
        }
    }
    fn status(status: u16) -> Self {
        Self {
            status,
            body: "{\"error\":\"fictional-provider-error\"}".to_owned(),
            headers: Vec::new(),
        }
    }
}

struct Fixture {
    endpoint: Url,
    requests: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
    _files: TempDir,
    config: BayouConfig,
}

impl Fixture {
    async fn new(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = Url::parse(&format!(
            "http://{}/api/v2/",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0_u8; 1024];
                while !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert_ne!(read, 0);
                    bytes.extend_from_slice(&chunk[..read]);
                    assert!(bytes.len() < 65536);
                }
                let text = String::from_utf8(bytes).unwrap();
                let mut lines = text.lines();
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().to_owned();
                let url = Url::parse(&format!("http://fixture.invalid{}", first.next().unwrap()))
                    .unwrap();
                let authorization = lines.find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("authorization")
                        .then(|| value.trim().to_owned())
                });
                let request = Request {
                    method,
                    url,
                    authorization,
                };
                captured.lock().unwrap().push(request.clone());
                let reply = handler(&request);
                let mut headers = String::new();
                for (key, value) in reply.headers {
                    headers.push_str(&format!("{key}: {value}\r\n"));
                }
                let response = format!(
                    "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
                // A bounded client may close immediately after rejecting a large
                // Content-Length, before consuming the fixture response body.
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let files = tempfile::tempdir().unwrap();
        let key_file = files.path().join("api-key");
        std::fs::write(&key_file, KEY).unwrap();
        Self {
            endpoint,
            requests,
            task,
            _files: files,
            config: BayouConfig {
                api_key_file: key_file,
            },
        }
    }

    fn adapter(&self) -> Bayou {
        Bayou::with_endpoint(&self.config, self.endpoint.clone()).unwrap()
    }

    fn assert_read_only_auth(&self) {
        let requests = self.requests.lock().unwrap();
        assert!(!requests.is_empty());
        for request in requests.iter() {
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.authorization,
                Some(format!("Basic {}", STANDARD.encode(format!("{KEY}:"))))
            );
            assert!(
                !request
                    .url
                    .query_pairs()
                    .any(|(name, _)| matches!(name.as_ref(), "key" | "token" | "api_key"))
            );
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn binding() -> AccountBinding {
    AccountBinding {
        id: "demo-energy".to_owned(),
        name: "Example energy".to_owned(),
        deployment: "example-installation".to_owned(),
        provider: ProviderId::Bayou,
        currency: "USD".to_owned(),
        customer_id: CUSTOMER_ID.to_owned(),
        provider_account: ACCOUNT_ID.to_owned(),
        services: vec![ServiceBinding {
            id: "demo-electric".to_owned(),
            name: "Example electric".to_owned(),
            provider_service: METER_ID.to_owned(),
            kind: ServiceKind::Electricity,
        }],
    }
}

fn day(value: &str) -> NaiveDate {
    value.parse().unwrap()
}
fn time(value: &str) -> DateTime<Utc> {
    value.parse().unwrap()
}
fn request(from: &str, to: &str) -> SyncRequest {
    SyncRequest {
        binding: binding(),
        from: day(from),
        to: day(to),
        continuation: None,
        observed_at: time("2026-10-04T12:00:00Z"),
    }
}

#[derive(Serialize)]
struct Customer {
    id: u64,
    external_id: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    utility: Option<&'static str>,
    is_currently_authenticated: bool,
    auth_status: &'static str,
    bills_are_ready: bool,
    utility_has_intervals: bool,
    account_has_intervals: bool,
    intervals_are_ready: bool,
    first_interval_discovered: Option<&'static str>,
    last_interval_discovered: Option<&'static str>,
    account_numbers: Vec<Account>,
    onboarding_token: &'static str,
}
#[derive(Serialize)]
struct Account {
    id: &'static str,
    meters: Vec<Meter>,
}
#[derive(Serialize)]
struct Meter {
    id: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
}

fn customer() -> Customer {
    // Deliberately put a different account first to catch first-account selection.
    Customer {
        id: 71001,
        external_id: "fictional-customer",
        utility: None,
        is_currently_authenticated: true,
        auth_status: "authenticated",
        bills_are_ready: true,
        utility_has_intervals: true,
        account_has_intervals: true,
        intervals_are_ready: true,
        first_interval_discovered: None,
        last_interval_discovered: None,
        account_numbers: vec![
            Account {
                id: "unbound-fictional-account",
                meters: vec![Meter {
                    id: "unbound-fictional-meter",
                    kind: "gas",
                }],
            },
            Account {
                id: ACCOUNT_ID,
                meters: vec![Meter {
                    id: METER_ID,
                    kind: "electric",
                }],
            },
        ],
        onboarding_token: "fictional-onboarding-token-must-not-escape",
    }
}

#[derive(Serialize)]
struct Bill {
    id: u64,
    customer_id: u64,
    account_number: &'static str,
    status: &'static str,
    billed_on: Option<&'static str>,
    billing_period_from: &'static str,
    billing_period_to: &'static str,
    outstanding_balance: i64,
    total_amount: Option<i64>,
    meters: Vec<Meter>,
    file_url: &'static str,
}

fn bill() -> Bill {
    Bill {
        id: 83001,
        customer_id: 71001,
        account_number: ACCOUNT_ID,
        status: "unlocked",
        billed_on: Some("2026-10-01"),
        billing_period_from: "2026-09-01",
        billing_period_to: "2026-09-30",
        outstanding_balance: 4500,
        total_amount: Some(12050),
        meters: vec![Meter {
            id: METER_ID,
            kind: "electric",
        }],
        file_url: "https://example.invalid/fictional-bill?signature=not-a-real-secret",
    }
}

#[derive(Serialize)]
struct IntervalResponse {
    meters: Vec<IntervalMeter>,
}
#[derive(Serialize)]
struct IntervalMeter {
    id: &'static str,
    intervals: Vec<Interval>,
}
#[derive(Serialize)]
struct Interval {
    start: &'static str,
    end: &'static str,
    electricity_consumption: Option<i64>,
    gas_consumption: Option<&'static str>,
}

fn interval_body(rows: Vec<Interval>) -> String {
    meter_interval_body(METER_ID, rows)
}

fn meter_interval_body(meter: &'static str, rows: Vec<Interval>) -> String {
    serde_json::to_string(&IntervalResponse {
        meters: vec![IntervalMeter {
            id: meter,
            intervals: rows,
        }],
    })
    .unwrap()
}

fn daily(start: &'static str, end: &'static str, amount: i64) -> Interval {
    Interval {
        start,
        end,
        electricity_consumption: Some(amount),
        gas_consumption: None,
    }
}

fn default_reply(request: &Request) -> Reply {
    match request.url.path() {
        "/api/v2/customers/71001" => Reply::ok(serde_json::to_string(&customer()).unwrap()),
        "/api/v2/customers/71001/bills" => {
            let pairs = request
                .url
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(pairs.get("account_number").unwrap(), ACCOUNT_ID);
            assert!(!pairs.contains_key("limit"));
            assert_eq!(pairs.get("start"), pairs.get("end"));
            if pairs.get("start").unwrap() == "2026-10-01" {
                Reply::ok(serde_json::to_string(&vec![bill()]).unwrap())
            } else {
                Reply::ok("[]")
            }
        }
        "/api/v2/customers/71001/intervals" => {
            let pairs = request
                .url
                .query_pairs()
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(pairs.get("account_number").unwrap(), ACCOUNT_ID);
            assert_eq!(pairs.get("meter_id").unwrap(), METER_ID);
            assert_eq!(pairs.get("latest_only").unwrap(), "true");
            if pairs.get("start").unwrap().starts_with("2026-10-01") {
                Reply::ok(interval_body(vec![
                    daily("2026-10-01T00:00:00Z", "2026-10-02T00:00:00Z", 1000),
                    daily("2026-10-02T00:00:00Z", "2026-10-03T00:00:00Z", 2000),
                ]))
            } else {
                Reply::ok(interval_body(vec![daily(
                    "2026-10-02T00:00:00Z",
                    "2026-10-03T00:00:00Z",
                    2000,
                )]))
            }
        }
        _ => Reply::status(404),
    }
}

async fn sync_all(adapter: &Bayou, mut request: SyncRequest) -> Vec<SyncPage> {
    let mut pages = Vec::new();
    for _ in 0..32 {
        let page = adapter.sync(request.clone()).await.unwrap();
        request.continuation = page.continuation.clone();
        let done = request.continuation.is_none();
        pages.push(page);
        if done {
            return pages;
        }
    }
    panic!("fictional sync did not terminate");
}

#[tokio::test]
async fn all_history_reads_undated_bills_and_provider_bounded_intervals() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let discovery_pending = AtomicBool::new(true);
    let fixture = Fixture::new(move |request| {
        let query = request
            .url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        match request.url.path() {
            "/api/v2/customers/71001" => {
                let mut customer = customer();
                if discovery_pending.swap(false, Ordering::Relaxed) {
                    customer.account_has_intervals = false;
                    customer.intervals_are_ready = false;
                } else {
                    customer.first_interval_discovered = Some("2024-01-01T00:00:00Z");
                    customer.last_interval_discovered = Some("2024-02-01T00:00:00Z");
                }
                Reply::ok(serde_json::to_string(&customer).unwrap())
            }
            "/api/v2/customers/71001/bills" => {
                assert_eq!(query.len(), 1);
                assert_eq!(query["account_number"], ACCOUNT_ID);
                let mut bill = bill();
                bill.billed_on = Some("2023-12-05");
                bill.billing_period_from = "2023-11-01";
                bill.billing_period_to = "2023-11-30";
                Reply::ok(serde_json::to_string(&vec![bill]).unwrap())
            }
            "/api/v2/customers/71001/intervals" => {
                assert_eq!(query["meter_id"], METER_ID);
                assert_eq!(query["account_number"], ACCOUNT_ID);
                assert_eq!(query["latest_only"], "true");
                if query["start"].starts_with("2024-01-01") {
                    assert!(query["end"].starts_with("2024-01-31"));
                    Reply::ok(interval_body(vec![daily(
                        "2024-01-01T00:00:00Z",
                        "2024-01-02T00:00:00Z",
                        10,
                    )]))
                } else {
                    assert!(query["start"].starts_with("2024-01-29"));
                    Reply::ok(interval_body(vec![daily(
                        "2024-02-01T00:00:00Z",
                        "2024-02-02T00:00:00Z",
                        11,
                    )]))
                }
            }
            _ => Reply::status(404),
        }
    })
    .await;
    let adapter = fixture.adapter();
    let mut request = HistoryRequest {
        binding: binding(),
        continuation: None,
        observed_at: time("2026-10-04T12:00:00Z"),
    };
    let first = adapter.sync_history(request.clone()).await.unwrap();
    assert!(
        first.observations.iter().any(
            |row| matches!(row, Observation::Bill(bill) if bill.issued_on == day("2023-12-05"))
        )
    );
    request.continuation = first.continuation;
    assert!(request.continuation.is_some());
    let middle = adapter.sync_history(request.clone()).await.unwrap();
    assert!(middle.observations.iter().any(|row| matches!(row, Observation::Usage(value) if value.start == time("2024-01-01T00:00:00Z"))));
    request.continuation = middle.continuation;
    assert!(request.continuation.is_some());
    let last = adapter.sync_history(request).await.unwrap();
    assert!(last.continuation.is_none());
    assert!(last.observations.iter().any(|row| matches!(row, Observation::Usage(value) if value.start == time("2024-02-01T00:00:00Z"))));
    let bills = last
        .coverage
        .iter()
        .find(|row| row.capability == Capability::Bills)
        .unwrap();
    assert_eq!(bills.state, CoverageState::Complete);
    assert!(bills.from.is_none() && bills.to.is_none());
}

#[tokio::test]
async fn sce_intervals_use_wire_meter_id_without_changing_bill_or_usage_identity() {
    let fixture = Fixture::new(|request| {
        let pairs = request
            .url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        match request.url.path() {
            "/api/v2/customers/71001" => {
                let mut data = customer();
                data.utility = Some(SCE);
                data.account_numbers[1].meters[0].id = SCE_REGISTERED_METER;
                Reply::ok(serde_json::to_string(&data).unwrap())
            }
            "/api/v2/customers/71001/bills" if pairs["start"] == "2026-10-01" => {
                assert_eq!(pairs["account_number"], ACCOUNT_ID);
                let mut data = bill();
                data.meters[0].id = SCE_REGISTERED_METER;
                Reply::ok(serde_json::to_string(&vec![data]).unwrap())
            }
            "/api/v2/customers/71001/intervals" => {
                assert_eq!(pairs["account_number"], ACCOUNT_ID);
                assert_eq!(pairs["latest_only"], "true");
                if pairs["meter_id"] != SCE_INTERVAL_METER {
                    return Reply::status(404);
                }
                let mut intervals =
                    vec![daily("2026-10-02T00:00:00Z", "2026-10-03T00:00:00Z", 2000)];
                if pairs["start"].starts_with("2026-10-01") {
                    intervals.insert(
                        0,
                        daily("2026-10-01T00:00:00Z", "2026-10-02T00:00:00Z", 1000),
                    );
                }
                Reply::ok(meter_interval_body(SCE_INTERVAL_METER, intervals))
            }
            _ => default_reply(request),
        }
    })
    .await;
    let adapter = fixture.adapter();
    let mut initial = request("2026-10-01", "2026-10-02");
    initial.binding.services[0].provider_service = SCE_REGISTERED_METER.to_owned();
    let pages = sync_all(&adapter, initial.clone()).await;
    assert!(
        pages.iter().flat_map(|page| &page.observations).any(
            |value| matches!(value, Observation::Bill(bill) if bill.total.minor_units == 12050)
        )
    );
    assert!(
        pages
            .last()
            .unwrap()
            .coverage
            .iter()
            .any(|value| value.capability == Capability::Usage
                && value.state == CoverageState::Complete)
    );
    let mut usage = pages
        .into_iter()
        .flat_map(|page| page.observations)
        .filter_map(|value| match value {
            Observation::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 2);
    let october_second = usage.pop().unwrap();
    assert_eq!(october_second.service_id, initial.binding.services[0].id);
    assert_eq!(october_second.quantity, Decimal::from(2000));
    initial.from = initial.to;
    let repeated = sync_all(&adapter, initial).await;
    let repeated_usage = repeated
        .iter()
        .flat_map(|page| &page.observations)
        .filter_map(|value| match value {
            Observation::Usage(usage) => Some(usage),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(repeated_usage.len(), 1);
    assert_eq!(repeated_usage[0].id, october_second.id);
    fixture.assert_read_only_auth();
}

#[tokio::test]
async fn sce_interval_wire_id_must_be_unambiguous_across_the_customer() {
    for (registered, alias, account) in [
        (SCE_REGISTERED_METER, SCE_INTERVAL_METER, 1),
        (SCE_INTERVAL_METER, "1010-120202", 0),
    ] {
        let fixture = Fixture::new(move |request| {
            if request.url.path() == "/api/v2/customers/71001" {
                let mut data = customer();
                data.utility = Some(SCE);
                data.account_numbers[1].meters[0].id = registered;
                data.account_numbers[account].meters.push(Meter {
                    id: alias,
                    kind: "electric",
                });
                return Reply::ok(serde_json::to_string(&data).unwrap());
            }
            default_reply(request)
        })
        .await;
        let adapter = fixture.adapter();
        let mut input = request("2026-10-02", "2026-10-02");
        input.binding.services[0].provider_service = registered.to_owned();
        input.continuation = adapter.sync(input.clone()).await.unwrap().continuation;
        assert!(matches!(
            adapter.sync(input).await,
            Err(UtilityError::BindingMismatch)
        ));
        assert_eq!(
            fixture.requests.lock().unwrap().len(),
            3,
            "ambiguous wire IDs must be rejected before the interval request"
        );
    }
}

#[tokio::test]
async fn sce_interval_meter_conversion_is_scoped_to_the_exact_wire_contract() {
    for (utility, registered, selected, wire, returned) in [
        (
            None,
            SCE_REGISTERED_METER,
            SCE_REGISTERED_METER,
            SCE_REGISTERED_METER,
            SCE_REGISTERED_METER,
        ),
        (
            Some("fictional-utility"),
            SCE_REGISTERED_METER,
            SCE_REGISTERED_METER,
            SCE_REGISTERED_METER,
            SCE_REGISTERED_METER,
        ),
        (
            Some(SCE),
            "101-01-20202",
            "101-01-20202",
            "101-01-20202",
            "101-01-20202",
        ),
        (Some(SCE), "0001-02", "0001-02", "000102", "000102"),
        (
            Some(SCE),
            SCE_REGISTERED_METER,
            SCE_REGISTERED_METER,
            SCE_INTERVAL_METER,
            SCE_REGISTERED_METER,
        ),
        (
            Some(SCE),
            SCE_INTERVAL_METER,
            SCE_REGISTERED_METER,
            SCE_INTERVAL_METER,
            SCE_INTERVAL_METER,
        ),
    ] {
        let fixture = Fixture::new(move |request| match request.url.path() {
            "/api/v2/customers/71001" => {
                let mut data = customer();
                data.utility = utility;
                data.account_numbers[1].meters[0].id = registered;
                Reply::ok(serde_json::to_string(&data).unwrap())
            }
            "/api/v2/customers/71001/intervals" => {
                assert!(
                    request
                        .url
                        .query_pairs()
                        .any(|(k, v)| k == "meter_id" && v == wire)
                );
                Reply::ok(meter_interval_body(returned, Vec::new()))
            }
            _ => default_reply(request),
        })
        .await;
        let adapter = fixture.adapter();
        let mut input = request("2026-10-02", "2026-10-02");
        input.binding.services[0].provider_service = selected.to_owned();
        let first = adapter.sync(input.clone()).await;
        if registered != selected {
            assert!(matches!(first, Err(UtilityError::BindingMismatch)));
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
            continue;
        }
        input.continuation = first.unwrap().continuation;
        let result = adapter.sync(input).await;
        if returned == wire {
            assert!(result.is_ok());
        } else {
            assert!(matches!(result, Err(UtilityError::BindingMismatch)));
        }
    }
}

#[tokio::test]
async fn selected_meter_usage_keeps_combined_account_bill_totals() {
    let fixture = Fixture::new(|request| {
        if request.url.path() == "/api/v2/customers/71001" {
            let mut value = customer();
            value
                .account_numbers
                .iter_mut()
                .find(|account| account.id == ACCOUNT_ID)
                .unwrap()
                .meters
                .push(Meter {
                    id: "same-account-gas",
                    kind: "gas",
                });
            Reply::ok(serde_json::to_string(&value).unwrap())
        } else if request.url.path() == "/api/v2/customers/71001/bills"
            && request
                .url
                .query_pairs()
                .any(|(key, value)| key == "start" && value == "2026-10-01")
        {
            let mut value = bill();
            value.meters.push(Meter {
                id: "same-account-gas",
                kind: "gas",
            });
            Reply::ok(serde_json::to_string(&vec![value]).unwrap())
        } else {
            default_reply(request)
        }
    })
    .await;
    let pages = sync_all(&fixture.adapter(), request("2026-10-01", "2026-10-02")).await;
    let observations = pages
        .into_iter()
        .flat_map(|page| page.observations)
        .collect::<Vec<_>>();
    let totals = observations
        .iter()
        .filter_map(|value| match value {
            Observation::Bill(bill) => Some(bill.total.minor_units),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(totals, vec![12050]);
    assert!(observations.iter().any(
        |value| matches!(value,Observation::Usage(usage) if usage.service_id=="demo-electric")
    ));
    assert!(!observations.iter().any(
        |value| matches!(value,Observation::Service(service) if service.kind==ServiceKind::Gas)
    ));
    fixture.assert_read_only_auth();
}

#[tokio::test]
async fn sync_pages_scope_accounts_preserve_bill_meaning_and_reconcile_interval_windows() {
    let fixture = Fixture::new(default_reply).await;
    let adapter = fixture.adapter();
    let pages = sync_all(&adapter, request("2026-10-01", "2026-10-02")).await;
    assert_eq!(pages.len(), 4);
    assert!(pages[..3].iter().all(|page| page.continuation.is_some()));
    let last = pages.last().unwrap();
    for capability in [Capability::Bills, Capability::Services, Capability::Usage] {
        assert!(
            last.coverage
                .iter()
                .any(|item| item.capability == capability && item.state == CoverageState::Complete)
        );
    }
    let balance = last
        .coverage
        .iter()
        .find(|item| item.capability == Capability::Balances)
        .unwrap();
    assert_eq!(balance.state, CoverageState::Partial);
    assert_eq!(
        balance.reason.as_deref(),
        Some("previous_cycle_balance_only")
    );
    let mut bills = Vec::new();
    let mut usage = Vec::new();
    for observation in pages.iter().flat_map(|page| &page.observations) {
        match observation {
            Observation::Bill(value) => bills.push(value),
            Observation::Usage(value) => usage.push(value),
            Observation::Balance(_) | Observation::Payment(_) => {
                panic!("Bayou must not invent current due or payment settlement")
            }
            _ => (),
        }
    }
    assert_eq!(bills.len(), 1);
    assert_eq!(bills[0].account_id, "demo-energy");
    assert_eq!(bills[0].total.minor_units, 12050);
    assert_eq!(
        bills[0].outstanding_balance.as_ref().unwrap().minor_units,
        4500
    );
    assert_eq!(bills[0].source_status.as_deref(), Some("unlocked"));
    assert_eq!(bills[0].due_on, None);
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].quantity, Decimal::from(1000));
    assert_eq!(usage[1].quantity, Decimal::from(2000));
    assert_eq!(usage[0].unit, UsageUnit::Wh);
    assert_ne!(usage[0].id, usage[1].id);
    assert!(usage.iter().all(|row| row.service_id == "demo-electric"));
    let serialized = serde_json::to_string(
        &pages
            .iter()
            .flat_map(|p| &p.observations)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    for private in [
        ACCOUNT_ID,
        CUSTOMER_ID,
        "onboarding-token",
        "file_url",
        "signature",
        KEY,
    ] {
        assert!(!serialized.contains(private));
    }
    fixture.assert_read_only_auth();
}

#[tokio::test]
async fn documented_interval_array_numeric_meter_and_fractional_gas_are_supported() {
    let fixture = Fixture::new(|request| {
        if request.url.path().ends_with("/intervals") {
            return Reply::ok("[{\"meters\":[{\"id\":4201,\"intervals\":[{\"start\":\"2026-10-01T00:00:00Z\",\"end\":\"2026-10-02T00:00:00Z\",\"gas_consumption\":1.234}]}]}]");
        }
        if request.url.path().ends_with("/bills") { return Reply::ok("[]"); }
        let mut data = customer();
        data.account_numbers[1].meters = vec![Meter { id: "4201", kind: "gas" }];
        Reply::ok(serde_json::to_string(&data).unwrap())
    }).await;
    let mut input = request("2026-10-01", "2026-10-01");
    input.binding.services[0].kind = ServiceKind::Gas;
    input.binding.services[0].provider_service = "4201".to_owned();
    let pages = sync_all(&fixture.adapter(), input).await;
    let usage = pages
        .last()
        .unwrap()
        .observations
        .iter()
        .find_map(|row| {
            if let Observation::Usage(usage) = row {
                Some(usage)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(usage.unit, UsageUnit::Therm);
    assert_eq!(usage.quantity.to_string(), "1.234");
    fixture.assert_read_only_auth();
}

#[tokio::test]
async fn bill_currency_uses_explicit_binding_instead_of_assuming_usd() {
    let fixture = Fixture::new(default_reply).await;
    let adapter = fixture.adapter();
    let mut input = request("2026-10-01", "2026-10-01");
    input.binding.currency = "CAD".to_owned();
    let page = adapter.sync(input).await.unwrap();
    let bill = page
        .observations
        .iter()
        .find_map(|observation| {
            if let Observation::Bill(bill) = observation {
                Some(bill)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(bill.total.currency, "CAD");
    assert_eq!(bill.outstanding_balance.as_ref().unwrap().currency, "CAD");
    fixture.assert_read_only_auth();
}

#[tokio::test]
async fn local_time_daily_intervals_crossing_utc_midnight_are_not_lost() {
    let fixture = Fixture::new(|request| {
        if request.url.path().ends_with("/intervals") {
            let start = request
                .url
                .query_pairs()
                .find(|(name, _)| name == "start")
                .unwrap()
                .1;
            return if start.starts_with("2026-10-01") {
                Reply::ok(interval_body(vec![daily(
                    "2026-10-01T07:00:00Z",
                    "2026-10-02T07:00:00Z",
                    3210,
                )]))
            } else {
                Reply::ok(interval_body(vec![daily(
                    "2026-10-02T07:00:00Z",
                    "2026-10-03T07:00:00Z",
                    4320,
                )]))
            };
        }
        default_reply(request)
    })
    .await;
    let pages = sync_all(&fixture.adapter(), request("2026-10-01", "2026-10-02")).await;
    let usage = pages
        .iter()
        .flat_map(|p| &p.observations)
        .filter(|o| matches!(o, Observation::Usage(_)))
        .count();
    assert_eq!(usage, 2);
    assert!(
        pages
            .last()
            .unwrap()
            .coverage
            .iter()
            .any(|c| c.capability == Capability::Usage
                && c.state == CoverageState::Partial
                && c.reason.as_deref() == Some("interval_gaps_or_data_not_ready"))
    );
}

#[tokio::test]
async fn bindings_are_rechecked_before_each_page_and_foreign_bills_never_escape() {
    let fixture = Fixture::new(|request| {
        if request.url.path().ends_with("/bills") {
            let mut foreign = bill();
            foreign.account_number = "different-fictional-account";
            return Reply::ok(serde_json::to_string(&vec![foreign]).unwrap());
        }
        default_reply(request)
    })
    .await;
    assert!(matches!(
        fixture
            .adapter()
            .sync(request("2026-10-01", "2026-10-01"))
            .await,
        Err(UtilityError::BindingMismatch)
    ));
    fixture.assert_read_only_auth();

    let fixture = Fixture::new(default_reply).await;
    let adapter = fixture.adapter();
    let mut input = request("2026-10-01", "2026-10-02");
    input.continuation = adapter.sync(input.clone()).await.unwrap().continuation;
    input.binding.provider_account = "different-fictional-account".to_owned();
    assert!(matches!(
        adapter.sync(input).await,
        Err(UtilityError::InvalidContinuation)
    ));
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        2,
        "changed binding must be rejected before more HTTP"
    );
}

#[tokio::test]
async fn missing_or_duplicate_bound_accounts_and_meter_type_mismatch_are_rejected() {
    for kind in ["missing", "duplicate", "wrong-type", "wrong-customer"] {
        let fixture = Fixture::new(move |_| {
            let mut data = customer();
            match kind {
                "missing" => data.account_numbers.truncate(1),
                "duplicate" => data.account_numbers.push(Account {
                    id: ACCOUNT_ID,
                    meters: vec![Meter {
                        id: METER_ID,
                        kind: "electric",
                    }],
                }),
                "wrong-type" => data.account_numbers[1].meters[0].kind = "gas",
                "wrong-customer" => data.id = 99999,
                _ => unreachable!(),
            }
            Reply::ok(serde_json::to_string(&data).unwrap())
        })
        .await;
        assert!(
            matches!(
                fixture
                    .adapter()
                    .sync(request("2026-10-01", "2026-10-01"))
                    .await,
                Err(UtilityError::BindingMismatch)
            ),
            "case: {kind}"
        );
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn auth_rate_limit_malformed_and_redirect_failures_are_typed_and_redacted() {
    for status in [401, 403, 404, 429, 500, 302] {
        let fixture = Fixture::new(move |_| {
            let mut reply = Reply::status(status);
            reply.body = "fictional-key-response-must-not-escape".to_owned();
            if status == 302 {
                reply.headers.push((
                    "Location".to_owned(),
                    "http://127.0.0.1:1/stolen".to_owned(),
                ));
            }
            reply
        })
        .await;
        let error = fixture
            .adapter()
            .sync(request("2026-10-01", "2026-10-01"))
            .await
            .unwrap_err();
        match status {
            401 => assert!(matches!(error, UtilityError::Authentication)),
            403 => assert!(matches!(error, UtilityError::AccessDenied)),
            404 => assert!(matches!(error, UtilityError::BindingMismatch)),
            429 => assert!(matches!(error, UtilityError::RateLimited)),
            500 => assert!(matches!(error, UtilityError::Unavailable)),
            302 => assert!(matches!(error, UtilityError::InvalidResponse)),
            _ => unreachable!(),
        }
        assert!(!error.to_string().contains("fictional-key-response"));
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    }
    let fixture = Fixture::new(|_| Reply::ok("{\"id\":\"wrong-type\"}")).await;
    assert!(matches!(
        fixture
            .adapter()
            .sync(request("2026-10-01", "2026-10-01"))
            .await,
        Err(UtilityError::InvalidResponse)
    ));
}

#[tokio::test]
async fn locked_unparsed_stale_and_unavailable_data_are_not_complete() {
    let fixture = Fixture::new(|request| {
        if request.url.path().ends_with("/bills") {
            let mut locked = bill();
            locked.status = "locked";
            locked.total_amount = None;
            return Reply::ok(serde_json::to_string(&vec![locked]).unwrap());
        }
        if request.url.path().ends_with("/intervals") {
            return Reply::ok(interval_body(Vec::new()));
        }
        let mut data = customer();
        data.is_currently_authenticated = false;
        data.auth_status = "refresh_mfa_needed";
        Reply::ok(serde_json::to_string(&data).unwrap())
    })
    .await;
    let pages = sync_all(&fixture.adapter(), request("2026-10-01", "2026-10-01")).await;
    assert!(
        !pages
            .iter()
            .flat_map(|p| &p.observations)
            .any(|o| matches!(o, Observation::Bill(_)))
    );
    for capability in [Capability::Bills, Capability::Usage] {
        assert!(
            pages
                .last()
                .unwrap()
                .coverage
                .iter()
                .any(|c| c.capability == capability
                    && c.state == CoverageState::Partial
                    && c.reason.as_deref() == Some("provider_reauthentication_required"))
        );
    }
    fixture.assert_read_only_auth();
}

#[tokio::test]
async fn empty_usage_and_gaps_remain_partial_and_overlaps_are_rejected() {
    for invalid in [false, true] {
        let fixture = Fixture::new(move |request| {
            if request.url.path().ends_with("/intervals") {
                return Reply::ok(interval_body(if invalid {
                    vec![
                        daily("2026-10-01T00:00:00Z", "2026-10-01T01:00:00Z", 100),
                        daily("2026-10-01T00:30:00Z", "2026-10-01T02:00:00Z", 200),
                    ]
                } else {
                    Vec::new()
                }));
            }
            default_reply(request)
        })
        .await;
        let adapter = fixture.adapter();
        let mut input = request("2026-10-01", "2026-10-01");
        input.continuation = adapter.sync(input.clone()).await.unwrap().continuation;
        let page = adapter.sync(input).await;
        if invalid {
            assert!(matches!(page, Err(UtilityError::InvalidResponse)));
        } else {
            assert!(page.unwrap().coverage.iter().any(|c|c.capability==Capability::Usage && c.state==CoverageState::Partial));
        }
    }
}

#[tokio::test]
async fn foreign_meter_and_negative_quantities_are_rejected() {
    for payload in [
        "{\"meters\":[{\"id\":\"foreign-fictional-meter\",\"intervals\":[]}]}",
        "{\"meters\":[{\"id\":\"fictional-electric-meter\",\"intervals\":[{\"start\":\"2026-10-01T00:00:00Z\",\"end\":\"2026-10-02T00:00:00Z\",\"electricity_consumption\":-10}]}]}",
    ] {
        let fixture = Fixture::new(move |request| {
            if request.url.path().ends_with("/intervals") {
                Reply::ok(payload)
            } else {
                default_reply(request)
            }
        })
        .await;
        let adapter = fixture.adapter();
        let mut input = request("2026-10-01", "2026-10-01");
        input.continuation = adapter.sync(input.clone()).await.unwrap().continuation;
        assert!(adapter.sync(input).await.is_err());
    }
}

#[tokio::test]
async fn invalid_credentials_origins_dates_and_cursors_never_make_http_requests() {
    let fixture = Fixture::new(default_reply).await;
    let adapter = fixture.adapter();
    let mut input = request("2026-10-02", "2026-10-01");
    assert!(matches!(
        adapter.sync(input.clone()).await,
        Err(UtilityError::InvalidResponse)
    ));
    input = request("2026-10-01", "2026-10-01");
    input.continuation = Some(Continuation("not-base64".to_owned()));
    assert!(matches!(
        adapter.sync(input).await,
        Err(UtilityError::InvalidContinuation)
    ));
    assert!(
        Bayou::with_endpoint(
            &fixture.config,
            Url::parse("https://example.invalid/api/v2/").unwrap()
        )
        .is_err()
    );
    assert!(
        Bayou::with_endpoint(
            &fixture.config,
            Url::parse("http://user:password@127.0.0.1/api/v2/").unwrap()
        )
        .is_err()
    );
    std::fs::write(&fixture.config.api_key_file, "fictional:key").unwrap();
    assert!(matches!(
        Bayou::with_endpoint(&fixture.config, fixture.endpoint.clone()),
        Err(UtilityError::NotConfigured)
    ));
    assert!(fixture.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn unsupported_account_interval_coverage_does_not_trigger_fetch_or_linking() {
    for utility_supported in [true, false] {
        let fixture = Fixture::new(move |request| {
            if request.url.path().ends_with("/bills") {
                return Reply::ok("[]");
            }
            let mut data = customer();
            data.account_has_intervals = false;
            data.utility_has_intervals = utility_supported;
            Reply::ok(serde_json::to_string(&data).unwrap())
        })
        .await;
        let pages = sync_all(&fixture.adapter(), request("2026-10-01", "2026-10-01")).await;
        assert_eq!(pages.len(), 1);
        let coverage = pages[0]
            .coverage
            .iter()
            .find(|item| item.capability == Capability::Usage)
            .unwrap();
        assert_eq!(
            coverage.state,
            if utility_supported {
                CoverageState::Unavailable
            } else {
                CoverageState::Unsupported
            }
        );
        assert_eq!(fixture.requests.lock().unwrap().len(), 2);
        fixture.assert_read_only_auth();
    }
}

#[tokio::test]
async fn continuation_rechecks_current_customer_binding_before_more_data() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = AtomicUsize::new(0);
    let fixture = Fixture::new(move |request| {
        if request.url.path() == "/api/v2/customers/71001" {
            let mut data = customer();
            if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                data.account_numbers.truncate(1);
            }
            return Reply::ok(serde_json::to_string(&data).unwrap());
        }
        default_reply(request)
    })
    .await;
    let adapter = fixture.adapter();
    let mut input = request("2026-10-01", "2026-10-02");
    input.continuation = adapter.sync(input.clone()).await.unwrap().continuation;
    assert!(matches!(
        adapter.sync(input).await,
        Err(UtilityError::BindingMismatch)
    ));
    assert_eq!(fixture.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn oversized_provider_response_is_rejected_before_deserialization() {
    let fixture = Fixture::new(|_| Reply::ok(" ".repeat(8 * 1024 * 1024 + 1))).await;
    assert!(matches!(
        fixture
            .adapter()
            .sync(request("2026-10-01", "2026-10-01"))
            .await,
        Err(UtilityError::InvalidResponse)
    ));
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}
