//! Normalized local read model. Provider account identifiers never identify a local account.
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    WasteManagement,
    Bayou,
    Lvmwd,
    SuperiorPropane,
    Starlink,
    Spectrum,
}

impl ProviderId {
    pub const fn name(self) -> &'static str {
        match self {
            Self::WasteManagement => "Waste Management",
            Self::Bayou => "Bayou",
            Self::Lvmwd => "LVMWD",
            Self::SuperiorPropane => "Superior Propane",
            Self::Starlink => "Starlink",
            Self::Spectrum => "Spectrum",
        }
    }
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Bills,
    Balances,
    Payments,
    Services,
    Collections,
    Usage,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProviderInfo {
    pub id: ProviderId,
    pub name: String,
    pub capabilities: Vec<Capability>,
    pub configured: bool,
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    Waste,
    Recycling,
    Electricity,
    Gas,
    Water,
    Propane,
    Internet,
    Other,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub provider: ProviderId,
    pub services: Vec<Service>,
    #[serde(default)]
    pub import_only: bool,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConnectableUtility {
    pub id: String,
    pub name: String,
    pub support: String,
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionProvider {
    Bayou,
}

#[derive(JsonSchema, Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    #[default]
    Queued,
    Creating,
    Unconfirmed,
    AwaitingAuthorization,
    Authenticating,
    Discovering,
    Ready,
    Connected,
    NeedsAuthentication,
    Unavailable,
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionAuthStatus {
    Authenticated,
    OnboardingCredentialsNeeded,
    OnboardingMfaNeeded,
    OnboardingReauthNeeded,
    OnboardingCustomerActionRequired,
    OnboardingAuthIssue,
    RefreshMfaNeeded,
    RefreshReauthNeeded,
    RefreshCustomerActionRequired,
    RefreshAuthIssue,
    #[serde(other)]
    Unknown,
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionIssue {
    AuthenticationRequired,
    AccessDenied,
    RateLimited,
    ProviderUnavailable,
    InvalidResponse,
    CustomerNotFound,
    ProcessInterrupted,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConnectionMeter {
    pub id: String,
    pub kind: ServiceKind,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConnectionAccount {
    pub id: String,
    pub meters: Vec<ConnectionMeter>,
}

#[derive(JsonSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConnectionProgress {
    pub state: ConnectionState,
    pub customer_id: Option<String>,
    pub auth_status: Option<ConnectionAuthStatus>,
    pub has_filled_credentials: bool,
    pub bills_ready: bool,
    pub intervals_ready: bool,
    pub accounts: Vec<ConnectionAccount>,
    pub linked_account_ids: Vec<String>,
    pub issue: Option<ConnectionIssue>,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Connection {
    pub id: String,
    pub provider: ConnectionProvider,
    pub request_id: String,
    pub utility_id: String,
    pub utility_name: String,
    pub name: String,
    pub currency: String,
    #[serde(flatten)]
    pub progress: ConnectionProgress,
    pub provider_settings_accepted_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub checked_at: Option<DateTime<Utc>>,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateConnection {
    pub provider: ConnectionProvider,
    pub request_id: String,
    pub utility_id: String,
    pub name: Option<String>,
    pub currency: String,
    pub accept_provider_settings: bool,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionRename {
    pub name: String,
    pub expected_name: String,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionSelection {
    pub account: String,
    pub meters: Vec<String>,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BindConnection {
    pub accounts: Vec<ConnectionSelection>,
}

#[derive(JsonSchema, Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionAction {}

// The hosted link is a temporary authorization credential. It is returned only
// on request, never included in diagnostics, database state, or ordinary status.
#[derive(JsonSchema, Deserialize, Serialize)]
pub struct ConnectionAuthorization {
    pub url: String,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub kind: ServiceKind,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Money {
    pub minor_units: i64,
    pub currency: String,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Bill {
    pub id: String,
    pub account_id: String,
    pub provider: ProviderId,
    pub issued_on: NaiveDate,
    pub due_on: Option<NaiveDate>,
    pub period_from: Option<NaiveDate>,
    pub period_to: Option<NaiveDate>,
    /// The whole provider account bill, including meters not selected for usage.
    pub total: Money,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_charges: Option<Money>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reported_quantities: Vec<ReportedQuantity>,
    pub outstanding_balance: Option<Money>,
    /// Source status only. Absence of a balance does not mean paid.
    pub source_status: Option<String>,
    pub observed_at: DateTime<Utc>,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ReportedQuantity {
    pub kind: ReportedQuantityKind,
    pub service_id: String,
    pub quantity: crate::imports::Quantity,
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReportedQuantityKind {
    BilledWater,
    PropaneDelivered,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Balance {
    pub id: String,
    pub account_id: String,
    pub amount: Money,
    pub as_of: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Payment {
    pub id: String,
    pub account_id: String,
    pub bill_id: Option<String>,
    pub amount: Money,
    pub paid_on: NaiveDate,
    pub source_status: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CollectionStatus {
    Scheduled,
    Completed,
    Exception,
    Unknown,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Collection {
    pub id: String,
    pub account_id: String,
    pub service_id: String,
    pub scheduled_date: NaiveDate,
    pub status: CollectionStatus,
    pub completed_at: Option<DateTime<Utc>>,
    pub eta: Option<DateTime<Utc>>,
    pub exception: Option<String>,
    pub observed_at: DateTime<Utc>,
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum UsageUnit {
    Wh,
    KWh,
    Therm,
    Ccf,
    Gallon,
    CubicMeter,
    Byte,
    Gigabyte,
    Terabyte,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Usage {
    pub id: String,
    pub account_id: String,
    pub service_id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[serde(with = "rust_decimal::serde::str")]
    #[schemars(with = "String")]
    pub quantity: Decimal,
    pub unit: UsageUnit,
    pub observed_at: DateTime<Utc>,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Observation {
    Bill(Bill),
    Balance(Balance),
    Payment(Payment),
    Service(Service),
    Collection(Collection),
    Usage(Usage),
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoverageState {
    Complete,
    Partial,
    Unavailable,
    Unsupported,
    NotConfigured,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Coverage {
    pub account_id: String,
    pub capability: Capability,
    pub state: CoverageState,
    /// A fixed, non-sensitive machine-readable reason, never a provider response body.
    pub reason: Option<String>,
    pub checked_at: DateTime<Utc>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_page: Option<String>,
    pub coverage: Vec<Coverage>,
}

#[derive(JsonSchema, Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_provider: Option<ProviderId>,
    pub service: Option<String>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    pub limit: Option<u32>,
    pub page: Option<String>,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Summary {
    pub accounts: usize,
    pub bills: u64,
    pub collections: u64,
    pub usage_intervals: u64,
    pub coverage: Vec<Coverage>,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub last_imported_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub import_only_accounts: usize,
    pub stale: bool,
    #[serde(default)]
    pub stale_automatic_accounts: Vec<StaleAutomaticAccount>,
    #[serde(default)]
    pub manual_accounts_stale: bool,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StaleAutomaticAccount {
    pub account_id: String,
    pub name: String,
    pub last_data_at: Option<DateTime<Utc>>,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Health {
    pub status: String,
    pub version: String,
    pub last_synced_at: Option<DateTime<Utc>>,
    pub stale: bool,
    pub coverage: Vec<Coverage>,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct System {
    pub name: String,
    pub version: String,
    pub read_only_providers: bool,
    #[serde(default)]
    pub read_only_sync: bool,
    #[serde(default)]
    pub hosted_connections: bool,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SyncInput {
    pub account: String,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HistorySyncInput {
    pub account: String,
    pub range: HistoryRange,
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRange {
    All,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SyncSelection {
    Dates(SyncInput),
    History(HistorySyncInput),
}

#[derive(JsonSchema, Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    Running,
    Complete,
    Partial,
    Failed,
    Interrupted,
}

#[derive(JsonSchema, Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SyncJob {
    pub id: String,
    pub account_id: String,
    pub state: SyncState,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub observations: u64,
    pub error: Option<String>,
}
