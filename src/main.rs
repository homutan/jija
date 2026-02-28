use std::{
    collections::HashMap,
    fmt::{self, Display, Formatter},
    sync::Arc,
    time::Instant,
};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Request as IncomingRequest},
    middleware::{self, Next},
    response::{IntoResponse, Response as OutgoingResponse},
    routing::post,
};
use color_eyre::eyre::{self, Context};
use reqwest::{
    Client as ReqwestClient, RequestBuilder, Response as IncomingResponse, StatusCode, Url,
    header::AUTHORIZATION,
};
use secrecy::{ExposeSecret as _, SecretString};
use tokio::{net::TcpListener, signal};
use tracing::Instrument as _;
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    EnvFilter, Layer as _, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

const PROXY_ADDRESS: &str = "PROXY_ADDRESS";
const PROXY_AUTH_KEY: &str = "PROXY_AUTH_KEY";
const PROXY_USER_AGENT: &str = "PROXY_USER_AGENT";

const PRETTY_LOGS: &str = "PRETTY_LOGS";

const ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const ANTHROPIC_AUTH_KEY: &str = "ANTHROPIC_AUTH_KEY";

const OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
const OPENAI_AUTH_KEY: &str = "OPENAI_AUTH_KEY";

#[derive(Debug, Clone)]
struct ProviderConfig {
    base_url: Url,
    auth_key: SecretString,
}

impl Display for ProviderConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{{ base_url: {}, auth_key: [REDACTED] }}", self.base_url.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct HttpClient {
    http_client: ReqwestClient,
    config: Arc<ProviderConfig>,
}

impl HttpClient {
    const ALLOWED_OUTGOING_HEADERS: [&str; 5] = [
        "content-type",
        "anthropic-version",
        "anthropic-beta",
        "openai-beta",
        "openai-organization",
    ];

    pub fn post(
        &self, path: &str, query: Option<&str>, body: Bytes, headers: &HeaderMap,
    ) -> eyre::Result<RequestBuilder> {
        let mut url = self.config.base_url.join(path)?;
        url.set_query(query);

        let mut request = self
            .http_client
            .post(url)
            .header("x-api-key", self.config.auth_key.expose_secret())
            .body(body);

        for (name, value) in headers {
            if HttpClient::ALLOWED_OUTGOING_HEADERS.contains(&name.as_str()) {
                request = request.header(name, value);
            }
        }

        Ok(request)
    }

    pub fn get(
        &self, path: &str, query: Option<&str>, headers: &HeaderMap,
    ) -> eyre::Result<RequestBuilder> {
        let mut url = self.config.base_url.join(path)?;
        url.set_query(query);

        let mut request = self
            .http_client
            .get(url)
            .bearer_auth(&self.config.auth_key.expose_secret());

        for (name, value) in headers {
            if HttpClient::ALLOWED_OUTGOING_HEADERS.contains(&name.as_str()) {
                request = request.header(name, value);
            }
        }

        Ok(request)
    }

    pub fn delete(
        &self, path: &str, query: Option<&str>, headers: &HeaderMap,
    ) -> eyre::Result<RequestBuilder> {
        let mut url = self.config.base_url.join(path)?;
        url.set_query(query);

        let mut request = self
            .http_client
            .delete(url)
            .bearer_auth(&self.config.auth_key.expose_secret());

        for (name, value) in headers {
            if HttpClient::ALLOWED_OUTGOING_HEADERS.contains(&name.as_str()) {
                request = request.header(name, value);
            }
        }

        Ok(request)
    }
}

#[derive(Debug)]
pub struct Error(eyre::Report);

