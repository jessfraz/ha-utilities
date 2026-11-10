//! Bayou v2 reads and explicit hosted connection setup. No data unlock or payment requests.
//!
//! The published bill and interval endpoints return unpaged arrays. Date windows
//! provide bounded continuation without inventing an undocumented offset API.
//! Sources: https://docs.bayou.energy/reference/get_customers-customer-id-bills
//! and https://docs.bayou.energy/reference/get_customers-customer-id-intervals.

use std::{str::FromStr, time::Duration};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Days, NaiveDate, Utc};
use reqwest::{Client, StatusCode};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    config::{AccountBinding, BayouConfig, ServiceBinding, read_secret, valid_id},
    models::{
        Bill, Capability, ConnectableUtility, ConnectionAccount, ConnectionAuthStatus,
        ConnectionMeter, Coverage, CoverageState, Money, Observation, ProviderId, Service,
        ServiceKind, Usage, UsageUnit,
    },
    utility::{Continuation, SyncPage, SyncRequest, Utility, UtilityError},
};

const BASE: &str = "https://bayou.energy/api/v2/";
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BILLS_PER_DAY: usize = 4096;
const MAX_INTERVALS_PER_WINDOW: usize = 10_000;
pub(crate) const CAPABILITIES: &[Capability] = &[
    Capability::Bills,
    Capability::Balances,
    Capability::Services,
    Capability::Usage,
];

/// A private HTTP client and private wire models stay inside this adapter.
pub struct Bayou {
    client: Client,
    base: Url,
    api_key: String,
}

impl Bayou {
    pub fn new(config: &BayouConfig) -> Result<Self, UtilityError> {
        let base = Url::parse(BASE).map_err(|_| UtilityError::InvalidResponse)?;
        Self::with_endpoint(config, base)
    }

    /// Alternate endpoints are limited to explicit loopback fixture servers.
    /// Redirects are disabled even on the production host, so keys never follow them.
    pub fn with_endpoint(config: &BayouConfig, base: Url) -> Result<Self, UtilityError> {
        let production = base.as_str() == BASE;
        let loopback = base.scheme() == "http"
            && matches!(base.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
            && base.path().ends_with('/');
        if !(production || loopback)
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(UtilityError::AccessDenied);
        }
        let api_key = read_secret(&config.api_key_file).map_err(|_| UtilityError::NotConfigured)?;
        if api_key.contains(':') {
            return Err(UtilityError::NotConfigured);
        }
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("ha-utilities/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| UtilityError::Unavailable)?;
        Ok(Self {
            client,
            base,
            api_key,
        })
    }

    fn endpoint(&self, customer: &str, resource: Option<&str>) -> Result<Url, UtilityError> {
        let mut url = self.base.clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| UtilityError::InvalidResponse)?;
        segments.pop_if_empty().push("customers").push(customer);
        if let Some(resource) = resource {
            segments.push(resource);
        }
        drop(segments);
        Ok(url)
    }

    async fn get<T: DeserializeOwned>(&self, url: Url) -> Result<T, UtilityError> {
        self.get_optional(url)
            .await?
            .ok_or(UtilityError::BindingMismatch)
    }

