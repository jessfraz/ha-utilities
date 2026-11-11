//! WM's published reporting API. No consumer-session scraping or service mutations.
//! Official schemas: https://api.wm.com/{InvoiceS,balance,Services,servicestat,serviceplan,servicenextpickup,ETA}/index.html
use crate::{
    config::{WmConfig, read_secret},
    models::*,
    utility::{HistoryRequest, SyncPage, SyncRequest, Utility, UtilityError},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Days, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use reqwest::{Client, StatusCode};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, de::DeserializeOwned};
use std::{collections::HashMap, str::FromStr, time::Duration};
use url::Url;

pub(crate) const CAPABILITIES: &[Capability] = &[
    Capability::Bills,
    Capability::Balances,
    Capability::Payments,
    Capability::Services,
    Capability::Collections,
];
const MAX_BODY: usize = 8 * 1024 * 1024;

pub struct WasteManagement {
    client: Client,
    base: Url,
    token: String,
    client_id: String,
}

impl WasteManagement {
    pub fn new(config: &WmConfig) -> anyhow::Result<Self> {
        Self::with_endpoint(
            Url::parse("https://api.wm.com/")?,
            read_secret(&config.token_file)?,
            read_secret(&config.client_id_file)?,
        )
    }

    /// Alternative origins are limited to loopback HTTP fixture servers.
    pub fn with_endpoint(base: Url, token: String, client_id: String) -> anyhow::Result<Self> {
        let production = base.as_str() == "https://api.wm.com/";
        let loopback = base.scheme() == "http"
            && base
                .host_str()
                .is_some_and(|h| h == "127.0.0.1" || h == "[::1]" || h == "localhost");
        anyhow::ensure!(
            (production || loopback)
                && base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none()
                && base.path() == "/",
            "invalid WM API origin"
        );
        anyhow::ensure!(
            !token.trim().is_empty() && !client_id.trim().is_empty(),
            "WM credentials are empty"
        );
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            base,
            token,
            client_id,
        })
    }

    async fn get<T: DeserializeOwned>(
        &self,
        version: &str,
        customer: &str,
        path: &[&str],
        query: &[(&str, String)],
    ) -> Result<T, UtilityError> {
        let mut url = self.base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| UtilityError::InvalidResponse)?;
            segments
                .pop_if_empty()
                .push(version)
                .push("customers")
                .push(customer);
            for segment in path {
                segments.push(segment);
            }
        }
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
        }
        let mut response = self
            .client
            .get(url)
            .bearer_auth(&self.token)
            .header("ClientId", &self.client_id)
            .header("Request-Tracking-Id", uuid::Uuid::now_v7().to_string())
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|_| UtilityError::Unavailable)?;
        match response.status() {
            StatusCode::UNAUTHORIZED => return Err(UtilityError::Authentication),
            StatusCode::FORBIDDEN => return Err(UtilityError::AccessDenied),
            StatusCode::TOO_MANY_REQUESTS => return Err(UtilityError::RateLimited),
            status if !status.is_success() => return Err(UtilityError::Unavailable),
            _ => (),
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_BODY as u64)
        {
            return Err(UtilityError::InvalidResponse);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| UtilityError::Unavailable)?
        {
            if body.len() + chunk.len() > MAX_BODY {
                return Err(UtilityError::InvalidResponse);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| UtilityError::InvalidResponse)
    }

    async fn collect(
        &self,
        request: SyncRequest,
        all_invoices: bool,
    ) -> Result<SyncPage, UtilityError> {
        if request.continuation.is_some() {
            return Err(UtilityError::InvalidContinuation);
        }
        let binding = &request.binding;
        if binding.provider != ProviderId::WasteManagement
            || binding.provider_account != binding.customer_id
            || binding.customer_id.is_empty()
            || binding.services.len() > 64
            || request.from > request.to
            || (!all_invoices && request.to.signed_duration_since(request.from).num_days() > 366)
        {
            return Err(UtilityError::BindingMismatch);
        }
        // The customer path is the documented account scope. Validate every configured
        // service against that customer's catalog before ingesting any financial data.
        let catalog: ServicesResponse = self
            .get("v1", &binding.customer_id, &["services"], &[])
            .await?;
        let mut by_id = HashMap::new();
        let mut names: HashMap<String, Vec<String>> = HashMap::new();
        for service in catalog.services {
            let id = service.id.text();
            if by_id.insert(id.clone(), service.clone()).is_some() {
                return Err(UtilityError::InvalidResponse);
            }
            if let Some(catalog) = service.enterprise_catalog {
                names.entry(catalog.name).or_default().push(id);
            }
        }
        for service in &binding.services {
            if !by_id.contains_key(&service.provider_service) {
                return Err(UtilityError::BindingMismatch);
            }
        }
        let mut observations: Vec<Observation> = binding
            .services
            .iter()
            .map(|s| {
                Observation::Service(Service {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    kind: s.kind,
                })
            })
            .collect();
        let mut coverages = vec![
            coverage(
                &request,
                Capability::Services,
                CoverageState::Complete,
                None,
            ),
            coverage(
                &request,
                Capability::Usage,
                CoverageState::Unsupported,
                Some("provider_not_supported"),
            ),
        ];
        let invoice_range = [
            ("fromDate", request.from.to_string()),
            ("toDate", request.to.to_string()),
        ];
        // WM explicitly documents omission of both filters as all invoices.
        let invoice_filters = if all_invoices {
            &[][..]
        } else {
            &invoice_range[..]
        };
        let invoices: Result<InvoicesResponse, _> = self
            .get("v2", &binding.customer_id, &["invoices"], invoice_filters)
            .await;

        match invoices.and_then(|response| map_invoices(&request, response, all_invoices)) {
            Ok(mut items) => {
                observations.append(&mut items);
                coverages.push(coverage(
                    &request,
                    Capability::Bills,
                    CoverageState::Complete,
                    None,
                ));
                coverages.push(coverage(
                    &request,
                    Capability::Payments,
                    CoverageState::Partial,
                    Some("invoice_associated_payments_only"),
                ));
            }
            Err(error) => {
                for capability in [Capability::Bills, Capability::Payments] {
                    coverages.push(failed_coverage(&request, capability, &error));
                }
            }
        }
        let balance: Result<BalanceResponse, _> = self
            .get("v1", &binding.customer_id, &["balance"], &[])
            .await;
        match balance.and_then(|response| map_balance(&request, response)) {
            Ok(item) => {
                observations.push(Observation::Balance(item));
                coverages.push(coverage(
                    &request,
                    Capability::Balances,
                    CoverageState::Complete,
                    None,
                ));
            }
            Err(error) => coverages.push(failed_coverage(&request, Capability::Balances, &error)),
        }
        let today = request.observed_at.date_naive();
        let days = today
            .signed_duration_since(request.from)
            .num_days()
            .clamp(1, 366);
        let history: Result<CollectionsResponse, _> = self
            .get(
                "v1",
                &binding.customer_id,
                &["services", "stats"],
                &[
                    ("numberOfDays", days.to_string()),
                    ("summary", "N".to_owned()),
                ],
            )
            .await;
        let plan: Result<CollectionsResponse, _> = self
            .get("v1", &binding.customer_id, &["services", "plan"], &[])
            .await;
        let next: Result<NextResponse, _> = self
            .get(
                "v1",
                &binding.customer_id,
                &["services", "next-pickup"],
                &[],
            )
            .await;
        let mut collections: HashMap<String, CollectionEvent> = HashMap::new();
        let mut collection_error = None;
        for (result, planned) in [(plan, true), (history, false)] {
            match result.and_then(|response| map_collections(&request, response)) {
                Ok(items) => {
                    for mut item in items {
                        item.needs_eta = planned;
                        if let Some(prior) = collections.get(&item.collection.id) {
                            item.needs_eta |= prior.needs_eta;
                        }
                        collections.insert(item.collection.id.clone(), item);
                    }
                }
                Err(error) => collection_error = Some(error),
            }
        }
        match next.and_then(|response| map_next(&request, response, &names)) {
            Ok(items) => {
                for item in items {
                    // A next-pickup row has no ticket ID. Do not duplicate an
                    // identified plan/history event for the same service/day.
                    if !collections.values().any(|event| {
                        event.collection.service_id == item.service_id
                            && event.collection.scheduled_date == item.scheduled_date
                    }) {
                        collections.insert(
                            item.id.clone(),
                            CollectionEvent {
                                collection: item,
                                ticket: None,
                                needs_eta: false,
                            },
                        );
                    }
                }
            }
            Err(error) => collection_error = Some(error),
        }
        // The plan's calendar date is authoritative, including after UTC
        // midnight. Distinct same-day tickets retain their own ETA request.
        for event in collections.values_mut().filter(|event| event.needs_eta) {
            let item = &mut event.collection;
            let Some(service) = binding
                .services
                .iter()
                .find(|service| service.id == item.service_id)
            else {
                return Err(UtilityError::BindingMismatch);
            };
            let mut query = vec![("requestDate", item.scheduled_date.to_string())];
            if let Some(ticket) = &event.ticket {
                query.push(("ticketNumber", ticket.clone()));
            }
            let response: Result<EtaResponse, _> = self
                .get(
                    "v1",
                    &binding.customer_id,
                    &["services", &service.provider_service, "ETA"],
                    &query,
                )
                .await;
            match response {
                Ok(response) => {
                    if let Some(eta) = response.eta {
                        if eta.reason_code.as_deref() == Some("ComputedETA") {
                            item.eta = eta
                                .from
                                .as_deref()
                                .and_then(|time| utc_time(item.scheduled_date, time));
                        }
                        if let Some(completed) =
                            eta.serviced_date_time.as_deref().and_then(parse_timestamp)
                        {
                            item.completed_at = Some(completed);
                            item.status = CollectionStatus::Completed;
                        }
                        if eta
                            .reason_code
                            .as_deref()
                            .is_some_and(|code| code != "ComputedETA")
                        {
                            item.exception = eta.reason_text.filter(|text| !text.trim().is_empty());
                        }
                    }
                }
                Err(error) => collection_error = Some(error),
            }
        }
        observations.extend(
            collections
                .into_values()
                .map(|event| Observation::Collection(event.collection)),
        );
        coverages.push(match collection_error {
            Some(error) => {
                let mut item = failed_coverage(&request, Capability::Collections, &error);
                item.state = CoverageState::Partial;
                item
            }
            None => coverage(
                &request,
                Capability::Collections,
                CoverageState::Partial,
                Some("bounded_history_and_next_pickup"),
            ),
        });
        if all_invoices {
            for item in coverages
                .iter_mut()
                .filter(|item| matches!(item.capability, Capability::Bills | Capability::Payments))
            {
                item.from = None;
                item.to = None;
            }
        }
        Ok(SyncPage {
            observations,
            coverage: coverages,
            continuation: None,
        })
    }
}

