//! Typed facts prepared by the collector, with references to original evidence.
use crate::{
    config::{AccountBinding, ServiceBinding, valid_id},
    models::{
        Balance, Bill, Money, Observation, Payment, ProviderId, ReportedQuantity,
        ReportedQuantityKind, Service, ServiceKind, Usage, UsageUnit,
    },
};
use chrono::{DateTime, NaiveDate, Utc};
use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_IMPORT_REQUEST_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("invalid import request")]
    Invalid,
    #[error("import binding conflicts with a saved account")]
    Binding,
    #[error("a source document is missing")]
    MissingDocument,
    #[error("an existing record has conflicting facts")]
    Conflict,
    #[error("import store operation failed")]
    Store(#[from] crate::store::StoreError),
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    pub account: ImportAccount,
    pub currency: String,
    pub records: Vec<ImportRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportAccount {
    pub id: String,
    pub name: String,
    pub provider: ProviderId,
    pub start_on: NaiveDate,
    #[serde(default)]
    pub provider_account_sha256: Option<String>,
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ImportReceipt {
    pub inserted: usize,
    pub enriched: usize,
    pub evidence_added: usize,
    pub unchanged: usize,
    pub corrected: usize,
    pub replayed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportAccountPurge {
    pub provider: ProviderId,
    pub expected_records: u64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ImportAccountPurgeReceipt {
    pub removed: bool,
    pub records: u64,
    pub documents: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImportRecord {
    Bill(BillInput),
    Payment(PaymentInput),
    MeterInterval(MeterIntervalInput),
    PropaneDelivery(DeliveryInput),
    BalanceSnapshot(BalanceInput),
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BillReference {
    InvoiceNumber(String),
    StatementNumber(String),
    StatementCycle(String),
}

impl BillReference {
    fn value(&self) -> &str {
        match self {
            Self::InvoiceNumber(value)
            | Self::StatementNumber(value)
            | Self::StatementCycle(value) => value,
        }
    }

    fn valid(&self, provider: ProviderId) -> bool {
        match self {
            Self::StatementCycle(cycle) => {
                provider == ProviderId::Spectrum
                    && NaiveDate::parse_from_str(&format!("{cycle}-01"), "%Y-%m-%d")
                        .is_ok_and(|date| date.format("%Y-%m").to_string() == *cycle)
            }
            _ => text(self.value(), 160),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Period {
    pub start: NaiveDate,
    pub end_exclusive: NaiveDate,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BillInput {
    pub reference: BillReference,
    #[serde(default)]
    pub scope: BillScope,
    pub issued_on: NaiveDate,
    pub due_on: Option<NaiveDate>,
    pub period: Option<Period>,
    pub totals: BillTotals,
    #[serde(default)]
    pub services: Vec<ServiceDetail>,
    #[serde(default)]
    pub charges: Vec<ChargeLine>,
    pub sources: Vec<SourceRef>,
    pub replaces_revision: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BillScope {
    #[default]
    Account,
    SelectedServices,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BillTotals {
    pub previous_balance_minor: Option<i64>,
    pub payments_received_minor: Option<i64>,
    pub adjustments_minor: Option<i64>,
    pub new_charges_minor: Option<i64>,
    pub amount_due_minor: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PaymentInput {
    pub reference: String,
    pub bill: Option<BillReference>,
    pub paid_on: NaiveDate,
    pub amount_minor: i64,
    pub status: PaymentStatus,
    pub sources: Vec<SourceRef>,
    pub replaces_revision: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    Confirmed,
    Reported,
    Pending,
    Failed,
    Reversed,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MeterIntervalInput {
    pub service_id: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub quantity: Quantity,
    pub quality: Quality,
    pub sources: Vec<SourceRef>,
    pub replaces_revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DeliveryInput {
    pub ticket: String,
    pub service_id: String,
    pub delivered_on: NaiveDate,
    pub volume: Quantity,
    pub bill: Option<BillReference>,
    pub sources: Vec<SourceRef>,
    pub replaces_revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BalanceInput {
    pub as_of: DateTime<Utc>,
    pub amount_minor: i64,
    pub sources: Vec<SourceRef>,
    pub replaces_revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceDetail {
    Water {
        service_id: String,
        billed_volume: Option<Quantity>,
        #[serde(default)]
        meter_reads: Vec<MeterRead>,
        current_budget: Option<Quantity>,
        next_estimated_budget: Option<Quantity>,
        meter_size: Option<String>,
        eto_inches: Option<ExactDecimal>,
    },
    Internet {
        service_id: String,
        plan: Option<String>,
        period: Option<Period>,
        allowance: Option<Quantity>,
        overage: Option<Quantity>,
    },
    Propane {
        service_id: String,
        delivery_on: Option<NaiveDate>,
        ticket: Option<String>,
        delivered_volume: Option<Quantity>,
    },
    Waste(WasteDetail),
    Recycling(WasteDetail),
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WasteDetail {
    pub service_id: String,
    pub container_size: Option<String>,
    pub pickup_frequency: Option<String>,
    pub period: Option<Period>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MeterRead {
    pub on: NaiveDate,
    pub register: ExactDecimal,
    pub quality: Quality,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    Actual,
    Estimated,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Quantity {
    pub value: ExactDecimal,
    pub unit: QuantityUnit,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuantityUnit {
    HundredCubicFeet,
    UsGallon,
    CubicMeter,
    Byte,
    Gigabyte,
    Terabyte,
    Count,
    CubicYard,
    Day,
    Month,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(transparent)]
pub struct ExactDecimal(
    #[serde(
        deserialize_with = "decimal",
        serialize_with = "rust_decimal::serde::str::serialize"
    )]
    #[schemars(with = "String")]
    pub Decimal,
);

fn decimal<'de, D: serde::Deserializer<'de>>(input: D) -> Result<Decimal, D::Error> {
    rust_decimal::serde::str::deserialize(input).map(|value| value.normalize())
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChargeLine {
    pub kind: ChargeKind,
    pub label: String,
    pub amount_minor: i64,
    pub service_id: Option<String>,
    pub quantity: Option<Quantity>,
    pub rate: Option<UnitRate>,
    pub period: Option<Period>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChargeKind {
    Usage,
    Service,
    Tax,
    Fee,
    Credit,
    Adjustment,
    Equipment,
    Installation,
    Delivery,
    Other,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UnitRate {
    pub amount: ExactDecimal,
    pub per: QuantityUnit,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct SourceRef {
    pub sha256: String,
    pub role: SourceRole,
    #[serde(default)]
    pub pages: Vec<u32>,
}

#[derive(
    Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, PartialOrd, Ord,
)]
#[serde(rename_all = "snake_case")]
pub enum SourceRole {
    Invoice,
    Statement,
    Supporting,
    PaymentConfirmation,
    DeliveryTicket,
    UsageExport,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ImportedRecord {
    pub id: String,
    pub revision: String,
    pub record: ImportRecord,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct BillDetails {
    pub currency: String,
    pub bill: ImportedRecord,
    pub related: Vec<ImportedRecord>,
}

pub fn digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 15)]));
    }
    value
}

pub fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn text(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn service_matches(provider: ProviderId, service: ServiceKind) -> bool {
    match provider {
        ProviderId::Lvmwd => service == ServiceKind::Water,
        ProviderId::SuperiorPropane => service == ServiceKind::Propane,
        ProviderId::Starlink | ProviderId::Spectrum => service == ServiceKind::Internet,
        ProviderId::WasteManagement => {
            matches!(service, ServiceKind::Waste | ServiceKind::Recycling)
        }
        ProviderId::Bayou => false,
    }
}

impl ImportRequest {
    pub fn validate(&self) -> Result<(), ImportError> {
        let account = &self.account;
        let mut service_ids = BTreeSet::new();
        if !valid_id(&account.id)
            || !text(&account.name, 100)
            || account.provider == ProviderId::Bayou
            || account
                .provider_account_sha256
                .as_deref()
                .is_some_and(|d| !valid_digest(d))
            || !matches!(self.currency.as_str(), "USD" | "CAD")
            || account.services.len() > 64
            || self.records.is_empty()
            || self.records.len() > 200
            || account.services.iter().any(|s| {
                !valid_id(&s.id)
                    || !text(&s.name, 100)
                    || !service_matches(account.provider, s.kind)
                    || !service_ids.insert(&s.id)
            })
        {
            return Err(ImportError::Invalid);
        }
        let mut ids = BTreeSet::new();
        for record in &self.records {
            record.validate(account)?;
            if !ids.insert(record.id(&account.id)?) {
                return Err(ImportError::Invalid);
            }
        }
        Ok(())
    }

    pub fn document_digests(&self) -> impl Iterator<Item = &str> {
        self.records
            .iter()
            .flat_map(|record| record.sources().iter().map(|source| source.sha256.as_str()))
    }

    pub(crate) fn binding(&self, deployment: &str) -> AccountBinding {
        let account = &self.account;
        let mut services: Vec<_> = account
            .services
            .iter()
            .map(|service| ServiceBinding {
                id: service.id.clone(),
                name: service.name.clone(),
                provider_service: service.id.clone(),
                kind: service.kind,
            })
            .collect();
        services.sort_by(|a, b| a.id.cmp(&b.id));
        AccountBinding {
            id: account.id.clone(),
            name: account.name.clone(),
            deployment: deployment.to_owned(),
            provider: account.provider,
            customer_id: "document_import".to_owned(),
            provider_account: account
                .provider_account_sha256
                .clone()
                .unwrap_or_else(|| account.id.clone()),
            currency: self.currency.clone(),
            services,
        }
    }
}

impl Quantity {
    fn valid(self, kind: ServiceKind) -> bool {
        !self.value.0.is_sign_negative() && self.unit.matches(kind)
    }
}

impl QuantityUnit {
    fn matches(self, kind: ServiceKind) -> bool {
        match kind {
            ServiceKind::Water => matches!(
                self,
                Self::HundredCubicFeet | Self::UsGallon | Self::CubicMeter
            ),
            ServiceKind::Propane => self == Self::UsGallon,
            ServiceKind::Internet => matches!(self, Self::Byte | Self::Gigabyte | Self::Terabyte),
            ServiceKind::Waste | ServiceKind::Recycling => {
                matches!(self, Self::Count | Self::CubicYard)
            }
            _ => false,
        }
    }
}

impl ChargeLine {
    fn valid_units(&self, account: &ImportAccount) -> bool {
        let valid_unit = |unit: QuantityUnit| {
            (self.kind != ChargeKind::Usage
                && matches!(
                    unit,
                    QuantityUnit::Count | QuantityUnit::Day | QuantityUnit::Month
                ))
                || account.services.iter().any(|service| {
                    self.service_id.as_ref().is_none_or(|id| *id == service.id)
                        && unit.matches(service.kind)
                })
        };
        self.quantity.is_none_or(|quantity| {
            valid_unit(quantity.unit)
                && (!quantity.value.0.is_sign_negative()
                    || matches!(self.kind, ChargeKind::Credit | ChargeKind::Adjustment))
        }) && self.rate.is_none_or(|rate| valid_unit(rate.per))
            && self
                .quantity
                .zip(self.rate)
                .is_none_or(|(quantity, rate)| quantity.unit == rate.per)
    }
}

impl ImportRecord {
    pub fn sources(&self) -> &[SourceRef] {
        match self {
            Self::Bill(value) => &value.sources,
            Self::Payment(value) => &value.sources,
            Self::MeterInterval(value) => &value.sources,
            Self::PropaneDelivery(value) => &value.sources,
            Self::BalanceSnapshot(value) => &value.sources,
        }
    }

    pub(crate) fn provenance_mut(&mut self) -> (&mut Vec<SourceRef>, &mut Option<String>) {
        match self {
            Self::Bill(value) => (&mut value.sources, &mut value.replaces_revision),
            Self::Payment(value) => (&mut value.sources, &mut value.replaces_revision),
            Self::MeterInterval(value) => (&mut value.sources, &mut value.replaces_revision),
            Self::PropaneDelivery(value) => (&mut value.sources, &mut value.replaces_revision),
            Self::BalanceSnapshot(value) => (&mut value.sources, &mut value.replaces_revision),
        }
    }

    pub(crate) fn replacement(&self) -> Option<&str> {
        match self {
            Self::Bill(value) => value.replaces_revision.as_deref(),
            Self::Payment(value) => value.replaces_revision.as_deref(),
            Self::MeterInterval(value) => value.replaces_revision.as_deref(),
            Self::PropaneDelivery(value) => value.replaces_revision.as_deref(),
            Self::BalanceSnapshot(value) => value.replaces_revision.as_deref(),
        }
    }

    pub(crate) fn facts(&self) -> Self {
        let mut result = self.clone();
        let (sources, replacement) = result.provenance_mut();
        sources.clear();
        *replacement = None;
        result
    }

    pub(crate) fn validate(&self, account: &ImportAccount) -> Result<(), ImportError> {
        let period_ok = |period: Period| {
            period.start >= account.start_on && period.start < period.end_exclusive
        };
        let service = |id: &str| account.services.iter().find(|s| s.id == id).map(|s| s.kind);
        let reference_ok = |reference: &BillReference| reference.valid(account.provider);
        let sources = self.sources();
        if sources.is_empty()
            || sources.len() > 100
            || sources
                .iter()
                .any(|s| !valid_digest(&s.sha256) || s.pages.len() > 100 || s.pages.contains(&0))
            || self.replacement().is_some_and(|r| !valid_digest(r))
        {
            return Err(ImportError::Invalid);
        }
        let valid = match self {
            Self::Bill(bill) => {
                let total = bill
                    .totals
                    .new_charges_minor
                    .or(bill.totals.amount_due_minor);
                let scoped = bill.scope == BillScope::Account
                    || (bill.totals.amount_due_minor.is_none()
                        && bill.totals.previous_balance_minor.is_none()
                        && bill.totals.payments_received_minor.is_none()
                        && bill.totals.adjustments_minor.is_none()
                        && bill.totals.new_charges_minor.map(i128::from)
                            == Some(
                                bill.charges
                                    .iter()
                                    .map(|line| i128::from(line.amount_minor))
                                    .sum(),
                            )
                        && !bill.charges.is_empty()
                        && bill.charges.iter().all(|line| line.service_id.is_some()));
                scoped
                    && reference_ok(&bill.reference)
                    && match &bill.reference {
                        BillReference::StatementCycle(cycle) => {
                            bill.period.is_some()
                                && bill.issued_on.format("%Y-%m").to_string() == *cycle
                        }
                        _ => true,
                    }
                    && total.is_some()
                    && bill.issued_on >= account.start_on
                    && bill.period.is_none_or(period_ok)
                    && bill.services.len() <= 64
                    && bill.charges.len() <= 500
                    && bill.charges.iter().all(|line| {
                        text(&line.label, 240)
                            && line
                                .service_id
                                .as_deref()
                                .is_none_or(|id| service(id).is_some())
                            && line.period.is_none_or(period_ok)
                            && line.valid_units(account)
                    })
                    && bill.services.iter().all(|item| match item {
                        ServiceDetail::Water {
                            service_id,
                            billed_volume,
                            meter_reads,
                            current_budget,
                            next_estimated_budget,
                            meter_size,
                            eto_inches,
                        } => {
                            service(service_id) == Some(ServiceKind::Water)
                                && [billed_volume, current_budget, next_estimated_budget]
                                    .into_iter()
                                    .flatten()
                                    .all(|q| q.valid(ServiceKind::Water))
                                && meter_reads.len() <= 32
                                && meter_reads.iter().all(|r| {
                                    r.on >= account.start_on && !r.register.0.is_sign_negative()
                                })
                                && meter_size.as_deref().is_none_or(|s| text(s, 100))
                                && eto_inches.is_none_or(|v| !v.0.is_sign_negative())
                        }
                        ServiceDetail::Internet {
                            service_id,
                            plan,
                            period,
                            allowance,
                            overage,
                        } => {
                            service(service_id) == Some(ServiceKind::Internet)
                                && period.is_none_or(period_ok)
                                && plan.as_deref().is_none_or(|p| text(p, 160))
                                && [allowance, overage]
                                    .into_iter()
                                    .flatten()
                                    .all(|q| q.valid(ServiceKind::Internet))
                        }
                        ServiceDetail::Propane {
                            service_id,
                            delivery_on,
                            ticket,
                            delivered_volume,
                        } => {
                            service(service_id) == Some(ServiceKind::Propane)
                                && delivery_on.is_none_or(|on| on >= account.start_on)
                                && ticket.as_deref().is_none_or(|t| text(t, 160))
                                && delivered_volume.is_none_or(|q| q.valid(ServiceKind::Propane))
                        }
                        ServiceDetail::Waste(detail) | ServiceDetail::Recycling(detail) => {
                            service(&detail.service_id)
                                == Some(if matches!(item, ServiceDetail::Waste(_)) {
                                    ServiceKind::Waste
                                } else {
                                    ServiceKind::Recycling
                                })
                                && detail.period.is_none_or(period_ok)
                                && [&detail.container_size, &detail.pickup_frequency]
                                    .into_iter()
                                    .flatten()
                                    .all(|s| text(s, 100))
                        }
                    })
            }
            Self::Payment(value) => {
                value.paid_on >= account.start_on
                    && value.amount_minor >= 0
                    && text(&value.reference, 160)
                    && value.bill.as_ref().is_none_or(reference_ok)
            }
            Self::MeterInterval(value) => {
                value.start < value.end
                    && value.start.date_naive() >= account.start_on
                    && service(&value.service_id).is_some_and(|kind| {
                        matches!(
                            kind,
                            ServiceKind::Water | ServiceKind::Internet | ServiceKind::Propane
                        ) && value.quantity.valid(kind)
                    })
            }
            Self::PropaneDelivery(value) => {
                value.delivered_on >= account.start_on
                    && service(&value.service_id) == Some(ServiceKind::Propane)
                    && text(&value.ticket, 160)
                    && value.volume.valid(ServiceKind::Propane)
                    && value.bill.as_ref().is_none_or(reference_ok)
            }
            Self::BalanceSnapshot(value) => value.as_of.date_naive() >= account.start_on,
        };
        if valid {
            Ok(())
        } else {
            Err(ImportError::Invalid)
        }
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Bill(_) => "bill",
            Self::Payment(_) => "payment",
            Self::MeterInterval(_) => "meter_interval",
            Self::PropaneDelivery(_) => "propane_delivery",
            Self::BalanceSnapshot(_) => "balance_snapshot",
        }
    }

    pub(crate) fn id(&self, account: &str) -> Result<String, serde_json::Error> {
        let key = match self {
            Self::Bill(value) => return bill_id(account, &value.reference),
            Self::Payment(value) => serde_json::to_vec(&value.reference)?,
            Self::MeterInterval(value) => {
                serde_json::to_vec(&(&value.service_id, value.start, value.end))?
            }
            Self::PropaneDelivery(value) => {
                serde_json::to_vec(&(&value.service_id, &value.ticket))?
            }
            Self::BalanceSnapshot(value) => serde_json::to_vec(&value.as_of)?,
        };
        Ok(format!("{account}:import:{}:{}", self.kind(), digest(&key)))
    }

    pub(crate) fn bill_link(&self, account: &str) -> Result<Option<String>, serde_json::Error> {
        let reference = match self {
            Self::Payment(value) => value.bill.as_ref(),
            Self::PropaneDelivery(value) => value.bill.as_ref(),
            _ => None,
        };
        reference.map(|value| bill_id(account, value)).transpose()
    }

    pub(crate) fn reconcile(
        &self,
        current: &Self,
        revision: &str,
    ) -> Result<(Self, bool), ImportError> {
        let mut incoming = self.facts();
        let old = current.facts();
        let corrected = self.replacement() == Some(revision) && incoming != old;
        if !corrected && incoming != old {
            match (&mut incoming, &old) {
                (Self::Bill(new), Self::Bill(old))
                    if new.reference == old.reference
                        && new.issued_on == old.issued_on
                        && new.scope == old.scope =>
                {
                    merge(&mut new.due_on, &old.due_on)?;
                    merge(&mut new.period, &old.period)?;
                    merge(
                        &mut new.totals.previous_balance_minor,
                        &old.totals.previous_balance_minor,
                    )?;
                    merge(
                        &mut new.totals.payments_received_minor,
                        &old.totals.payments_received_minor,
                    )?;
                    merge(
                        &mut new.totals.adjustments_minor,
                        &old.totals.adjustments_minor,
                    )?;
                    merge(
                        &mut new.totals.new_charges_minor,
                        &old.totals.new_charges_minor,
                    )?;
                    merge(
                        &mut new.totals.amount_due_minor,
                        &old.totals.amount_due_minor,
                    )?;
                    merge_list(&mut new.services, &old.services)?;
                    merge_list(&mut new.charges, &old.charges)?;
                }
                (Self::Payment(new), Self::Payment(old)) => {
                    merge(&mut new.bill, &old.bill)?;
                    if new.status == PaymentStatus::Reported
                        && old.status == PaymentStatus::Confirmed
                    {
                        new.status = old.status;
                    }
                    if new.reference != old.reference
                        || new.paid_on != old.paid_on
                        || new.amount_minor != old.amount_minor
                        || (new.status != old.status
                            && !(new.status == PaymentStatus::Confirmed
                                && old.status == PaymentStatus::Reported))
                    {
                        return Err(ImportError::Conflict);
                    }
                }
                _ => return Err(ImportError::Conflict),
            }
        }
        *incoming.provenance_mut().0 = if corrected {
            combined_sources(self.sources().iter())
        } else {
            combined_sources(current.sources().iter().chain(self.sources()))
        };
        Ok((incoming, corrected))
    }

    pub(crate) fn observation(
        &self,
        binding: &AccountBinding,
        id: &str,
        observed_at: DateTime<Utc>,
    ) -> Result<Option<Observation>, ImportError> {
        let money = |minor_units| Money {
            minor_units,
            currency: binding.currency.clone(),
        };
        let account_id = binding.id.clone();
        let id = id.to_owned();
        Ok(Some(match self {
            Self::Bill(value) => Observation::Bill(Bill {
                id,
                account_id,
                provider: binding.provider,
                issued_on: value.issued_on,
                due_on: value.due_on,
                period_from: value.period.map(|p| p.start),
                period_to: value.period.and_then(|p| p.end_exclusive.pred_opt()),
                total: money(
                    value
                        .totals
                        .new_charges_minor
                        .or(value.totals.amount_due_minor)
                        .ok_or(ImportError::Invalid)?,
                ),
                new_charges: value.totals.new_charges_minor.map(money),
                reported_quantities: value
                    .services
                    .iter()
                    .filter_map(|service| match service {
                        ServiceDetail::Water {
                            service_id,
                            billed_volume: Some(quantity),
                            ..
                        } => Some(ReportedQuantity {
                            kind: ReportedQuantityKind::BilledWater,
                            service_id: service_id.clone(),
                            quantity: *quantity,
                        }),
                        ServiceDetail::Propane {
                            service_id,
                            delivered_volume: Some(quantity),
                            ..
                        } => Some(ReportedQuantity {
                            kind: ReportedQuantityKind::PropaneDelivered,
                            service_id: service_id.clone(),
                            quantity: *quantity,
                        }),
                        _ => None,
                    })
                    .collect(),
                outstanding_balance: None,
                source_status: None,
                observed_at,
            }),
            Self::Payment(value) => Observation::Payment(Payment {
                bill_id: self
                    .bill_link(&account_id)
                    .map_err(crate::store::StoreError::from)?,
                id,
                account_id,
                paid_on: value.paid_on,
                amount: money(value.amount_minor),
                source_status: match value.status {
                    PaymentStatus::Confirmed => "confirmed",
                    PaymentStatus::Reported => "reported_payment",
                    PaymentStatus::Pending => "pending",
                    PaymentStatus::Failed => "failed",
                    PaymentStatus::Reversed => "reversed",
                }
                .to_owned(),
                observed_at,
            }),
            Self::MeterInterval(value) if value.quality != Quality::Estimated => {
                let unit = match value.quantity.unit {
                    QuantityUnit::HundredCubicFeet => UsageUnit::Ccf,
                    QuantityUnit::UsGallon => UsageUnit::Gallon,
                    QuantityUnit::CubicMeter => UsageUnit::CubicMeter,
                    QuantityUnit::Byte => UsageUnit::Byte,
                    QuantityUnit::Gigabyte => UsageUnit::Gigabyte,
                    QuantityUnit::Terabyte => UsageUnit::Terabyte,
                    _ => return Ok(None),
                };
                Observation::Usage(Usage {
                    id,
                    account_id,
                    service_id: value.service_id.clone(),
                    start: value.start,
                    end: value.end,
                    quantity: value.quantity.value.0,
                    unit,
                    observed_at,
                })
            }
            Self::BalanceSnapshot(value) => Observation::Balance(Balance {
                id,
                account_id,
                amount: money(value.amount_minor),
                as_of: value.as_of,
                observed_at,
            }),
            Self::PropaneDelivery(_) | Self::MeterInterval(_) => return Ok(None),
        }))
    }
}

fn merge<T: Clone + Eq>(new: &mut Option<T>, old: &Option<T>) -> Result<(), ImportError> {
    if new.is_none() {
        new.clone_from(old);
    } else if old.is_some() && new != old {
        return Err(ImportError::Conflict);
    }
    Ok(())
}

fn merge_list<T: Clone + Eq>(new: &mut Vec<T>, old: &Vec<T>) -> Result<(), ImportError> {
    if new.is_empty() {
        new.clone_from(old);
    } else if !old.is_empty() && new != old {
        return Err(ImportError::Conflict);
    }
    Ok(())
}

pub(crate) fn combined_sources<'a>(sources: impl Iterator<Item = &'a SourceRef>) -> Vec<SourceRef> {
    let mut combined = BTreeMap::<_, BTreeSet<_>>::new();
    for source in sources {
        combined
            .entry((source.sha256.clone(), source.role))
            .or_default()
            .extend(source.pages.iter().copied());
    }
    combined
        .into_iter()
        .map(|((sha256, role), pages)| SourceRef {
            sha256,
            role,
            pages: pages.into_iter().collect(),
        })
        .collect()
}

fn bill_id(account: &str, reference: &BillReference) -> Result<String, serde_json::Error> {
    Ok(format!(
        "{account}:import:bill:{}",
        digest(&serde_json::to_vec(reference)?)
    ))
}
