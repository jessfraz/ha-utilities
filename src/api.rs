//! Dropshot owns routing, typed request validation, and generated OpenAPI.
use crate::{
    config::{Config, valid_id},
    connect::{ConnectionError, ConnectionHandle},
    imports::{
        BillDetails, ImportError, ImportReceipt, ImportRequest, MAX_DOCUMENT_BYTES,
        MAX_IMPORT_REQUEST_BYTES,
    },
    models::*,
    store::{Store, StoreError},
    sync::{Providers, SyncHandle},
};
use dropshot::{
    ApiDescription, Body, ConfigDropshot, HttpError, HttpResponseAccepted, HttpResponseHeaders,
    HttpResponseOk, HttpResponseUpdatedNoContent, HttpServer, HttpServerStarter, Path, Query,
    RequestContext, TypedBody, UntypedBody, endpoint,
};
use http::{HeaderValue, Response};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use subtle::ConstantTimeEq;

#[derive(Clone)]
pub struct Context {
    pub store: Store,
    pub config: Arc<Config>,
    pub providers: Arc<Providers>,
    pub sync: SyncHandle,
    pub connections: ConnectionHandle,
    owner_digest: [u8; 32],
    ingress_peer: Option<IpAddr>,
}
impl Context {
    pub fn new(
        store: Store,
        config: Arc<Config>,
        provider_registry: Arc<Providers>,
        sync: SyncHandle,
        connection_handle: ConnectionHandle,
        token: &str,
    ) -> Self {
        Self {
            store,
            config,
            providers: provider_registry,
            sync,
            connections: connection_handle,
            owner_digest: Sha256::digest(token).into(),
            ingress_peer: None,
        }
    }
    pub fn for_ingress(mut self, peer: IpAddr) -> Self {
        self.ingress_peer = Some(peer);
        self
    }
    async fn accounts(&self) -> Result<Vec<String>, HttpError> {
        Ok(self
            .store
            .bindings(&self.config.deployment)
            .await
            .map_err(store_error)?
            .into_iter()
            .map(|account| account.id)
            .collect())
    }
}

#[derive(Serialize, JsonSchema)]
struct PrivateHeaders {
    #[serde(rename = "cache-control")]
    cache_control: String,
}
impl PrivateHeaders {
    fn new() -> Self {
        Self {
            cache_control: "no-store".to_owned(),
        }
    }
}
type PrivateOk<T> = HttpResponseHeaders<HttpResponseOk<T>, PrivateHeaders>;
fn private<T: Serialize + JsonSchema + Send + Sync + 'static>(value: T) -> PrivateOk<T> {
    HttpResponseHeaders::new(HttpResponseOk(value), PrivateHeaders::new())
}

fn fail(status: dropshot::ClientErrorStatusCode, code: &str) -> HttpError {
    HttpError::for_client_error(Some(code.to_owned()), status, code.to_owned())
}
fn store_error(error: StoreError) -> HttpError {
    use dropshot::ClientErrorStatusCode as S;
    match error {
        StoreError::NotFound => fail(S::NOT_FOUND, "not_found"),
        StoreError::InvalidQuery => fail(S::BAD_REQUEST, "invalid_query"),
        StoreError::Busy => fail(S::CONFLICT, "sync_busy"),
        _ => HttpError::for_internal_error("database_error".to_owned()),
    }
}

fn import_error(error: ImportError) -> HttpError {
    use dropshot::ClientErrorStatusCode as S;
    match error {
        ImportError::Invalid => fail(S::BAD_REQUEST, "invalid_import"),
        ImportError::Binding => fail(S::CONFLICT, "import_binding_conflict"),
        ImportError::MissingDocument => fail(S::BAD_REQUEST, "missing_source_document"),
        ImportError::Conflict => fail(S::CONFLICT, "import_record_conflict"),
        ImportError::Store(error) => store_error(error),
    }
}