impl Utility for WasteManagement {
    fn capabilities(&self) -> &'static [Capability] {
        CAPABILITIES
    }
    async fn sync(&self, request: SyncRequest) -> Result<SyncPage, UtilityError> {
        self.collect(request, false).await
    }

    async fn sync_history(&self, request: HistoryRequest) -> Result<SyncPage, UtilityError> {
        let from = request
            .observed_at
            .date_naive()
            .checked_sub_days(Days::new(366))
            .ok_or(UtilityError::InvalidResponse)?;
        // Collections only expose a lookback count and current/next service.
        // Keep the existing bounded coverage there while reading all invoices.
        self.collect(
            SyncRequest {
                binding: request.binding,
                from,
                to: NaiveDate::MAX,
                continuation: request.continuation,
                observed_at: request.observed_at,
            },
            true,
        )
        .await
    }
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum WireId {
    Text(String),
    Number(u64),
}
impl WireId {
    fn text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Number(n) => n.to_string(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum WireAmount {
    Text(String),
    Number(Decimal),
}
impl WireAmount {
    fn minor(&self) -> Result<i64, UtilityError> {
        let amount = match self {
            Self::Text(s) => Decimal::from_str(s).map_err(|_| UtilityError::InvalidResponse)?,
            Self::Number(n) => *n,
        };
        let cents = amount
            .checked_mul(Decimal::from(100))
            .ok_or(UtilityError::InvalidResponse)?;
        if !cents.fract().is_zero() {
            return Err(UtilityError::InvalidResponse);
        }
        cents.to_i64().ok_or(UtilityError::InvalidResponse)
    }
    fn optional_minor(&self) -> Result<Option<i64>, UtilityError> {
        if matches!(self,Self::Text(s) if s=="null" || s.trim().is_empty()) {
            Ok(None)
        } else {
            self.minor().map(Some)
        }
    }
}
#[derive(Deserialize)]
struct InvoicesResponse {
    currency: String,
    invoices: Vec<Invoice>,
}
#[derive(Deserialize)]
struct Invoice {
    id: WireId,
    date: String,
    amount: WireAmount,
    payment_info: Option<PaymentInfo>,
    balance_amount: Option<WireAmount>,
}
#[derive(Deserialize)]
struct PaymentInfo {
    amount: WireAmount,
    date: String,
    reference_number: Option<String>,
}
#[derive(Deserialize)]
struct BalanceResponse {
    balance: Vec<BalanceItem>,
}
#[derive(Deserialize)]
struct BalanceItem {
    #[serde(rename = "type")]
    kind: String,
    amount: WireAmount,
}
#[derive(Deserialize)]
struct ServicesResponse {
    services: Vec<WireService>,
}
#[derive(Clone, Deserialize)]
struct WireService {
    id: WireId,
    enterprise_catalog: Option<Catalog>,
}
#[derive(Clone, Deserialize)]
struct Catalog {
    name: String,
}
#[derive(Deserialize)]
struct CollectionsResponse {
    services: Vec<WireCollection>,
    metadata: Option<Vec<Metadata>>,
}
#[derive(Deserialize)]
struct Metadata {
    #[serde(rename = "totalCount")]
    total_count: WireId,
}
#[derive(Deserialize)]
struct WireCollection {
    id: WireId,
    ticket_number: Option<WireId>,
    status: String,
    planned_date: String,
    service_time: Option<String>,
    exception_reason_code: Option<String>,
}
#[derive(Deserialize)]
struct NextResponse {
    services: Vec<NextPickup>,
}
#[derive(Deserialize)]
struct NextPickup {
    name: String,
    next_pickup: String,
}
#[derive(Deserialize)]
struct EtaResponse {
    #[serde(rename = "ETA")]
    eta: Option<Eta>,
}
#[derive(Deserialize)]
struct Eta {
    from: Option<String>,
    reason_code: Option<String>,
    reason_text: Option<String>,
    serviced_date_time: Option<String>,
}

fn date(value: &str) -> Result<NaiveDate, UtilityError> {
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" ,", ",");
    ["%Y-%m-%d", "%m/%d/%Y", "%m-%d-%y", "%A, %b %e, %Y"]
        .iter()
        .find_map(|format| NaiveDate::parse_from_str(&normalized, format).ok())
        .ok_or(UtilityError::InvalidResponse)
}
fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(time) = DateTime::parse_from_rfc3339(value) {
        return Some(time.with_timezone(&Utc));
    }
    let (naive, zone) = value.rsplit_once(' ')?;
    let offset = match zone {
        "UTC" => 0,
        "EST" => -5 * 3600,
        "EDT" => -4 * 3600,
        "CST" => -6 * 3600,
        "CDT" => -5 * 3600,
        "MST" => -7 * 3600,
        "MDT" => -6 * 3600,
        "PST" => -8 * 3600,
        "PDT" => -7 * 3600,
        _ => return None,
    };
    let time = NaiveDateTime::parse_from_str(naive, "%Y-%m-%d %H:%M:%S").ok()?;
    time.checked_sub_signed(chrono::Duration::seconds(offset))
        .map(|time| time.and_utc())
}
fn utc_time(day: NaiveDate, value: &str) -> Option<DateTime<Utc>> {
    let compact = value.replace(' ', "");
    NaiveTime::parse_from_str(&compact, "%H:%M:%S%.f")
        .ok()
        .map(|time| day.and_time(time).and_utc())
}
fn money(amount: i64, currency: &str) -> Money {
    Money {
        minor_units: amount,
        currency: currency.to_owned(),
    }
}
fn map_invoices(
    request: &SyncRequest,
    response: InvoicesResponse,
    all_invoices: bool,
) -> Result<Vec<Observation>, UtilityError> {
    if response.currency != request.binding.currency
        || response.currency.len() != 3
        || !response.currency.bytes().all(|c| c.is_ascii_uppercase())
        || response.invoices.len() > 20_000
    {
        return Err(UtilityError::InvalidResponse);
    }
    let mut items = Vec::new();
    for invoice in response.invoices {
        let issued = date(&invoice.date)?;
        if !all_invoices && (issued < request.from || issued > request.to) {
            return Err(UtilityError::InvalidResponse);
        }
        if !crate::config::valid_id(&invoice.id.text()) {
            return Err(UtilityError::InvalidResponse);
        }
        let id = format!("{}:bill:{}", request.binding.id, invoice.id.text());
        let balance = invoice
            .balance_amount
            .map(|a| a.optional_minor())
            .transpose()?
            .flatten()
            .map(|a| money(a, &response.currency));
        items.push(Observation::Bill(Bill {
            id: id.clone(),
            account_id: request.binding.id.clone(),
            provider: ProviderId::WasteManagement,
            issued_on: issued,
            due_on: None,
            period_from: None,
            period_to: None,
            total: money(invoice.amount.minor()?, &response.currency),
            new_charges: None,
            reported_quantities: vec![],
            outstanding_balance: balance,
            source_status: None,
            observed_at: request.observed_at,
        }));
        if let Some(payment) = invoice.payment_info
            && let Some(amount) = payment.amount.optional_minor()?
        {
            // WM reports a payment attached to this invoice, not its settlement status.
            let paid_on = date(&payment.date)?;
            let reference = payment.reference_number.unwrap_or_default();
            items.push(Observation::Payment(Payment {
                id: payment_id(&request.binding.id, &id, paid_on, amount, &reference),
                account_id: request.binding.id.clone(),
                bill_id: Some(id),
                amount: money(amount, &response.currency),
                paid_on,
                source_status: "reported_payment".to_owned(),
                observed_at: request.observed_at,
            }));
        }
    }
    Ok(items)
}