    async fn get_optional<T: DeserializeOwned>(&self, url: Url) -> Result<Option<T>, UtilityError> {
        // Bayou uses the key as Basic's username; the documented password is blank.
        let mut response = self
            .client
            .get(url)
            .basic_auth(&self.api_key, Some(""))
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|_| UtilityError::Unavailable)?;
        match response.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED => return Err(UtilityError::Authentication),
            StatusCode::FORBIDDEN => return Err(UtilityError::AccessDenied),
            StatusCode::NOT_FOUND => return Ok(None),
            StatusCode::TOO_MANY_REQUESTS => return Err(UtilityError::RateLimited),
            status if status.is_server_error() => return Err(UtilityError::Unavailable),
            _ => return Err(UtilityError::InvalidResponse),
        }
        if response
            .content_length()
            .is_some_and(|length| length > u64::try_from(MAX_RESPONSE_BYTES).unwrap_or(u64::MAX))
        {
            return Err(UtilityError::InvalidResponse);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| UtilityError::Unavailable)?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(UtilityError::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| UtilityError::InvalidResponse)
    }

    pub(crate) async fn connectable_utilities(
        &self,
    ) -> Result<Vec<ConnectableUtility>, UtilityError> {
        let url = self
            .base
            .join("utilities")
            .map_err(|_| UtilityError::InvalidResponse)?;
        let response: WireUtilities = self.get(url).await?;
        if response.utilities.len() > 1000 {
            return Err(UtilityError::InvalidResponse);
        }
        let mut ids = std::collections::HashSet::new();
        let mut utilities = Vec::with_capacity(response.utilities.len());
        for utility in response.utilities {
            if !valid_id(&utility.id)
                || !ids.insert(utility.id.clone())
                || !remote_text(&utility.name)
                || !remote_text(&utility.support)
            {
                return Err(UtilityError::InvalidResponse);
            }
            utilities.push(ConnectableUtility {
                id: utility.id,
                name: utility.name,
                support: utility.support,
            });
        }
        utilities.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        Ok(utilities)
    }

    /// External IDs are unique in Bayou. The owning service saves this intent
    /// before sending and uses GET by external ID to reconcile uncertain results.
    /// https://docs.bayou.energy/reference/post_customers
    pub(crate) async fn create_connection(
        &self,
        external_id: &str,
        utility: &str,
    ) -> Result<(), UtilityError> {
        let url = self
            .base
            .join("customers")
            .map_err(|_| UtilityError::InvalidResponse)?;
        let response = self
            .client
            .post(url)
            .basic_auth(&self.api_key, Some(""))
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&CreateCustomer {
                external_id,
                utility,
            })
            .send()
            .await
            .map_err(|_| UtilityError::Unavailable)?;
        match response.status() {
            StatusCode::CREATED | StatusCode::CONFLICT => Ok(()),
            StatusCode::UNAUTHORIZED => Err(UtilityError::Authentication),
            StatusCode::FORBIDDEN => Err(UtilityError::AccessDenied),
            StatusCode::TOO_MANY_REQUESTS => Err(UtilityError::RateLimited),
            status if status.is_server_error() => Err(UtilityError::Unavailable),
            _ => Err(UtilityError::InvalidResponse),
        }
    }

    /// The documented lookup also regenerates expired onboarding links. Tokens
    /// stay in this transient response and never enter the read model.
    /// https://docs.bayou.energy/reference/get_customers-id
    pub(crate) async fn connection(
        &self,
        external_id: &str,
        utility: &str,
        expected_customer: Option<&str>,
    ) -> Result<Option<BayouConnection>, UtilityError> {
        let lookup = format!("external_id:{external_id}");
        let Some(customer) = self
            .get_optional::<WireConnectionCustomer>(self.endpoint(&lookup, None)?)
            .await?
        else {
            return Ok(None);
        };
        if customer.id == 0
            || customer.external_id.as_deref() != Some(external_id)
            || customer.utility != utility
            || expected_customer.is_some_and(|id| id != customer.id.to_string())
            || customer.account_numbers.len() > 100
        {
            return Err(UtilityError::BindingMismatch);
        }
        let mut ids = std::collections::HashSet::new();
        let mut accounts = Vec::new();
        for account in customer.account_numbers {
            if !remote_text(&account.id)
                || !ids.insert(account.id.clone())
                || account.meters.len() > 64
            {
                return Err(UtilityError::InvalidResponse);
            }
            let mut meter_ids = std::collections::HashSet::new();
            let mut meters = Vec::new();
            for meter in account.meters {
                if !remote_text(&meter.id) || !meter_ids.insert(meter.id.clone()) {
                    return Err(UtilityError::InvalidResponse);
                }
                meters.push(ConnectionMeter {
                    id: meter.id,
                    kind: meter.kind.service_kind(),
                });
            }
            accounts.push(ConnectionAccount {
                id: account.id,
                meters,
            });
        }
        Ok(Some(BayouConnection {
            customer_id: customer.id.to_string(),
            auth_status: customer.auth_status,
            has_filled_credentials: customer.has_filled_credentials,
            authenticated: customer.is_currently_authenticated,
            bills_ready: customer.bills_are_ready,
            intervals_ready: customer.intervals_are_ready,
            accounts,
            authorization: customer.onboarding_link,
        }))
    }

    async fn customer(&self, binding: &AccountBinding) -> Result<Customer, UtilityError> {
        let customer: Customer = self.get(self.endpoint(&binding.customer_id, None)?).await?;
        if !customer.matches_id(&binding.customer_id) {
            return Err(UtilityError::BindingMismatch);
        }
        let account = customer.account(binding)?;
        for service in &binding.services {
            let mut meters = account
                .meters
                .iter()
                .filter(|meter| meter.id == service.provider_service);
            let meter = meters.next().ok_or(UtilityError::BindingMismatch)?;
            if meters.next().is_some() || meter.kind.service_kind() != service.kind {
                return Err(UtilityError::BindingMismatch);
            }
        }
        Ok(customer)
    }

    async fn bills(
        &self,
        request: &SyncRequest,
        customer: &Customer,
        day: NaiveDate,
    ) -> Result<(Vec<Observation>, bool), UtilityError> {
        let mut url = self.endpoint(&request.binding.customer_id, Some("bills"))?;
        url.query_pairs_mut()
            .append_pair("account_number", &request.binding.provider_account)
            .append_pair("start", &day.to_string())
            .append_pair("end", &day.to_string());
        // Omitting limit is deliberate: the documented API has no next/offset.
        let bills: Vec<WireBill> = self.get(url).await?;
        if bills.len() > MAX_BILLS_PER_DAY {
            return Err(UtilityError::InvalidResponse);
        }
        let mut observations = Vec::with_capacity(bills.len());
        let mut incomplete = false;
        let mut ids = std::collections::HashSet::new();
        let account = customer.account(&request.binding)?;
        for bill in bills {
            if bill.customer_id != customer.id
                || bill.account_number != request.binding.provider_account
                || !ids.insert(bill.id)
            {
                return Err(UtilityError::BindingMismatch);
            }
            // Even locked/unparsed bills must belong to this account; those states
            // have no usable amount, and reading must never unlock paid data.
            for meter in &bill.meters {
                let mut matched = account.meters.iter().filter(|source| source.id == meter.id);
                let source = matched.next().ok_or(UtilityError::BindingMismatch)?;
                if matched.next().is_some()
                    || meter.kind.service_kind() != source.kind.service_kind()
                {
                    return Err(UtilityError::BindingMismatch);
                }
            }
            let Some(issued_on) = bill.billed_on else {
                incomplete = true;
                continue;
            };
            if issued_on != day {
                return Err(UtilityError::InvalidResponse);
            }
            if bill
                .billing_period_from
                .zip(bill.billing_period_to)
                .is_some_and(|(from, to)| from > to)
            {
                return Err(UtilityError::InvalidResponse);
            }
            let Some(total) = bill.total_amount else {
                incomplete = true;
                continue;
            };
            if bill.status != BillStatus::Unlocked {
                incomplete = true;
            }
            observations.push(Observation::Bill(Bill {
                id: format!("bayou:{}:bill:{}", request.binding.id, bill.id),
                account_id: request.binding.id.clone(),
                provider: ProviderId::Bayou,
                issued_on,
                due_on: None,
                period_from: bill.billing_period_from,
                period_to: bill.billing_period_to,
                // Bayou returns cents but no currency. Account configuration is
                // explicit because supported utilities include Canadian bills.
                total: money(total, &request.binding.currency),
                // Officially the PREVIOUS cycle's balance, not current due and
                // absolutely not evidence that the present bill was settled.
                outstanding_balance: bill
                    .outstanding_balance
                    .map(|amount| money(amount, &request.binding.currency)),
                source_status: Some(bill.status.code().to_owned()),
                observed_at: request.observed_at,
            }));
        }
        Ok((observations, incomplete))
    }

    async fn intervals(
        &self,
        request: &SyncRequest,
        service: &ServiceBinding,
        day: NaiveDate,
        prior_end: Option<DateTime<Utc>>,
    ) -> Result<IntervalPage, UtilityError> {
        let start = midnight(day)?;
        let next = midnight(add_days(day, 1)?)?;
        // Bayou filters both period start AND end. Overlap accommodates daily
        // local-time intervals crossing UTC midnight (including 25-hour DST days).
        // Retaining only starts in this day avoids duplicate observations.
        let query_end = midnight(add_days(day, 3)?)?;
        let mut url = self.endpoint(&request.binding.customer_id, Some("intervals"))?;
        url.query_pairs_mut()
            .append_pair("account_number", &request.binding.provider_account)
            .append_pair("meter_id", &service.provider_service)
            .append_pair("start", &start.to_rfc3339())
            .append_pair("end", &query_end.to_rfc3339())
            .append_pair("latest_only", "true");
        let response: IntervalResponse = self.get(url).await?;
        let mut meters = response.meters();
        if meters.len() > 1 {
            return Err(UtilityError::BindingMismatch);
        }
        let mut rows = match meters.pop() {
            None => Vec::new(),
            Some(meter) => {
                if meter.id.text() != service.provider_service {
                    return Err(UtilityError::BindingMismatch);
                }
                meter.intervals
            }
        };
        if rows.len() > MAX_INTERVALS_PER_WINDOW {
            return Err(UtilityError::InvalidResponse);
        }
        rows.sort_by_key(|row| (row.start, row.end));
        let mut observations = Vec::new();
        let mut last_end = prior_end;
        let mut incomplete = false;
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            if row.start < start || row.end > query_end || row.end <= row.start {
                return Err(UtilityError::InvalidResponse);
            }
            if row.start >= next {
                continue;
            }
            // The contract advertises subdaily or daily data, not arbitrary
            // aggregate ranges. Reject overlapping/duplicate readings even when
            // a broken upstream ignores latest_only.
            if row.end - row.start > chrono::Duration::hours(26)
                || !seen.insert((row.start, row.end))
                || last_end.is_some_and(|end| row.start < end)
            {
                return Err(UtilityError::InvalidResponse);
            }
            if let Some(end) = last_end {
                incomplete |= row.start != end;
            } else {
                incomplete |= row.start != midnight(request.from)?;
            }
            last_end = Some(row.end);
            let (quantity, unit) = match service.kind {
                ServiceKind::Electricity => match row.electricity_consumption {
                    Some(value) if value >= 0 => (Decimal::from(value), UsageUnit::Wh),
                    Some(_) => return Err(UtilityError::InvalidResponse),
                    None => {
                        incomplete = true;
                        continue;
                    }
                },
                ServiceKind::Gas => match row.gas_consumption {
                    Some(value) => {
                        let decimal = value.decimal()?;
                        if decimal.is_sign_negative() {
                            return Err(UtilityError::InvalidResponse);
                        }
                        (decimal, UsageUnit::Therm)
                    }
                    None => {
                        incomplete = true;
                        continue;
                    }
                },
                _ => return Err(UtilityError::BindingMismatch),
            };
            observations.push(Observation::Usage(Usage {
                id: format!(
                    "bayou:{}:{}:{}:{}",
                    request.binding.id,
                    service.id,
                    row.start.timestamp(),
                    row.end.timestamp()
                ),
                account_id: request.binding.id.clone(),
                service_id: service.id.clone(),
                start: row.start,
                end: row.end,
                quantity,
                unit,
                observed_at: request.observed_at,
            }));
        }
        Ok(IntervalPage {
            observations,
            last_end,
            incomplete,
        })
    }
}