fn connection_error(error: ConnectionError) -> HttpError {
    use dropshot::ClientErrorStatusCode as S;
    match error {
        ConnectionError::InvalidInput => fail(S::BAD_REQUEST, "invalid_connection_input"),
        ConnectionError::ConsentRequired => {
            fail(S::BAD_REQUEST, "provider_settings_consent_required")
        }
        ConnectionError::NotConfigured
        | ConnectionError::Provider(crate::utility::UtilityError::NotConfigured) => {
            HttpError::for_unavail(
                Some("bayou_not_configured".to_owned()),
                "bayou_not_configured".to_owned(),
            )
        }
        ConnectionError::WorkerUnavailable => HttpError::for_unavail(
            Some("connection_worker_unavailable".to_owned()),
            "connection_worker_unavailable".to_owned(),
        ),
        ConnectionError::Changed
        | ConnectionError::Store(StoreError::BindingChanged | StoreError::Busy) => {
            fail(S::CONFLICT, "connection_changed")
        }
        ConnectionError::NotReady => fail(S::CONFLICT, "connection_not_ready"),
        ConnectionError::AuthorizationLinkUnavailable => HttpError::for_unavail(
            Some("bayou_authorization_link_unavailable".to_owned()),
            "bayou_authorization_link_unavailable".to_owned(),
        ),
        ConnectionError::Store(StoreError::NotFound) => fail(S::NOT_FOUND, "connection_not_found"),
        ConnectionError::Store(StoreError::InvalidQuery) => {
            fail(S::BAD_REQUEST, "invalid_connection_input")
        }
        ConnectionError::Store(_) => HttpError::for_internal_error("database_error".to_owned()),
        ConnectionError::Provider(error) => {
            let code = match error {
                crate::utility::UtilityError::Authentication => "bayou_authentication_required",
                crate::utility::UtilityError::AccessDenied => "bayou_access_denied",
                crate::utility::UtilityError::RateLimited => "bayou_rate_limited",
                crate::utility::UtilityError::Unavailable => "bayou_unavailable",
                _ => "bayou_invalid_response",
            };
            HttpError::for_unavail(Some(code.to_owned()), code.to_owned())
        }
    }
}
fn header<'a>(request: &'a RequestContext<Arc<Context>>, key: &str) -> Option<&'a str> {
    request
        .request
        .headers()
        .get(key)
        .and_then(|v| v.to_str().ok())
}
fn ingress(request: &RequestContext<Arc<Context>>) -> Result<(), HttpError> {
    if let Some(peer) = request.context().ingress_peer {
        if request.request.remote_addr().ip() != peer {
            return Err(fail(
                dropshot::ClientErrorStatusCode::FORBIDDEN,
                "invalid_ingress_peer",
            ));
        }
        if !header(request, "x-remote-user-id")
            .is_some_and(|s| !s.is_empty() && s.len() <= 200 && !s.chars().any(char::is_control))
        {
            return Err(fail(
                dropshot::ClientErrorStatusCode::UNAUTHORIZED,
                "ha_user_required",
            ));
        }
    }
    Ok(())
}
fn authorized(request: &RequestContext<Arc<Context>>) -> Result<(), HttpError> {
    use dropshot::ClientErrorStatusCode as S;
    ingress(request)?;
    let bearer = header(request, "authorization").and_then(|v| v.strip_prefix("Bearer "));
    if request.context().ingress_peer.is_none() {
        let cookie = header(request, "cookie").and_then(|cookies| {
            cookies
                .split(';')
                .find_map(|p| p.trim().strip_prefix("utilities_session="))
        });
        let token = bearer
            .or(cookie)
            .ok_or_else(|| fail(S::UNAUTHORIZED, "authentication_required"))?;
        let digest: [u8; 32] = Sha256::digest(token).into();
        if !bool::from(digest.ct_eq(&request.context().owner_digest)) {
            return Err(fail(S::UNAUTHORIZED, "invalid_credential"));
        }
    }
    // Bearer clients do not use ambient cookies. Browser writes need a custom
    // same-origin header, including authenticated Supervisor ingress requests.
    if request.request.method() != http::Method::GET
        && bearer.is_none()
        && header(request, "x-utilities-request") != Some("1")
    {
        return Err(fail(S::FORBIDDEN, "browser_request_header_required"));
    }
    Ok(())
}

