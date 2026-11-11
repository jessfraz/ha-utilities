//! Native and HA transports share the same typed local read model.
use crate::imports::{
    ImportReceipt, ImportRequest, MAX_DOCUMENT_BYTES, MAX_IMPORT_REQUEST_BYTES, valid_digest,
};
use crate::models::*;
use reqwest::{
    Client as HttpClient, Method, Response, StatusCode,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}
impl ClientError {
    pub fn new(error: &'static str, status: Option<u16>) -> Self {
        Self {
            error,
            message: error.replace('_', " "),
            status,
            request_id: None,
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
            if let Some(provider) = query.import_provider {
                let value = serde_json::to_value(provider)
                    .map_err(|_| ClientError::new("invalid_request", None))?;
                let value = value
                    .as_str()
                    .ok_or_else(|| ClientError::new("invalid_request", None))?;
                pairs.append_pair("import_provider", value);
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
    async fn send(
        &self,
        method: Method,
        url: Url,
        body: Option<(&'static str, Vec<u8>)>,
    ) -> Result<Response, ClientError> {
        let conflict = match url.path_segments().and_then(|mut segments| {
            segments.nth(match self.transport {
                Transport::Native => 1,
                Transport::Ha => 3,
            })
        }) {
            Some("connections") => "connection_changed",
            Some("imports" | "documents") => "import_conflict",
            _ => "sync_busy",
        };
        let mut builder = self
            .http
            .request(method, url)
            .header(AUTHORIZATION, self.authorization.clone())
            .header("Accept", "application/json");
        if let Some((content_type, body)) = body {
            builder = builder.header("Content-Type", content_type).body(body);
        }
        let response = builder
            .send()
            .await
            .map_err(|_| ClientError::new("service_unavailable", None))?;
        let status = response.status();
        if !status.is_success() {
            return Err(ClientError::new(
                match status.as_u16() {
                    400 | 422 => "invalid_request",
                    401 => "authentication_required",
                    403 => "access_denied",
                    404 => "not_found",
                    409 => conflict,
                    413 => "request_too_large",
                    415 => "unsupported_content_type",
                    429 => "rate_limited",
                    _ => "service_error",
                },
                Some(status.as_u16()),
            ));
        }
        Ok(response)
    }
    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        url: Url,
        body: Option<Vec<u8>>,
    ) -> Result<T, ClientError> {
        let response = self
            .send(method, url, body.map(|body| ("application/json", body)))
            .await?;
        Self::response_json(response).await
    }
    async fn response_json<T: DeserializeOwned>(mut response: Response) -> Result<T, ClientError> {
        let status = response.status();
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
        self.submit_sync(input).await
    }
    pub async fn sync_history(&self, account: String) -> Result<SyncJob, ClientError> {
        self.submit_sync(&HistorySyncInput {
            account,
            range: HistoryRange::All,
        })
        .await
    }
    async fn submit_sync(&self, input: &impl Serialize) -> Result<SyncJob, ClientError> {
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
    pub async fn sync_list(&self) -> Result<Vec<SyncJob>, ClientError> {
        self.get("sync", None, None).await
    }

    pub async fn upload_document(&self, sha256: &str, bytes: Vec<u8>) -> Result<(), ClientError> {
        if !valid_digest(sha256) || bytes.is_empty() {
            return Err(ClientError::new("invalid_document", None));
        }
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(ClientError::new("request_too_large", None));
        }
        let response = self
            .send(
                Method::PUT,
                self.endpoint("documents", Some(sha256), None)?,
                Some(("application/octet-stream", bytes)),
            )
            .await?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(ClientError::new(
                "invalid_response",
                Some(response.status().as_u16()),
            ));
        }
        Ok(())
    }

    pub async fn import(&self, input: &ImportRequest) -> Result<ImportReceipt, ClientError> {
        input
            .validate()
            .map_err(|_| ClientError::new("invalid_import", None))?;
        let body =
            serde_json::to_vec(input).map_err(|_| ClientError::new("invalid_import", None))?;
        if body.len() > MAX_IMPORT_REQUEST_BYTES {
            return Err(ClientError::new("request_too_large", None));
        }
        let response = self
            .send(
                Method::POST,
                self.endpoint("imports", None, None)?,
                Some(("application/json", body)),
            )
            .await?;
        if response.status() != StatusCode::OK {
            return Err(ClientError::new(
                "invalid_response",
                Some(response.status().as_u16()),
            ));
        }
        Self::response_json(response).await
    }

    pub async fn connection_utilities(&self) -> Result<Page<ConnectableUtility>, ClientError> {
        let mut url = self.endpoint("providers", Some("bayou"), None)?;
        url.path_segments_mut()
            .map_err(|_| ClientError::new("invalid_url", None))?
            .push("utilities");
        self.request(Method::GET, url, None).await
    }

    pub async fn connections(&self) -> Result<Page<Connection>, ClientError> {
        self.get("connections", None, None).await
    }

    fn connection_endpoint(&self, id: &str, action: Option<&str>) -> Result<Url, ClientError> {
        let id = uuid::Uuid::parse_str(id)
            .map_err(|_| ClientError::new("invalid_connection_id", None))?;
        let mut url = self.endpoint("connections", Some(&id.to_string()), None)?;
        if let Some(action) = action {
            url.path_segments_mut()
                .map_err(|_| ClientError::new("invalid_url", None))?
                .push(action);
        }
        Ok(url)
    }

    pub async fn connection(&self, id: &str) -> Result<Connection, ClientError> {
        self.request(Method::GET, self.connection_endpoint(id, None)?, None)
            .await
    }

    pub async fn connection_start(
        &self,
        input: &CreateConnection,
    ) -> Result<Connection, ClientError> {
        let result = async {
            if !self.system().await?.hosted_connections {
                return Err(ClientError::new(
                    "backend_does_not_support_connections",
                    None,
                ));
            }
            self.request(
                Method::POST,
                self.endpoint("connections", None, None)?,
                Some(
                    serde_json::to_vec(input)
                        .map_err(|_| ClientError::new("invalid_request", None))?,
                ),
            )
            .await
        }
        .await;
        result.map_err(|mut error| {
            error.request_id = uuid::Uuid::parse_str(&input.request_id)
                .ok()
                .map(|id| id.to_string());
            error
        })
    }

    async fn connection_action(&self, id: &str, action: &str) -> Result<Connection, ClientError> {
        self.request(
            Method::POST,
            self.connection_endpoint(id, Some(action))?,
            Some(
                serde_json::to_vec(&ConnectionAction::default())
                    .map_err(|_| ClientError::new("invalid_request", None))?,
            ),
        )
        .await
    }

    pub async fn connection_refresh(&self, id: &str) -> Result<Connection, ClientError> {
        self.connection_action(id, "refresh").await
    }

    pub async fn connection_resume(&self, id: &str) -> Result<Connection, ClientError> {
        self.connection_action(id, "resume").await
    }

    pub async fn connection_authorization(
        &self,
        id: &str,
    ) -> Result<ConnectionAuthorization, ClientError> {
        self.request(
            Method::GET,
            self.connection_endpoint(id, Some("authorization"))?,
            None,
        )
        .await
    }

    pub async fn connection_bind(
        &self,
        id: &str,
        input: &BindConnection,
    ) -> Result<Connection, ClientError> {
        self.request(
            Method::POST,
            self.connection_endpoint(id, Some("bind"))?,
            Some(serde_json::to_vec(input).map_err(|_| ClientError::new("invalid_request", None))?),
        )
        .await
    }
}