pub(crate) struct BayouConnection {
    pub customer_id: String,
    pub auth_status: Option<ConnectionAuthStatus>,
    pub has_filled_credentials: bool,
    pub authenticated: bool,
    pub bills_ready: bool,
    pub intervals_ready: bool,
    pub accounts: Vec<ConnectionAccount>,
    pub authorization: Option<String>,
}

#[derive(Serialize)]
struct CreateCustomer<'a> {
    external_id: &'a str,
    utility: &'a str,
}

#[derive(Deserialize)]
struct WireUtilities {
    utilities: Vec<WireUtility>,
}

#[derive(Deserialize)]
struct WireUtility {
    id: String,
    name: String,
    support: String,
}

#[derive(Deserialize)]
struct WireConnectionCustomer {
    id: u64,
    external_id: Option<String>,
    utility: String,
    #[serde(default)]
    has_filled_credentials: bool,
    is_currently_authenticated: bool,
    auth_status: Option<ConnectionAuthStatus>,
    #[serde(default)]
    bills_are_ready: bool,
    #[serde(default)]
    intervals_are_ready: bool,
    #[serde(default)]
    account_numbers: Vec<WireAccount>,
    onboarding_link: Option<String>,
}

fn remote_text(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 300 && !value.chars().any(char::is_control)
}