#[endpoint {method=GET,path="/v1/system",tags=["system"]}]
async fn system(request: RequestContext<Arc<Context>>) -> Result<PrivateOk<System>, HttpError> {
    authorized(&request)?;
    Ok(private(System {
        name: "Utilities".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        read_only_providers: false,
        read_only_sync: true,
        hosted_connections: true,
    }))
}
#[endpoint {method=GET,path="/health",tags=["system"]}]
async fn health(request: RequestContext<Arc<Context>>) -> Result<PrivateOk<Health>, HttpError> {
    authorized(&request)?;
    let snapshot = request
        .context()
        .store
        .summary(&request.context().accounts().await?)
        .await
        .map_err(store_error)?;
    let status = if snapshot.accounts == 0 {
        "not_configured"
    } else if snapshot.stale
        || snapshot.coverage.iter().any(|c| {
            matches!(
                c.state,
                CoverageState::Unavailable | CoverageState::NotConfigured
            )
        })
    {
        "degraded"
    } else {
        "ok"
    };
    Ok(private(Health {
        status: status.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        last_synced_at: snapshot.last_synced_at,
        stale: snapshot.stale,
        coverage: snapshot.coverage,
    }))
}
#[endpoint {method=GET,path="/v1/providers",tags=["utilities"]}]
async fn providers(
    request: RequestContext<Arc<Context>>,
) -> Result<PrivateOk<Page<ProviderInfo>>, HttpError> {
    authorized(&request)?;
    let context = request.context();
    let bindings = context
        .store
        .bindings(&context.config.deployment)
        .await
        .map_err(store_error)?;
    let allowed: Vec<_> = bindings.iter().map(|account| account.id.clone()).collect();
    let mut providers = context.providers.info();
    for binding in &bindings {
        if !providers.iter().any(|item| item.id == binding.provider) {
            providers.push(ProviderInfo {
                id: binding.provider,
                name: binding.provider.name().to_owned(),
                capabilities: vec![
                    Capability::Bills,
                    Capability::Payments,
                    Capability::Balances,
                    Capability::Usage,
                ],
                configured: false,
            });
        }
    }
    Ok(private(Page {
        items: providers,
        next_page: None,
        coverage: request
            .context()
            .store
            .coverage()
            .await
            .map_err(store_error)?
            .into_iter()
            .filter(|item| allowed.contains(&item.account_id))
            .collect(),
    }))
}
#[endpoint {method=GET,path="/v1/accounts",tags=["utilities"]}]
async fn accounts(
    request: RequestContext<Arc<Context>>,
) -> Result<PrivateOk<Page<Account>>, HttpError> {
    authorized(&request)?;
    let mut current = request.context().config.as_ref().clone();
    current.accounts = request
        .context()
        .store
        .bindings(&current.deployment)
        .await
        .map_err(store_error)?;
    let allowed = current
        .accounts
        .iter()
        .map(|account| account.id.clone())
        .collect::<Vec<_>>();
    let import_states = request
        .context()
        .store
        .import_states(&allowed)
        .await
        .map_err(store_error)?;
    let mut items = current.public_accounts();
    for item in &mut items {
        item.import_only = import_states
            .iter()
            .any(|state| state.id == item.id && state.import_only);
    }
    Ok(private(Page {
        items,
        next_page: None,
        coverage: request
            .context()
            .store
            .coverage()
            .await
            .map_err(store_error)?
            .into_iter()
            .filter(|item| allowed.contains(&item.account_id))
            .collect(),
    }))
}

#[endpoint {method=GET,path="/v1/providers/bayou/utilities",tags=["connections"]}]
async fn connection_utilities(
    request: RequestContext<Arc<Context>>,
) -> Result<PrivateOk<Page<ConnectableUtility>>, HttpError> {
    authorized(&request)?;
    Ok(private(Page {
        items: request
            .context()
            .connections
            .utilities()
            .await
            .map_err(connection_error)?,
        next_page: None,
        coverage: Vec::new(),
    }))
}