pub(crate) fn payment_id(
    account: &str,
    bill: &str,
    paid_on: NaiveDate,
    amount: i64,
    reference: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let key = URL_SAFE_NO_PAD.encode(Sha256::digest(format!(
        "{bill}|{paid_on}|{amount}|{reference}"
    )));
    format!("{account}:payment:{key}")
}
fn map_balance(request: &SyncRequest, response: BalanceResponse) -> Result<Balance, UtilityError> {
    let totals = response
        .balance
        .into_iter()
        .filter(|b| b.kind.eq_ignore_ascii_case("total"))
        .collect::<Vec<_>>();
    if totals.len() != 1 {
        return Err(UtilityError::InvalidResponse);
    }
    // The balance endpoint omits currency. Require the explicit account binding's
    // currency rather than assuming USD for a potentially CAD account.
    let currency = &request.binding.currency;
    Ok(Balance {
        id: format!("{}:balance:current", request.binding.id),
        account_id: request.binding.id.clone(),
        amount: money(totals[0].amount.minor()?, currency),
        as_of: request.observed_at,
        observed_at: request.observed_at,
    })
}
fn collection_id(account: &str, service: &str, day: NaiveDate, ticket: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let base = format!("{account}:collection:{service}:{day}");
    match ticket {
        Some(ticket) => format!(
            "{base}:ticket:{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(ticket))
        ),
        None => base,
    }
}
struct CollectionEvent {
    collection: Collection,
    ticket: Option<String>,
    needs_eta: bool,
}
fn map_collections(
    request: &SyncRequest,
    response: CollectionsResponse,
) -> Result<Vec<CollectionEvent>, UtilityError> {
    if response.services.len() > 20_000
        || response.metadata.as_ref().is_some_and(|items| {
            items.iter().any(|m| {
                m.total_count.text().parse::<usize>().ok() != Some(response.services.len())
            })
        })
    {
        return Err(UtilityError::InvalidResponse);
    }
    let mut items = Vec::new();
    for collection in response.services {
        let external = collection.id.text();
        let Some(service) = request
            .binding
            .services
            .iter()
            .find(|s| s.provider_service == external)
        else {
            continue;
        };
        let day = date(&collection.planned_date)?;
        if day < request.from || day > request.to {
            continue;
        }
        let status = match collection.status.as_str() {
            "Confirmed Positive" => CollectionStatus::Completed,
            "Confirmed Negative" | "Confirmed Incomplete" => CollectionStatus::Exception,
            "Planned" => CollectionStatus::Scheduled,
            _ => CollectionStatus::Unknown,
        };
        let ticket = collection.ticket_number.as_ref().map(WireId::text);
        if ticket
            .as_deref()
            .is_some_and(|value| !crate::config::valid_id(value))
        {
            return Err(UtilityError::InvalidResponse);
        }
        items.push(CollectionEvent {
            collection: Collection {
                id: collection_id(&request.binding.id, &service.id, day, ticket.as_deref()),
                account_id: request.binding.id.clone(),
                service_id: service.id.clone(),
                scheduled_date: day,
                status,
                completed_at: collection.service_time.as_deref().and_then(parse_timestamp),
                eta: None,
                exception: collection.exception_reason_code,
                observed_at: request.observed_at,
            },
            ticket,
            needs_eta: false,
        });
    }
    Ok(items)
}
fn map_next(
    request: &SyncRequest,
    response: NextResponse,
    names: &HashMap<String, Vec<String>>,
) -> Result<Vec<Collection>, UtilityError> {
    let mut items = Vec::new();
    for pickup in response.services {
        let ids = names
            .get(&pickup.name)
            .ok_or(UtilityError::BindingMismatch)?;
        if ids.len() != 1 {
            return Err(UtilityError::BindingMismatch);
        }
        let Some(service) = request
            .binding
            .services
            .iter()
            .find(|s| s.provider_service == ids[0])
        else {
            continue;
        };
        let day = date(&pickup.next_pickup)?;
        if day < request.from || day > request.to {
            continue;
        }
        items.push(Collection {
            id: collection_id(&request.binding.id, &service.id, day, None),
            account_id: request.binding.id.clone(),
            service_id: service.id.clone(),
            scheduled_date: day,
            status: CollectionStatus::Scheduled,
            completed_at: None,
            eta: None,
            exception: None,
            observed_at: request.observed_at,
        });
    }
    Ok(items)
}
fn coverage(
    request: &SyncRequest,
    capability: Capability,
    state: CoverageState,
    reason: Option<&str>,
) -> Coverage {
    Coverage {
        account_id: request.binding.id.clone(),
        capability,
        state,
        reason: reason.map(str::to_owned),
        checked_at: request.observed_at,
        from: request.from.and_hms_opt(0, 0, 0).map(|d| d.and_utc()),
        to: request
            .to
            .succ_opt()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
            .map(|d| d.and_utc()),
    }
}
fn failed_coverage(
    request: &SyncRequest,
    capability: Capability,
    error: &UtilityError,
) -> Coverage {
    coverage(
        request,
        capability,
        CoverageState::Unavailable,
        Some(match error {
            UtilityError::Authentication => "authentication_required",
            UtilityError::AccessDenied => "access_denied",
            UtilityError::RateLimited => "rate_limited",
            UtilityError::BindingMismatch => "binding_mismatch",
            UtilityError::InvalidResponse => "invalid_response",
            _ => "provider_unavailable",
        }),
    )
}

#[cfg(test)]
mod tests {
    use crate::utility::waste_management::parse_timestamp;
    use chrono::NaiveDate;

    #[test]
    fn out_of_range_provider_timestamp_does_not_panic() {
        let time = NaiveDate::MAX.and_hms_opt(23, 59, 59).unwrap();
        let value = format!("{} PST", time.format("%Y-%m-%d %H:%M:%S"));
        assert!(parse_timestamp(&value).is_none());
    }
}