impl Utility for Bayou {
    fn capabilities(&self) -> &'static [Capability] {
        CAPABILITIES
    }

    async fn sync(&self, request: SyncRequest) -> Result<SyncPage, UtilityError> {
        validate_request(&request)?;
        let mut cursor = Cursor::for_request(&request)?;
        // Recheck the current account, service bindings and auth verdict on every
        // page, not just on a now-stale initial customer response.
        let customer = self.customer(&request.binding).await?;
        cursor.auth_incomplete |= !customer.is_currently_authenticated
            || customer.auth_status.as_deref() != Some("authenticated");
        cursor.bills_incomplete |= !customer.bills_are_ready;
        cursor.intervals_incomplete |= !customer.intervals_are_ready;
        let usage_available = customer.utility_has_intervals
            && customer.account_has_intervals
            && !request.binding.services.is_empty();
        let mut observations = Vec::new();
        let next = match cursor.stage {
            Stage::Bills { day } => {
                let (bills, incomplete) = self.bills(&request, &customer, day).await?;
                observations.extend(bills);
                cursor.bills_incomplete |= incomplete;
                if day < request.to {
                    Some(Stage::Bills {
                        day: add_days(day, 1)?,
                    })
                } else if usage_available {
                    Some(Stage::Usage {
                        day: request.from,
                        meter: 0,
                    })
                } else {
                    None
                }
            }
            Stage::Usage { day, meter } => {
                if !usage_available {
                    // A revoked/changed account cannot cause a formerly valid
                    // continuation to silently become a complete usage import.
                    cursor.intervals_incomplete = true;
                    None
                } else {
                    let service = request
                        .binding
                        .services
                        .get(meter)
                        .ok_or(UtilityError::InvalidContinuation)?;
                    let page = self
                        .intervals(&request, service, day, cursor.last_interval_end)
                        .await?;
                    observations.extend(page.observations);
                    cursor.intervals_incomplete |= page.incomplete;
                    cursor.last_interval_end = page.last_end;
                    if day < request.to {
                        Some(Stage::Usage {
                            day: add_days(day, 1)?,
                            meter,
                        })
                    } else {
                        cursor.intervals_incomplete |=
                            cursor.last_interval_end != Some(midnight(add_days(request.to, 1)?)?);
                        cursor.last_interval_end = None;
                        if meter + 1 < request.binding.services.len() {
                            Some(Stage::Usage {
                                day: request.from,
                                meter: meter + 1,
                            })
                        } else {
                            None
                        }
                    }
                }
            }
        };
        // Only configured, validated services enter the local model. The wire's
        // addresses, onboarding tokens, signed PDF URLs and credentials do not.
        observations.extend(request.binding.services.iter().map(|service| {
            Observation::Service(Service {
                id: service.id.clone(),
                name: service.name.clone(),
                kind: service.kind,
            })
        }));
        let coverage = coverage(&request, &customer, &cursor, next.as_ref())?;
        let continuation = if let Some(stage) = next {
            cursor.stage = stage;
            Some(Continuation(URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&cursor).map_err(|_| UtilityError::InvalidContinuation)?,
            )))
        } else {
            None
        };
        Ok(SyncPage {
            observations,
            coverage,
            continuation,
        })
    }
}