#[endpoint {method=GET,path="/v1/connections",tags=["connections"]}]
async fn connections(
    request: RequestContext<Arc<Context>>,
) -> Result<PrivateOk<Page<Connection>>, HttpError> {
    authorized(&request)?;
    Ok(private(Page {
        items: request
            .context()
            .connections
            .list()
            .await
            .map_err(connection_error)?,
        next_page: None,
        coverage: Vec::new(),
    }))
}

#[endpoint {method=POST,path="/v1/connections",tags=["connections"]}]
async fn connection_start(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<CreateConnection>,
) -> Result<HttpResponseHeaders<HttpResponseAccepted<Connection>, PrivateHeaders>, HttpError> {
    authorized(&request)?;
    Ok(HttpResponseHeaders::new(
        HttpResponseAccepted(
            request
                .context()
                .connections
                .start(body.into_inner())
                .await
                .map_err(connection_error)?,
        ),
        PrivateHeaders::new(),
    ))
}

#[endpoint {method=GET,path="/v1/connections/{id}",tags=["connections"]}]
async fn connection_get(
    request: RequestContext<Arc<Context>>,
    path: Path<RecordPath>,
) -> Result<PrivateOk<Connection>, HttpError> {
    authorized(&request)?;
    Ok(private(
        request
            .context()
            .connections
            .get(&path.into_inner().id)
            .await
            .map_err(connection_error)?,
    ))
}

#[endpoint {method=POST,path="/v1/connections/{id}/refresh",tags=["connections"]}]
async fn connection_refresh(
    request: RequestContext<Arc<Context>>,
    path: Path<RecordPath>,
    _body: TypedBody<ConnectionAction>,
) -> Result<PrivateOk<Connection>, HttpError> {
    authorized(&request)?;
    Ok(private(
        request
            .context()
            .connections
            .refresh(&path.into_inner().id)
            .await
            .map_err(connection_error)?,
    ))
}

#[endpoint {method=POST,path="/v1/connections/{id}/resume",tags=["connections"]}]
async fn connection_resume(
    request: RequestContext<Arc<Context>>,
    path: Path<RecordPath>,
    _body: TypedBody<ConnectionAction>,
) -> Result<PrivateOk<Connection>, HttpError> {
    authorized(&request)?;
    Ok(private(
        request
            .context()
            .connections
            .resume(&path.into_inner().id)
            .await
            .map_err(connection_error)?,
    ))
}

#[endpoint {method=GET,path="/v1/connections/{id}/authorization",tags=["connections"]}]
async fn connection_authorization(
    request: RequestContext<Arc<Context>>,
    path: Path<RecordPath>,
) -> Result<PrivateOk<ConnectionAuthorization>, HttpError> {
    authorized(&request)?;
    Ok(private(
        request
            .context()
            .connections
            .authorization(&path.into_inner().id)
            .await
            .map_err(connection_error)?,
    ))
}

