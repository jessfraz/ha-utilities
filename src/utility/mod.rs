//! Providers own HTTP, authentication, paging, and wire models; the store owns reconciliation.
pub mod bayou;
pub mod waste_management;

use crate::{
    config::AccountBinding,
    models::{Capability, Coverage, LiveProvider, Observation},
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::{future::Future, io::Write};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct Continuation(pub String);

#[derive(Clone, Debug)]
pub struct SyncRequest {
    pub binding: AccountBinding,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub continuation: Option<Continuation>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct HistoryRequest {
    pub binding: AccountBinding,
    pub continuation: Option<Continuation>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct SyncPage {
    pub observations: Vec<Observation>,
    pub coverage: Vec<Coverage>,
    pub continuation: Option<Continuation>,
}

#[derive(Debug, thiserror::Error)]
pub enum UtilityError {
    #[error("provider credentials are not configured")]
    NotConfigured,
    #[error("provider authentication failed")]
    Authentication,
    #[error("provider access denied")]
    AccessDenied,
    #[error("provider rate limited the request")]
    RateLimited,
    #[error("provider is unavailable")]
    Unavailable,
    #[error("provider is preparing history")]
    HistoryPreparing,
    #[error("provider returned an invalid response")]
    InvalidResponse,
    #[error("provider account or service does not match the configured binding")]
    BindingMismatch,
    #[error("provider continuation is invalid")]
    InvalidContinuation,
}

impl UtilityError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured => "credentials_not_configured",
            Self::Authentication => "authentication_required",
            Self::AccessDenied => "access_denied",
            Self::RateLimited => "rate_limited",
            Self::Unavailable => "provider_unavailable",
            Self::HistoryPreparing => "provider_preparing_history",
            Self::InvalidResponse => "invalid_response",
            Self::BindingMismatch => "binding_mismatch",
            Self::InvalidContinuation => "invalid_continuation",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SyncStage {
    Worker,
    History,
    DateRange,
    Customer,
    Bills,
    Usage,
}

#[derive(Serialize)]
struct SyncFailure {
    event: &'static str,
    provider: Option<LiveProvider>,
    stage: SyncStage,
    reason: &'static str,
}

fn write_failure(
    output: &mut impl Write,
    provider: Option<LiveProvider>,
    stage: SyncStage,
    reason: &'static str,
) {
    // Only closed enums and fixed error codes can reach the native app log.
    let event = SyncFailure {
        event: "utility_sync_failed",
        provider,
        stage,
        reason,
    };
    if serde_json::to_writer(&mut *output, &event).is_ok() {
        let _ = output.write_all(b"\n");
    }
}

pub(crate) fn report_failure(provider: LiveProvider, stage: SyncStage, error: &UtilityError) {
    write_failure(
        &mut std::io::stderr().lock(),
        Some(provider),
        stage,
        error.code(),
    );
}

pub(crate) fn report_terminal_failure(provider: Option<LiveProvider>, reason: &'static str) {
    let code = match reason {
        "credentials_not_configured"
        | "authentication_required"
        | "access_denied"
        | "rate_limited"
        | "provider_unavailable"
        | "provider_preparing_history"
        | "invalid_response"
        | "binding_mismatch"
        | "invalid_continuation"
        | "sync_timeout"
        | "ingest_rejected"
        | "database_error"
        | "process_interrupted"
        | "sync_page_budget_exceeded"
        | "sync_queue_unavailable" => reason,
        _ => "sync_failed",
    };
    write_failure(
        &mut std::io::stderr().lock(),
        provider,
        SyncStage::Worker,
        code,
    );
}

pub trait Utility: Send + Sync {
    /// Every adapter produces live observations. Manual documents use the import boundary.
    fn provider(&self) -> LiveProvider;
    fn capabilities(&self) -> &'static [Capability];
    fn sync(
        &self,
        request: SyncRequest,
    ) -> impl Future<Output = Result<SyncPage, UtilityError>> + Send;

    fn sync_history(
        &self,
        request: HistoryRequest,
    ) -> impl Future<Output = Result<SyncPage, UtilityError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_failure_event_contains_only_provider_stage_and_fixed_reason() {
        let mut output = Vec::new();
        write_failure(
            &mut output,
            Some(LiveProvider::Bayou),
            SyncStage::Usage,
            UtilityError::BindingMismatch.code(),
        );
        let event: std::collections::BTreeMap<String, String> =
            serde_json::from_slice(&output).unwrap();
        assert_eq!(
            event,
            std::collections::BTreeMap::from([
                ("event".to_owned(), "utility_sync_failed".to_owned()),
                ("provider".to_owned(), "bayou".to_owned()),
                ("stage".to_owned(), "usage".to_owned()),
                ("reason".to_owned(), "binding_mismatch".to_owned()),
            ])
        );
        assert_eq!(output.last(), Some(&b'\n'));
    }
}
