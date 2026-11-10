//! Native and HA transports share the same typed local read model.
use crate::models::*;
use reqwest::{
    Client as HttpClient, Method,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;
use url::Url;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Transport {
    Native,
    #[value(name = "home-assistant", alias = "ha")]
    Ha,
}
#[derive(Debug, thiserror::Error, Serialize)]
#[error("{message}")]
pub struct ClientError {
    pub error: &'static str,
    pub message: String,
    pub status: Option<u16>,
}
impl ClientError {
    pub fn new(error: &'static str, status: Option<u16>) -> Self {
        Self {
            error,
            message: error.replace('_', " "),
            status,
        }
    }
}
pub struct Client {
    http: HttpClient,
    base: Url,
    transport: Transport,
    authorization: HeaderValue,
}
impl Client {
    pub fn new(url: &str, transport: Transport, token: &str) -> Result<Self, ClientError> {
        let base = Url::parse(url).map_err(|_| ClientError::new("invalid_url", None))?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.path() != "/"
        {
            return Err(ClientError::new("invalid_url", None));
        }
        if token.is_empty() || token.len() > 8192 || !token.bytes().all(|v| v.is_ascii_graphic()) {
            return Err(ClientError::new("invalid_credential", None));
        }
        let mut authorization = HeaderValue::from_bytes(format!("Bearer {token}").as_bytes())
            .map_err(|_| ClientError::new("invalid_credential", None))?;
        authorization.set_sensitive(true);
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| ClientError::new("client_unavailable", None))?;
        Ok(Self {
            http,
            base,
            transport,
            authorization,
        })
    }
    fn endpoint(
        &self,
        resource: &str,
        id: Option<&str>,
        query: Option<&ListQuery>,
    ) -> Result<Url, ClientError> {
        let mut url = self.base.clone();
        {
            let mut path = url
                .path_segments_mut()
                .map_err(|_| ClientError::new("invalid_url", None))?;
            path.pop_if_empty();
            match self.transport {
                Transport::Native => {
                    if resource != "health" {
                        path.push("v1");
                    }
                }
                Transport::Ha => {
                    path.push("api").push("utilities").push("v1");
                }
            }
            path.push(resource);
            if let Some(id) = id {
                path.push(id);
            }
        }
        if let Some(query) = query {
            let mut pairs = url.query_pairs_mut();
            if let Some(account) = &query.account {
                pairs.append_pair("account", account);
            }
            if let Some(service) = &query.service {
                pairs.append_pair("service", service);
            }
            if let Some(from) = query.from {
                pairs.append_pair("from", &from.to_string());
            }
            if let Some(to) = query.to {
                pairs.append_pair("to", &to.to_string());
            }
            if let Some(limit) = query.limit {
                pairs.append_pair("limit", &limit.to_string());
            }
            if let Some(page) = &query.page {
                pairs.append_pair("page", page);
            }
        }
        Ok(url)
    }
    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<T, ClientError> {
        let mut builder = self
            .http
            .request(method, url)
            .header(AUTHORIZATION, self.authorization.clone())
            .header("Accept", "application/json");
        if let Some(body) = body {
            builder = builder
                .header("Content-Type", "application/json")
                .body(body);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|_| ClientError::new("service_unavailable", None))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ClientError::new(
                match status.as_u16() {
                    400 => "invalid_request",
                    401 => "authentication_required",
                    403 => "access_denied",
                    404 => "not_found",
                    409 => "sync_busy",
                    429 => "rate_limited",
                    _ => "service_error",
                },
                Some(status.as_u16()),
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ClientError::new("service_unavailable", None))?
        {
            if body.len() + chunk.len() > 8 * 1024 * 1024 {
                return Err(ClientError::new("response_too_large", None));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map_err(|_| ClientError::new("invalid_response", Some(status.as_u16())))
    }
    async fn get<T: DeserializeOwned>(
        &self,
        resource: &str,
        id: Option<&str>,
        query: Option<&ListQuery>,
    ) -> Result<T, ClientError> {
        self.request(Method::GET, self.endpoint(resource, id, query)?, None)
            .await
    }
    pub async fn health(&self) -> Result<Health, ClientError> {
        self.get("health", None, None).await
    }
    pub async fn system(&self) -> Result<System, ClientError> {
        self.get("system", None, None).await
    }
    pub async fn providers(&self) -> Result<Page<ProviderInfo>, ClientError> {
        self.get("providers", None, None).await
    }
    pub async fn accounts(&self) -> Result<Page<Account>, ClientError> {
        self.get("accounts", None, None).await
    }
    pub async fn summary(&self) -> Result<Summary, ClientError> {
        self.get("summary", None, None).await
    }
    pub async fn bills(&self, query: &ListQuery) -> Result<Page<Bill>, ClientError> {
        self.get("bills", None, Some(query)).await
    }
    pub async fn bill(&self, id: &str) -> Result<Bill, ClientError> {
        self.get("bills", Some(id), None).await
    }
    pub async fn balances(&self, query: &ListQuery) -> Result<Page<Balance>, ClientError> {
        self.get("balances", None, Some(query)).await
    }
    pub async fn collections(&self, query: &ListQuery) -> Result<Page<Collection>, ClientError> {
        self.get("collections", None, Some(query)).await
    }
    pub async fn usage(&self, query: &ListQuery) -> Result<Page<Usage>, ClientError> {
        self.get("usage", None, Some(query)).await
    }
    pub async fn sync_request(&self, input: &SyncInput) -> Result<SyncJob, ClientError> {
        self.request(
            Method::POST,
            self.endpoint("sync", None, None)?,
            Some(serde_json::to_vec(input).map_err(|_| ClientError::new("invalid_request", None))?),
        )
        .await
    }
    pub async fn sync_get(&self, id: &str) -> Result<SyncJob, ClientError> {
        self.get("sync", Some(id), None).await
    }
}