#[endpoint {method=POST,path="/v1/connections/{id}/bind",tags=["connections"]}]
async fn connection_bind(
    request: RequestContext<Arc<Context>>,
    path: Path<RecordPath>,
    body: TypedBody<BindConnection>,
) -> Result<PrivateOk<Connection>, HttpError> {
    authorized(&request)?;
    let (connection, jobs) = request
        .context()
        .connections
        .bind(&path.into_inner().id, body.into_inner())
        .await
        .map_err(connection_error)?;
    if request
        .context()
        .sync
        .enqueue_first_syncs(jobs)
        .await
        .is_err()
    {
        slog::warn!(request.log, "Could not record first sync queue failure");
    }
    Ok(private(connection))
}
#[endpoint {method=GET,path="/v1/summary",tags=["utilities"]}]
async fn summary(request: RequestContext<Arc<Context>>) -> Result<PrivateOk<Summary>, HttpError> {
    authorized(&request)?;
    let context = request.context();
    let bindings = context
        .store
        .bindings(&context.config.deployment)
        .await
        .map_err(store_error)?;
    let (ids, unconfigured): (Vec<_>, Vec<_>) = (
        bindings.iter().map(|account| account.id.clone()).collect(),
        bindings
            .iter()
            .filter(|account| !context.providers.configured(account.provider))
            .map(|account| account.id.clone())
            .collect(),
    );
    let mut summary = context.store.summary(&ids).await.map_err(store_error)?;
    let cutoff = chrono::Utc::now() - chrono::Duration::days(40);
    summary.stale_automatic_accounts = context
        .store
        .stale_automatic_accounts(&context.config.deployment, cutoff)
        .await
        .map_err(store_error)?
        .into_iter()
        .filter(|(provider, _)| context.providers.configured(*provider))
        .map(|(_, item)| item)
        .collect();
    let manual = if unconfigured.is_empty() {
        Vec::new()
    } else {
        context
            .store
            .import_states(&unconfigured)
            .await
            .map_err(store_error)?
            .into_iter()
            .filter_map(|account| (!account.import_only).then_some(account.id))
            .collect::<Vec<_>>()
    };
    summary.manual_accounts_stale = !manual.is_empty()
        && context
            .store
            .freshness(&manual)
            .await
            .map_err(store_error)?
            .1;
    Ok(private(summary))
}
#[endpoint {method=GET,path="/v1/bills",tags=["utilities"]}]
async fn bills(
    request: RequestContext<Arc<Context>>,
    query: Query<ListQuery>,
) -> Result<PrivateOk<Page<Bill>>, HttpError> {
    authorized(&request)?;
    Ok(private(
        request
            .context()
            .store
            .list(
                "bill",
                &query.into_inner(),
                &request.context().accounts().await?,
            )
            .await
            .map_err(store_error)?,
    ))
}
#[endpoint {method=GET,path="/v1/balances",tags=["utilities"]}]
async fn balances(
    request: RequestContext<Arc<Context>>,
    query: Query<ListQuery>,
) -> Result<PrivateOk<Page<Balance>>, HttpError> {
    authorized(&request)?;
    Ok(private(
        request
            .context()
            .store
            .list(
                "balance",
                &query.into_inner(),
                &request.context().accounts().await?,
            )
            .await
            .map_err(store_error)?,
    ))
}
#[endpoint {method=GET,path="/v1/collections",tags=["utilities"]}]
async fn collections(
    request: RequestContext<Arc<Context>>,
    query: Query<ListQuery>,
) -> Result<PrivateOk<Page<Collection>>, HttpError> {
    authorized(&request)?;
    Ok(private(
        request
            .context()
            .store
            .list(
                "collection",
                &query.into_inner(),
                &request.context().accounts().await?,
            )
            .await
            .map_err(store_error)?,
    ))
}
#[endpoint {method=GET,path="/v1/usage",tags=["utilities"]}]
async fn usage(
    request: RequestContext<Arc<Context>>,
    query: Query<ListQuery>,
) -> Result<PrivateOk<Page<Usage>>, HttpError> {
    authorized(&request)?;
    Ok(private(
        request
            .context()
            .store
            .list(
                "usage",
                &query.into_inner(),
                &request.context().accounts().await?,
            )
            .await
            .map_err(store_error)?,
    ))
}
#[derive(Deserialize, JsonSchema)]
struct RecordPath {
    id: String,
}

#[derive(Deserialize, JsonSchema)]
struct DocumentPath {
    sha256: String,
}

