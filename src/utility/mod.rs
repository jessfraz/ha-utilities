//! Providers own HTTP, authentication, paging, and wire models; the store owns reconciliation.
pub mod bayou;
pub mod waste_management;

use crate::{
    config::AccountBinding,
    models::{Capability, Coverage, Observation},
};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::future::Future;

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

pub trait Utility: Send + Sync {
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