fn validate_request(request: &SyncRequest) -> Result<(), UtilityError> {
    let binding = &request.binding;
    if binding.provider != ProviderId::Bayou
        || !valid_id(&binding.id)
        || binding.deployment.trim().is_empty()
        || binding.customer_id.trim().is_empty()
        || binding.provider_account.trim().is_empty()
        || binding.currency.len() != 3
        || !binding
            .currency
            .bytes()
            .all(|byte| byte.is_ascii_uppercase())
        || binding.services.len() > 100
        || binding.services.iter().any(|service| {
            !valid_id(&service.id)
                || service.provider_service.is_empty()
                || !matches!(service.kind, ServiceKind::Electricity | ServiceKind::Gas)
        })
    {
        return Err(UtilityError::BindingMismatch);
    }
    if request.from > request.to || (request.to - request.from).num_days() > 365 {
        return Err(UtilityError::InvalidResponse);
    }
    let mut ids = std::collections::HashSet::new();
    let mut meters = std::collections::HashSet::new();
    if binding
        .services
        .iter()
        .any(|service| !ids.insert(&service.id) || !meters.insert(&service.provider_service))
    {
        return Err(UtilityError::BindingMismatch);
    }
    Ok(())
}

fn money(minor_units: i64, currency: &str) -> Money {
    Money {
        minor_units,
        currency: currency.to_owned(),
    }
}

