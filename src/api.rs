//! Dropshot owns routing, typed request validation, and generated OpenAPI.
use crate::{
    config::{Config, valid_id},
    models::*,
    store::{Store, StoreError},
    sync::{Providers, SyncHandle},
};
use dropshot::{
    ApiDescription, Body, ConfigDropshot, HttpError, HttpResponseAccepted, HttpResponseHeaders,
    HttpResponseOk, HttpServer, HttpServerStarter, Path, Query, RequestContext, TypedBody,
    endpoint,
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
    owner_digest: [u8; 32],
    ingress_peer: Option<IpAddr>,
}
impl Context {
    pub fn new(
        store: Store,
        config: Arc<Config>,
        provider_registry: Arc<Providers>,
        sync: SyncHandle,
        token: &str,
    ) -> Self {
        Self {
            store,
            config,
            providers: provider_registry,
            sync,
            owner_digest: Sha256::digest(token).into(),
            ingress_peer: None,
        }
    }
    pub fn for_ingress(mut self, peer: IpAddr) -> Self {
        self.ingress_peer = Some(peer);
        self
    }
    fn accounts(&self) -> Vec<String> {
        self.config.accounts.iter().map(|a| a.id.clone()).collect()
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
        read_only_providers: true,
    }))
}
#[endpoint {method=GET,path="/health",tags=["system"]}]
async fn health(request: RequestContext<Arc<Context>>) -> Result<PrivateOk<Health>, HttpError> {
    authorized(&request)?;
    let snapshot = request
        .context()
        .store
        .summary(&request.context().accounts())
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
    Ok(private(Page {
        items: request.context().providers.info(),
        next_page: None,
        coverage: request
            .context()
            .store
            .coverage()
            .await
            .map_err(store_error)?
            .into_iter()
            .filter(|item| request.context().accounts().contains(&item.account_id))
            .collect(),
    }))
}
#[endpoint {method=GET,path="/v1/accounts",tags=["utilities"]}]
async fn accounts(
    request: RequestContext<Arc<Context>>,
) -> Result<PrivateOk<Page<Account>>, HttpError> {
    authorized(&request)?;
    Ok(private(Page {
        items: request.context().config.public_accounts(),
        next_page: None,
        coverage: request
            .context()
            .store
            .coverage()
            .await
            .map_err(store_error)?
            .into_iter()
            .filter(|item| request.context().accounts().contains(&item.account_id))
            .collect(),
    }))
}
#[endpoint {method=GET,path="/v1/summary",tags=["utilities"]}]
async fn summary(request: RequestContext<Arc<Context>>) -> Result<PrivateOk<Summary>, HttpError> {
    authorized(&request)?;
    Ok(private(
        request
            .context()
            .store
            .summary(&request.context().accounts())
            .await
            .map_err(store_error)?,
    ))
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
            .list("bill", &query.into_inner(), &request.context().accounts())
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
                &request.context().accounts(),
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
                &request.context().accounts(),
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
            .list("usage", &query.into_inner(), &request.context().accounts())
            .await
            .map_err(store_error)?,
    ))
}
#[derive(Deserialize, JsonSchema)]
struct RecordPath {
    id: String,
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
            .bill(&id, &request.context().accounts())
            .await
            .map_err(store_error)?,
    ))
}
#[endpoint {method=POST,path="/v1/sync",tags=["sync"]}]
async fn sync_request(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<SyncInput>,
) -> Result<HttpResponseHeaders<HttpResponseAccepted<SyncJob>, PrivateHeaders>, HttpError> {
    authorized(&request)?;
    Ok(HttpResponseHeaders::new(
        HttpResponseAccepted(
            request
                .context()
                .sync
                .request(body.into_inner())
                .await
                .map_err(store_error)?,
        ),
        PrivateHeaders::new(),
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
    if !request.context().accounts().contains(&job.account_id) {
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
    let endpoints: [dropshot::ApiEndpoint<Arc<Context>>; 18] = [
        system.into(),
        health.into(),
        providers.into(),
        accounts.into(),
        summary.into(),
        bills.into(),
        bill_get.into(),
        balances.into(),
        collections.into(),
        usage.into(),
        sync_request.into(),
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