impl IntoResponse for Error {
    fn into_response(self) -> OutgoingResponse {
        tracing::error!(error = ?self.0, "Handler error");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}

impl<E> From<E> for Error
where
    E: Into<eyre::Report>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

fn url_from_env(key: &str) -> eyre::Result<Url> {
    let s = {
        let mut s = std::env::var(key)?;

        if !s.ends_with("/") {
            s.push('/');
        }

        s
    };

    Ok(Url::parse(&s)?)
}

async fn log_request(request: IncomingRequest<Body>, next: Next) -> OutgoingResponse {
    let request_id: u32 = rand::random_range(10_000_000..100_000_000);

    let method = request.method().clone();
    let uri = request.uri().clone();
    let start = Instant::now();

    async move {
        tracing::info!(method = %method, uri = %uri, "Incoming");

        let response = next.run(request).await;

        let latency = start.elapsed();
        let status = response.status();

        tracing::info!(method = %method, uri = %uri, status = %status, latency_ms = latency.as_millis(), "Completed");

        response
    }
    .instrument(tracing::info_span!("request", id = request_id))
    .await
}

async fn authorize(
    request: IncomingRequest<Body>, next: Next, expected_key: SecretString,
) -> Result<OutgoingResponse, Error> {
    let auth_header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let provided_key = auth_header
        .and_then(|h| h.strip_prefix("Bearer "))
        .or_else(|| auth_header);

    match provided_key {
        Some(key) if key == expected_key.expose_secret() => Ok(next.run(request).await),
        _ => {
            tracing::warn!("Unauthorized request attempt");
            Ok(StatusCode::UNAUTHORIZED.into_response())
        }
    }
}

#[tokio::main]
async fn main() {
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    let fmt_layer = tracing_subscriber::fmt::layer().with_target(false);
    let fmt_layer = match std::env::var(PRETTY_LOGS).map(|s| s.parse()) {
        Ok(Ok(true)) => fmt_layer.pretty().boxed(),
        _ => fmt_layer.boxed(),
    };

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(ErrorLayer::default())
        .try_init()
        .unwrap();

    if let Err(err) = run().await {
        tracing::error!(error = ?err, "Application error");
    }
}

async fn run() -> eyre::Result<()> {
    color_eyre::install()?;
    let _ = dotenvy::dotenv();

    let anthropic_config = ProviderConfig {
        base_url: url_from_env(ANTHROPIC_BASE_URL).context(ANTHROPIC_BASE_URL)?,
        auth_key: std::env::var(ANTHROPIC_AUTH_KEY)
            .context(ANTHROPIC_AUTH_KEY)?
            .into(),
    };

    tracing::info!("Using Anthropic as: {anthropic_config}");

    let openai_config = ProviderConfig {
        base_url: url_from_env(OPENAI_BASE_URL).context(OPENAI_BASE_URL)?,
        auth_key: std::env::var(OPENAI_AUTH_KEY)
            .context(OPENAI_AUTH_KEY)?
            .into(),
    };

    tracing::info!("Using OpenAI as: {openai_config}");

    let http_client = ReqwestClient::builder()
        .user_agent(
            std::env::var(PROXY_USER_AGENT)
                .as_deref()
                .unwrap_or("litellm/1.81.14"),
        )
        .build()?;

    let anthropic_client = HttpClient {
        http_client: http_client.clone(),
        config: Arc::new(anthropic_config),
    };

    let openai_client = HttpClient {
        http_client,
        config: Arc::new(openai_config),
    };

    let proxy_auth_key: SecretString = std::env::var(PROXY_AUTH_KEY)
        .context(PROXY_AUTH_KEY)?
        .into();

    let router = Router::new()
        .route(
            "/anthropic/{*path}",
            post(handlers::post)
                .get(handlers::get)
                .delete(handlers::delete),
        )
        .with_state(anthropic_client)
        .route(
            "/openai/{*path}",
            post(handlers::post)
                .get(handlers::get)
                .delete(handlers::delete),
        )
        .with_state(openai_client)
        .layer(middleware::from_fn(move |request, next| {
            authorize(request, next, proxy_auth_key.clone())
        }))
        .layer(middleware::from_fn(log_request));

    let app = router.into_make_service();
    let tcp_listener =
        TcpListener::bind(std::env::var(PROXY_ADDRESS).context(PROXY_ADDRESS)?).await?;

    tracing::info!("Server listening at {}", tcp_listener.local_addr()?);

    axum::serve(tcp_listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(Into::into)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler")
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("Shutdown signal received");
}

fn query_to_str(query: &HashMap<String, String>) -> Option<String> {
    (query.len() > 0).then_some(
        query
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&"),
    )
}

fn proxy_response(response: IncomingResponse) -> eyre::Result<OutgoingResponse> {
    const ALLOWED_INCOMING_HEADERS: [&str; 16] = [
        "content-type",
        "content-length",
        "cache-control",
        // anthropic ratelimits
        "anthropic-ratelimit-requests-limit",
        "anthropic-ratelimit-requests-remaining",
        "anthropic-ratelimit-requests-reset",
        "anthropic-ratelimit-tokens-limit",
        "anthropic-ratelimit-tokens-remaining",
        "anthropic-ratelimit-tokens-reset",
        // openai ratelimits
        "x-ratelimit-limit-requests",
        "x-ratelimit-limit-tokens",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-remaining-tokens",
        "x-ratelimit-reset-requests",
        "x-ratelimit-reset-tokens",
        "x-request-id",
    ];

    let status = response.status();

    let mut headers = HeaderMap::new();

    for (name, value) in response.headers() {
        if ALLOWED_INCOMING_HEADERS.contains(&name.as_str()) {
            headers.insert(name, value.clone());
        }
    }

    let body = Body::from_stream(response.bytes_stream());

    Ok((status, headers, body).into_response())
}

mod handlers {
    use std::collections::HashMap;

    use axum::{
        body::Bytes,
        extract::{Path, Query, State},
        http::HeaderMap,
        response::Response as OutgoingResponse,
    };

    use crate::{Error, HttpClient, proxy_response, query_to_str};

    pub async fn post(
        http_client: State<HttpClient>, Path(path): Path<String>,
        Query(query): Query<HashMap<String, String>>, headers: HeaderMap, body: Bytes,
    ) -> Result<OutgoingResponse, Error> {
        let response = http_client
            .post(&path, query_to_str(&query).as_deref(), body, &headers)?
            .send()
            .await?;

        proxy_response(response).map_err(Into::into)
    }

    pub async fn get(
        http_client: State<HttpClient>, Path(path): Path<String>,
        Query(query): Query<HashMap<String, String>>, headers: HeaderMap,
    ) -> Result<OutgoingResponse, Error> {
        let response = http_client
            .get(&path, query_to_str(&query).as_deref(), &headers)?
            .send()
            .await?;

        proxy_response(response).map_err(Into::into)
    }

    pub async fn delete(
        http_client: State<HttpClient>, Path(path): Path<String>,
        Query(query): Query<HashMap<String, String>>, headers: HeaderMap,
    ) -> Result<OutgoingResponse, Error> {
        let response = http_client
            .delete(&path, query_to_str(&query).as_deref(), &headers)?
            .send()
            .await?;

        proxy_response(response).map_err(Into::into)
    }
}