fn midnight(day: NaiveDate) -> Result<DateTime<Utc>, UtilityError> {
    day.and_hms_opt(0, 0, 0)
        .map(|time| time.and_utc())
        .ok_or(UtilityError::InvalidResponse)
}

fn add_days(day: NaiveDate, days: u64) -> Result<NaiveDate, UtilityError> {
    day.checked_add_days(Days::new(days))
        .ok_or(UtilityError::InvalidResponse)
}

fn coverage(
    request: &SyncRequest,
    customer: &Customer,
    cursor: &Cursor,
    next: Option<&Stage>,
) -> Result<Vec<Coverage>, UtilityError> {
    let bills_pending = matches!(next, Some(Stage::Bills { .. }));
    let bills_reason = if bills_pending {
        Some("more_pages")
    } else if cursor.auth_incomplete {
        Some("provider_reauthentication_required")
    } else if cursor.bills_incomplete {
        Some("bills_not_ready_or_locked")
    } else {
        None
    };
    let (usage_state, usage_reason) = if !customer.utility_has_intervals {
        (CoverageState::Unsupported, Some("utility_has_no_intervals"))
    } else if !customer.account_has_intervals {
        (CoverageState::Unavailable, Some("account_has_no_intervals"))
    } else if request.binding.services.is_empty() {
        (CoverageState::Unavailable, Some("no_bound_services"))
    } else if next.is_some() {
        (CoverageState::Partial, Some("more_pages"))
    } else if cursor.auth_incomplete {
        (
            CoverageState::Partial,
            Some("provider_reauthentication_required"),
        )
    } else if cursor.intervals_incomplete {
        (
            CoverageState::Partial,
            Some("interval_gaps_or_data_not_ready"),
        )
    } else {
        (CoverageState::Complete, None)
    };
    let values = [
        (
            Capability::Bills,
            if bills_reason.is_some() {
                CoverageState::Partial
            } else {
                CoverageState::Complete
            },
            bills_reason,
        ),
        (
            Capability::Balances,
            CoverageState::Partial,
            Some("previous_cycle_balance_only"),
        ),
        (
            Capability::Payments,
            CoverageState::Unsupported,
            Some("no_documented_payment_history"),
        ),
        (Capability::Services, CoverageState::Complete, None),
        (
            Capability::Collections,
            CoverageState::Unsupported,
            Some("not_a_collection_provider"),
        ),
        (Capability::Usage, usage_state, usage_reason),
    ];
    let from = Some(midnight(request.from)?);
    let to = Some(midnight(add_days(request.to, 1)?)?);
    Ok(values
        .into_iter()
        .map(|(capability, state, reason)| Coverage {
            account_id: request.binding.id.clone(),
            capability,
            state,
            reason: reason.map(str::to_owned),
            checked_at: request.observed_at,
            from,
            to,
        })
        .collect())
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    version: u8,
    binding_digest: String,
    from: NaiveDate,
    to: NaiveDate,
    stage: Stage,
    bills_incomplete: bool,
    intervals_incomplete: bool,
    auth_incomplete: bool,
    last_interval_end: Option<DateTime<Utc>>,
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
enum Stage {
    Bills { day: NaiveDate },
    Usage { day: NaiveDate, meter: usize },
}

impl Cursor {
    fn for_request(request: &SyncRequest) -> Result<Self, UtilityError> {
        let binding_digest = URL_SAFE_NO_PAD.encode(Sha256::digest(
            serde_json::to_vec(&request.binding).map_err(|_| UtilityError::BindingMismatch)?,
        ));
        if let Some(token) = &request.continuation {
            if token.0.len() > 2048 {
                return Err(UtilityError::InvalidContinuation);
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(&token.0)
                .map_err(|_| UtilityError::InvalidContinuation)?;
            let cursor: Self =
                serde_json::from_slice(&bytes).map_err(|_| UtilityError::InvalidContinuation)?;
            let (day, meter) = match cursor.stage {
                Stage::Bills { day } => (day, None),
                Stage::Usage { day, meter } => (day, Some(meter)),
            };
            if cursor.version != 1
                || cursor.binding_digest != binding_digest
                || cursor.from != request.from
                || cursor.to != request.to
                || day < request.from
                || day > request.to
                || meter.is_some_and(|meter| meter >= request.binding.services.len())
            {
                return Err(UtilityError::InvalidContinuation);
            }
            return Ok(cursor);
        }
        Ok(Self {
            version: 1,
            binding_digest,
            from: request.from,
            to: request.to,
            stage: Stage::Bills { day: request.from },
            bills_incomplete: false,
            intervals_incomplete: false,
            auth_incomplete: false,
            last_interval_end: None,
        })
    }
}

struct IntervalPage {
    observations: Vec<Observation>,
    last_end: Option<DateTime<Utc>>,
    incomplete: bool,
}

// Unknown fields are intentionally ignored at the provider boundary. Only these
// typed fields are read; no address, PDF URL, onboarding token or raw body escapes.
#[derive(Deserialize)]
struct Customer {
    id: u64,
    external_id: Option<String>,
    is_currently_authenticated: bool,
    auth_status: Option<String>,
    bills_are_ready: bool,
    utility_has_intervals: bool,
    account_has_intervals: bool,
    intervals_are_ready: bool,
    account_numbers: Vec<WireAccount>,
}

impl Customer {
    fn matches_id(&self, requested: &str) -> bool {
        if let Some(external) = requested.strip_prefix("external_id:") {
            self.external_id.as_deref() == Some(external)
        } else {
            requested == self.id.to_string()
        }
    }

    fn account(&self, binding: &AccountBinding) -> Result<&WireAccount, UtilityError> {
        let mut matches = self
            .account_numbers
            .iter()
            .filter(|account| account.id == binding.provider_account);
        let account = matches.next().ok_or(UtilityError::BindingMismatch)?;
        if matches.next().is_some() {
            return Err(UtilityError::BindingMismatch);
        }
        Ok(account)
    }
}

#[derive(Deserialize)]
struct WireAccount {
    id: String,
    meters: Vec<WireMeter>,
}

#[derive(Deserialize)]
struct WireMeter {
    id: String,
    #[serde(rename = "type")]
    kind: MeterKind,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum MeterKind {
    Electric,
    Gas,
}

impl MeterKind {
    fn service_kind(&self) -> ServiceKind {
        match self {
            Self::Electric => ServiceKind::Electricity,
            Self::Gas => ServiceKind::Gas,
        }
    }
}

#[derive(Deserialize)]
struct WireBill {
    id: u64,
    customer_id: u64,
    account_number: String,
    status: BillStatus,
    billed_on: Option<NaiveDate>,
    billing_period_from: Option<NaiveDate>,
    billing_period_to: Option<NaiveDate>,
    outstanding_balance: Option<i64>,
    total_amount: Option<i64>,
    #[serde(default)]
    meters: Vec<WireMeter>,
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum BillStatus {
    NotSupported,
    Unparsed,
    Locked,
    PartiallyUnlocked,
    UnlockedForGas,
    UnlockedForElectric,
    Unlocked,
}

impl BillStatus {
    fn code(&self) -> &'static str {
        match self {
            Self::NotSupported => "not_supported",
            Self::Unparsed => "unparsed",
            Self::Locked => "locked",
            Self::PartiallyUnlocked => "partially_unlocked",
            Self::UnlockedForGas => "unlocked_for_gas",
            Self::UnlockedForElectric => "unlocked_for_electric",
            Self::Unlocked => "unlocked",
        }
    }
}

// The official OpenAPI response is an array, while Bayou's "Interval data"
// guide shows one object and a numeric meter ID. Accept exactly those documented
// wire shapes; both still require one explicitly bound meter, never a first-item
// fallback when the requested meter filter was ignored.
#[derive(Deserialize)]
#[serde(untagged)]
enum IntervalResponse {
    Single(Intervals),
    Multiple(Vec<Intervals>),
}

impl IntervalResponse {
    fn meters(self) -> Vec<IntervalMeter> {
        match self {
            Self::Single(response) => response.meters,
            Self::Multiple(responses) => responses
                .into_iter()
                .flat_map(|response| response.meters)
                .collect(),
        }
    }
}

#[derive(Deserialize)]
struct Intervals {
    meters: Vec<IntervalMeter>,
}

#[derive(Deserialize)]
struct IntervalMeter {
    id: WireIdentifier,
    intervals: Vec<WireInterval>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireIdentifier {
    Text(String),
    Number(u64),
}

impl WireIdentifier {
    fn text(self) -> String {
        match self {
            Self::Text(value) => value,
            Self::Number(value) => value.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct WireInterval {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    electricity_consumption: Option<i64>,
    gas_consumption: Option<Numeric>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Numeric {
    Text(String),
    Number(serde_json::Number),
}

impl Numeric {
    fn decimal(self) -> Result<Decimal, UtilityError> {
        let text = match self {
            Self::Text(text) => text,
            Self::Number(number) => number.to_string(),
        };
        Decimal::from_str(&text)
            .or_else(|_| Decimal::from_scientific(&text))
            .map_err(|_| UtilityError::InvalidResponse)
    }
}