#[endpoint {method=PUT,path="/v1/documents/{sha256}",tags=["imports"],request_body_max_bytes=MAX_DOCUMENT_BYTES}]
async fn document_put(
    request: RequestContext<Arc<Context>>,
    path: Path<DocumentPath>,
    body: UntypedBody,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    authorized(&request)?;
    if header(&request, "content-type") != Some("application/octet-stream") {
        return Err(fail(
            dropshot::ClientErrorStatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_document_content_type",
        ));
    }
    let context = request.context();
    context
        .store
        .put_document(
            &context.config.deployment,
            &path.into_inner().sha256,
            body.as_bytes(),
        )
        .await
        .map_err(import_error)?;
    Ok(HttpResponseUpdatedNoContent())
}

#[endpoint {method=GET,path="/v1/documents/{sha256}",tags=["imports"]}]
async fn document_get(
    request: RequestContext<Arc<Context>>,
    path: Path<DocumentPath>,
) -> Result<Response<Body>, HttpError> {
    authorized(&request)?;
    let context = request.context();
    let sha256 = path.into_inner().sha256;
    let bytes = context
        .store
        .document(&context.config.deployment, &sha256)
        .await
        .map_err(store_error)?;
    let extension = if bytes.starts_with(b"%PDF-") {
        "pdf"
    } else {
        "txt"
    };
    Response::builder()
        .header("content-type", "application/octet-stream")
        .header(
            "content-disposition",
            format!("attachment; filename=\"source-{sha256}.{extension}\""),
        )
        .header("x-content-type-options", "nosniff")
        .header("cache-control", "no-store")
        .body(Body::from(bytes))
        .map_err(|_| HttpError::for_internal_error("response_error".to_owned()))
}

#[endpoint {method=POST,path="/v1/imports",tags=["imports"],request_body_max_bytes=MAX_IMPORT_REQUEST_BYTES}]
async fn import_records(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<ImportRequest>,
) -> Result<PrivateOk<ImportReceipt>, HttpError> {
    authorized(&request)?;
    let context = request.context();
    Ok(private(
        context
            .store
            .import(&context.config.deployment, body.into_inner())
            .await
            .map_err(import_error)?,
    ))
}

#[endpoint {method=GET,path="/v1/bills/{id}/details",tags=["utilities"]}]
async fn bill_details(
    request: RequestContext<Arc<Context>>,
    path: Path<RecordPath>,
) -> Result<PrivateOk<BillDetails>, HttpError> {
    authorized(&request)?;
    let id = path.into_inner().id;
    if id.is_empty() || id.len() > 500 {
        return Err(fail(
            dropshot::ClientErrorStatusCode::BAD_REQUEST,
            "invalid_id",
        ));
    }
    let context = request.context();
    Ok(private(
        context
            .store
            .bill_details(&context.config.deployment, &id)
            .await
            .map_err(store_error)?,
    ))
}
#[endpoint {method=GET,path="/v1/bills/{id}",tags=["utilities"]}]
async fn bill_get(
    request: RequestContext<Arc<Context>>,
    path: Path<RecordPath>,
) -> Result<PrivateOk<Bill>, HttpError> {
    authorized(&request)?;
    let id = path.into_inner().id;
    if id.len() > 500 || id.is_empty() {
        return Err(fail(
            dropshot::ClientErrorStatusCode::BAD_REQUEST,
            "invalid_id",
        ));
    }
    Ok(private(
        request
            .context()
            .store
            .bill(&id, &request.context().accounts().await?)
            .await
            .map_err(store_error)?,
    ))
}
#[endpoint {method=POST,path="/v1/sync",tags=["sync"]}]
async fn sync_request(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<SyncSelection>,
) -> Result<HttpResponseHeaders<HttpResponseAccepted<SyncJob>, PrivateHeaders>, HttpError> {
    authorized(&request)?;
    let sync = &request.context().sync;
    let job = match body.into_inner() {
        SyncSelection::Dates(input) => sync.request(input).await,
        SyncSelection::History(input) => sync.request_history(input).await,
    }
    .map_err(store_error)?;
    Ok(HttpResponseHeaders::new(
        HttpResponseAccepted(job),
        PrivateHeaders::new(),
    ))
}
#[endpoint {method=GET,path="/v1/sync",tags=["sync"]}]
async fn sync_latest(
    request: RequestContext<Arc<Context>>,
) -> Result<PrivateOk<Vec<SyncJob>>, HttpError> {
    authorized(&request)?;
    Ok(private(
        request
            .context()
            .store
            .latest_jobs(&request.context().config.deployment)
            .await
            .map_err(store_error)?,
    ))
}
#[endpoint {method=GET,path="/v1/sync/{id}",tags=["sync"]}]
async fn sync_get(
    request: RequestContext<Arc<Context>>,
    path: Path<RecordPath>,
) -> Result<PrivateOk<SyncJob>, HttpError> {
    authorized(&request)?;
    let id = path.into_inner().id;
    if !valid_id(&id) {
        return Err(fail(
            dropshot::ClientErrorStatusCode::BAD_REQUEST,
            "invalid_id",
        ));
    }
    let job = request
        .context()
        .store
        .job(&id)
        .await
        .map_err(store_error)?;
    if !request
        .context()
        .accounts()
        .await?
        .contains(&job.account_id)
    {
        return Err(fail(
            dropshot::ClientErrorStatusCode::NOT_FOUND,
            "not_found",
        ));
    }
    Ok(private(job))
}

