//! Fictional published WM response shapes exercise the real HTTP adapter.
use chrono::{DateTime, NaiveDate, Utc};
use ha_utilities::{
    config::{AccountBinding, ServiceBinding},
    models::*,
    utility::{
        HistoryRequest, SyncRequest, Utility, UtilityError, waste_management::WasteManagement,
    },
};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct Fixture {
    url: url::Url,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new(handler: impl Fn(&str) -> (u16, String) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = url::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut chunk = [0; 1024];
                while !bytes.windows(4).any(|p| p == b"\r\n\r\n") {
                    let read = stream.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..read]);
                    assert!(bytes.len() < 16384);
                }
                let request = String::from_utf8(bytes).unwrap();
                let path = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let (status, body) = handler(path);
                captured.lock().unwrap().push(request);
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    fn adapter(&self) -> WasteManagement {
        WasteManagement::with_endpoint(
            self.url.clone(),
            "fictional-wm-token".to_owned(),
            "fictional-client-id".to_owned(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn request() -> SyncRequest {
    SyncRequest {
        binding: AccountBinding {
            id: "demo-wm".to_owned(),
            deployment: "demo".to_owned(),
            provider: ProviderId::WasteManagement,
            customer_id: "900000001".to_owned(),
            provider_account: "900000001".to_owned(),
            currency: "USD".to_owned(),
            services: vec![ServiceBinding {
                id: "demo-waste".to_owned(),
                provider_service: "demo-bin".to_owned(),
                kind: ServiceKind::Waste,
            }],
        },
        from: NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(),
        to: NaiveDate::from_ymd_opt(2026, 5, 20).unwrap(),
        continuation: None,
        observed_at: DateTime::parse_from_rfc3339("2026-05-12T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc),
    }
}
fn reply(path: &str) -> (u16, String) {
    let body = if path.starts_with("/v2/customers/900000001/invoices") {
        r#"{"currency":"USD","invoices":[{"id":"DEMO-INVOICE","date":"2026-05-01","amount":"56.98","balance_amount":"null","payment_info":{"amount":"12.30","date":"2026-05-02","reference_number":"DEMO-PAYMENT"}}]}"#
    } else if path.starts_with("/v1/customers/900000001/balance") {
        r#"{"balance":[{"type":"total","amount":"44.68"}]}"#
    } else if path.starts_with("/v1/customers/900000001/services/stats") {
        r#"{"services":[{"id":"demo-bin","status":"Confirmed Negative","planned_date":"05/05/2026","service_time":"2026-05-05 07:30:00 PDT","exception_reason_code":"Container not out"}],"metadata":[{"totalCount":"1"}]}"#
    } else if path.starts_with("/v1/customers/900000001/services/plan") {
        r#"{"services":[{"id":"demo-bin","status":"Planned","planned_date":"05/12/2026"}]}"#
    } else if path.starts_with("/v1/customers/900000001/services/next-pickup") {
        r#"{"services":[{"name":"Fictional Waste","next_pickup":"Thursday, May 14 , 2026"}]}"#
    } else if path.starts_with("/v1/customers/900000001/services/demo-bin/ETA") {
        r#"{"ETA":{"from":"14: 30: 00.0","to":"16: 30: 00.0","reason_code":"ComputedETA","reason_text":"Service-ETAComputed"}}"#
    } else if path.starts_with("/v1/customers/900000001/services") {
        r#"{"services":[{"id":"demo-bin","enterprise_catalog":{"name":"Fictional Waste"}}]}"#
    } else {
        return (404, "{}".to_owned());
    };
    (200, body.to_owned())
}

#[tokio::test]
async fn all_history_omits_invoice_dates_but_keeps_collection_lookback_bounded() {
    let fixture = Fixture::new(|path| {
        if path.starts_with("/v2/customers/900000001/invoices") {
            assert_eq!(path, "/v2/customers/900000001/invoices");
            return (200, r#"{"currency":"USD","invoices":[{"id":"DEMO-OLD-INVOICE","date":"2023-05-01","amount":"10.25","balance_amount":"null"}]}"#.to_owned());
        }
        if path.starts_with("/v1/customers/900000001/services/stats") {
            assert!(path.contains("numberOfDays=366"));
        }
        reply(path)
    }).await;
    let requested = request();
    let page = fixture
        .adapter()
        .sync_history(HistoryRequest {
            binding: requested.binding,
            continuation: None,
            observed_at: requested.observed_at,
        })
        .await
        .unwrap();
    assert!(page.observations.iter().any(
        |row| matches!(row, Observation::Bill(bill) if bill.issued_on.to_string() == "2023-05-01")
    ));
    let bills = page
        .coverage
        .iter()
        .find(|row| row.capability == Capability::Bills)
        .unwrap();
    assert_eq!(bills.state, CoverageState::Complete);
    assert!(bills.from.is_none() && bills.to.is_none());
    let collections = page
        .coverage
        .iter()
        .find(|row| row.capability == Capability::Collections)
        .unwrap();
    assert_eq!(
        collections.reason.as_deref(),
        Some("bounded_history_and_next_pickup")
    );
}

#[tokio::test]
async fn published_http_shapes_map_money_payments_collections_and_eta_without_mutations() {
    let fixture = Fixture::new(reply).await;
    let page = fixture.adapter().sync(request()).await.unwrap();
    let bills = page
        .observations
        .iter()
        .filter_map(|o| {
            if let Observation::Bill(b) = o {
                Some(b)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(bills.len(), 1);
    assert_eq!(bills[0].total.minor_units, 5698);
    assert!(bills[0].outstanding_balance.is_none());
    assert!(bills[0].source_status.is_none());
    let payment = page
        .observations
        .iter()
        .find_map(|o| {
            if let Observation::Payment(p) = o {
                Some(p)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(payment.amount.minor_units, 1230);
    assert_eq!(payment.source_status, "reported_payment");
    let balance = page
        .observations
        .iter()
        .find_map(|o| {
            if let Observation::Balance(b) = o {
                Some(b)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(balance.amount.minor_units, 4468);
    let collections = page
        .observations
        .iter()
        .filter_map(|o| {
            if let Observation::Collection(c) = o {
                Some(c)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(collections.len(), 3);
    let today = collections
        .iter()
        .find(|c| c.scheduled_date.day() == 12)
        .unwrap();
    assert_eq!(
        today.eta,
        Some(
            DateTime::parse_from_rfc3339("2026-05-12T14:30:00Z")
                .unwrap()
                .with_timezone(&Utc)
        )
    );
    assert_eq!(today.status, CollectionStatus::Scheduled);
    let exception = collections
        .iter()
        .find(|c| c.scheduled_date.day() == 5)
        .unwrap();
    assert_eq!(exception.status, CollectionStatus::Exception);
    assert_eq!(exception.exception.as_deref(), Some("Container not out"));
    assert!(
        page.coverage
            .iter()
            .any(|c| c.capability == Capability::Collections && c.state == CoverageState::Partial)
    );
    assert!(
        page.coverage
            .iter()
            .any(|c| c.capability == Capability::Usage && c.state == CoverageState::Unsupported)
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 7);
    for text in requests.iter() {
        assert!(text.starts_with("GET "));
        assert!(
            text.to_ascii_lowercase()
                .contains("authorization: bearer fictional-wm-token")
        );
        assert!(
            text.to_ascii_lowercase()
                .contains("clientid: fictional-client-id")
        );
        assert!(text.to_ascii_lowercase().contains("request-tracking-id:"));
    }
}
use chrono::Datelike;

#[tokio::test]
async fn missing_or_mismatched_bindings_stop_before_financial_ingest() {
    let fixture = Fixture::new(reply).await;
    let mut input = request();
    input.binding.provider_account = "other".to_owned();
    assert!(matches!(
        fixture.adapter().sync(input).await,
        Err(UtilityError::BindingMismatch)
    ));
    assert!(fixture.requests.lock().unwrap().is_empty());
    let mut input = request();
    input.binding.services[0].provider_service = "foreign-bin".to_owned();
    assert!(matches!(
        fixture.adapter().sync(input).await,
        Err(UtilityError::BindingMismatch)
    ));
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn endpoint_failures_are_explicit_and_do_not_erase_other_coverage() {
    let fixture = Fixture::new(|path| {
        if path.contains("/invoices") {
            (403, "{\"private\":\"never-return-this\"}".to_owned())
        } else {
            reply(path)
        }
    })
    .await;
    let page = fixture.adapter().sync(request()).await.unwrap();
    assert!(
        !page
            .observations
            .iter()
            .any(|o| matches!(o, Observation::Bill(_)))
    );
    assert!(
        page.observations
            .iter()
            .any(|o| matches!(o, Observation::Balance(_)))
    );
    let coverage = page
        .coverage
        .iter()
        .find(|c| c.capability == Capability::Bills)
        .unwrap();
    assert_eq!(coverage.state, CoverageState::Unavailable);
    assert_eq!(coverage.reason.as_deref(), Some("access_denied"));
}

#[tokio::test]
async fn fractional_cents_wrong_currency_and_ambiguous_schedule_names_are_not_silently_mapped() {
    for mode in [0, 1, 2] {
        let fixture = Fixture::new(move |path| {
            let (status, body) = reply(path);
            (
                status,
                if mode == 0 && path.contains("/invoices") {
                    body.replace("56.98", "56.981")
                } else if mode == 1 && path.contains("/invoices") {
                    body.replace("USD", "CAD")
                } else if mode == 2 && path.contains("next-pickup") {
                    body.replace("Fictional Waste", "Unbound name")
                } else {
                    body
                },
            )
        })
        .await;
        let page = fixture.adapter().sync(request()).await.unwrap();
        let capability = if mode < 2 {
            Capability::Bills
        } else {
            Capability::Collections
        };
        let coverage = page
            .coverage
            .iter()
            .find(|c| c.capability == capability)
            .unwrap();
        assert_ne!(coverage.state, CoverageState::Complete);
    }
}

#[tokio::test]
async fn provider_auth_and_oversized_bodies_return_typed_errors_without_provider_text() {
    for status in [401, 403, 429, 500] {
        let fixture = Fixture::new(move |_| (status, "private provider body".to_owned())).await;
        let error = fixture.adapter().sync(request()).await.unwrap_err();
        assert!(!error.to_string().contains("private"));
        assert!(matches!(
            error,
            UtilityError::Authentication
                | UtilityError::AccessDenied
                | UtilityError::RateLimited
                | UtilityError::Unavailable
        ));
    }
    let fixture = Fixture::new(|_| (200, " ".repeat(8 * 1024 * 1024 + 1))).await;
    assert!(matches!(
        fixture.adapter().sync(request()).await,
        Err(UtilityError::InvalidResponse)
    ));
}

#[tokio::test]
async fn same_day_tickets_stay_distinct_and_plan_history_reconcile_by_ticket() {
    let fixture=Fixture::new(|path| {
        if path.contains("/stats") {(200,r#"{"services":[{"id":"demo-bin","ticket_number":"DEMO-A","status":"Confirmed Negative","planned_date":"05/12/2026","exception_reason_code":"Container blocked"},{"id":"demo-bin","ticket_number":"DEMO-B","status":"Confirmed Positive","planned_date":"05/12/2026"}],"metadata":[{"totalCount":"2"}]}"#.to_owned())}
        else if path.contains("/plan") {(200,r#"{"services":[{"id":"demo-bin","ticket_number":"DEMO-A","status":"Planned","planned_date":"05/12/2026"}]}"#.to_owned())}
        else {reply(path)}
    }).await;
    let page = fixture.adapter().sync(request()).await.unwrap();
    let events = page
        .observations
        .iter()
        .filter_map(|o| match o {
            Observation::Collection(c) if c.scheduled_date.day() == 12 => Some(c),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2);
    assert_ne!(events[0].id, events[1].id);
    assert!(
        events
            .iter()
            .any(|c| c.status == CollectionStatus::Exception
                && c.exception.as_deref() == Some("Container blocked"))
    );
    assert!(
        events
            .iter()
            .any(|c| c.status == CollectionStatus::Completed)
    );
}

#[tokio::test]
async fn eta_uses_authoritative_plan_date_after_utc_midnight() {
    let fixture = Fixture::new(reply).await;
    let mut input = request();
    input.observed_at = DateTime::parse_from_rfc3339("2026-05-13T01:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let page = fixture.adapter().sync(input).await.unwrap();
    let current = page
        .observations
        .iter()
        .find_map(|o| match o {
            Observation::Collection(c) if c.scheduled_date.day() == 12 => Some(c),
            _ => None,
        })
        .unwrap();
    assert!(current.eta.is_some());
    let requests = fixture.requests.lock().unwrap();
    let eta = requests.iter().find(|r| r.contains("/ETA?")).unwrap();
    assert!(eta.contains("requestDate=2026-05-12"));
}