fn static_response(
    content: impl Into<Body>,
    content_type: &str,
) -> Result<Response<Body>, HttpError> {
    Response::builder().header("content-type",content_type).header("cache-control","no-store").header("x-content-type-options","nosniff").header("referrer-policy","no-referrer").header("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'self'; form-action 'self'; frame-ancestors 'self'").body(content.into()).map_err(|_|HttpError::for_internal_error("response_error".to_owned()))
}
#[endpoint {method=GET,path="/",tags=["web"]}]
async fn index(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress(&request)?;
    if authorized(&request).is_err() {
        return Response::builder()
            .status(303)
            .header("location", "login")
            .header("cache-control", "no-store")
            .body(Body::empty())
            .map_err(|_| HttpError::for_internal_error("response_error".to_owned()));
    }
    static_response(
        include_str!("../web/index.html").replace(
            "<html ",
            if request.context().ingress_peer.is_some() {
                "<html data-ingress=\"true\" "
            } else {
                "<html data-ingress=\"false\" "
            },
        ),
        "text/html; charset=utf-8",
    )
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Login {
    token: String,
}
#[endpoint {method=GET,path="/login",tags=["web"]}]
async fn login_page(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress(&request)?;
    static_response(
        include_str!("../web/login.html"),
        "text/html; charset=utf-8",
    )
}
#[endpoint {method=POST,path="/login",tags=["web"]}]
async fn login(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<Login>,
) -> Result<Response<Body>, HttpError> {
    ingress(&request)?;
    if header(&request, "x-utilities-request") != Some("1") {
        return Err(fail(
            dropshot::ClientErrorStatusCode::FORBIDDEN,
            "browser_request_header_required",
        ));
    }
    let input = body.into_inner();
    let digest: [u8; 32] = Sha256::digest(&input.token).into();
    if !bool::from(digest.ct_eq(&request.context().owner_digest)) {
        return Err(fail(
            dropshot::ClientErrorStatusCode::UNAUTHORIZED,
            "invalid_credential",
        ));
    }
    // Path-relative sessions also work behind a reverse proxy prefix.
    let cookie = HeaderValue::from_str(&format!(
        "utilities_session={}; HttpOnly; SameSite=Strict; Max-Age=28800",
        input.token
    ))
    .map_err(|_| {
        fail(
            dropshot::ClientErrorStatusCode::BAD_REQUEST,
            "invalid_credential",
        )
    })?;
    Response::builder()
        .status(204)
        .header("set-cookie", cookie)
        .header("cache-control", "no-store")
        .body(Body::empty())
        .map_err(|_| HttpError::for_internal_error("response_error".to_owned()))
}
#[derive(Deserialize, JsonSchema)]
struct AssetPath {
    name: String,
}
#[endpoint {method=GET,path="/assets/{name}",tags=["web"]}]
async fn asset(
    request: RequestContext<Arc<Context>>,
    path: Path<AssetPath>,
) -> Result<Response<Body>, HttpError> {
    ingress(&request)?;
    let (content, mime) = match path.into_inner().name.as_str() {
        "app.js" => (
            include_str!("../web/app.js"),
            "text/javascript; charset=utf-8",
        ),
        "app.css" => (include_str!("../web/app.css"), "text/css; charset=utf-8"),
        "connections.js" => (
            include_str!("../web/connections.js"),
            "text/javascript; charset=utf-8",
        ),
        "login.js" => (
            include_str!("../web/login.js"),
            "text/javascript; charset=utf-8",
        ),
        "shell.js" => (ha_app_ui::SCRIPT, "text/javascript; charset=utf-8"),
        "shell.css" => (ha_app_ui::STYLESHEET, "text/css; charset=utf-8"),
        _ => {
            return Err(fail(
                dropshot::ClientErrorStatusCode::NOT_FOUND,
                "not_found",
            ));
        }
    };
    static_response(content, mime)
}
#[endpoint {method=GET,path="/openapi.json",tags=["system"]}]
async fn schema(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress(&request)?;
    static_response(
        serde_json::to_string(
            &openapi().map_err(|_| HttpError::for_internal_error("schema_error".to_owned()))?,
        )
        .map_err(|_| HttpError::for_internal_error("schema_error".to_owned()))?,
        "application/json",
    )
}

pub fn description() -> anyhow::Result<ApiDescription<Arc<Context>>> {
    let mut api = ApiDescription::new();
    let endpoints: [dropshot::ApiEndpoint<Arc<Context>>; 31] = [
        system.into(),
        health.into(),
        providers.into(),
        accounts.into(),
        connection_utilities.into(),
        connections.into(),
        connection_start.into(),
        connection_get.into(),
        connection_refresh.into(),
        connection_resume.into(),
        connection_authorization.into(),
        connection_bind.into(),
        summary.into(),
        bills.into(),
        bill_get.into(),
        bill_details.into(),
        document_put.into(),
        document_get.into(),
        import_records.into(),
        balances.into(),
        collections.into(),
        usage.into(),
        sync_request.into(),
        sync_latest.into(),
        sync_get.into(),
        index.into(),
        asset.into(),
        login_page.into(),
        login.into(),
        schema.into(),
        logout.into(),
    ];
    for endpoint in endpoints {
        api.register(endpoint)
            .map_err(|_| anyhow::anyhow!("endpoint registration failed"))?;
    }
    Ok(api)
}
#[endpoint {method=POST,path="/logout",tags=["web"]}]
async fn logout(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    authorized(&request)?;
    Response::builder()
        .status(204)
        .header(
            "set-cookie",
            "utilities_session=; HttpOnly; SameSite=Strict; Max-Age=0",
        )
        .header("cache-control", "no-store")
        .body(Body::empty())
        .map_err(|_| HttpError::for_internal_error("response_error".to_owned()))
}
pub fn openapi() -> anyhow::Result<serde_json::Value> {
    Ok(description()?.openapi("Utilities API",semver::Version::parse(env!("CARGO_PKG_VERSION"))?).description("Local utility observations and explicit coverage. Bearer owner credential or authenticated HA bridge. Sync reads providers only; no payments or service changes.").json()?)
}
pub fn start(bind: SocketAddr, context: Arc<Context>) -> anyhow::Result<HttpServer<Arc<Context>>> {
    // Request URLs may contain local identifiers. The app emits no request/body
    // logs; provider failures use fixed codes in durable sync records instead.
    let log = slog::Logger::root(slog::Discard, slog::o!());
    HttpServerStarter::new(
        &ConfigDropshot {
            bind_address: bind,
            default_request_body_max_bytes: 16 * 1024,
            ..Default::default()
        },
        description()?,
        context,
        &log,
    )
    .map_err(|_| anyhow::anyhow!("HTTP server could not start"))
    .map(HttpServerStarter::start)
}
