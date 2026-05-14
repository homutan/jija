use std::{
    collections::HashMap,
    fmt::{self, Display, Formatter},
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    http::HeaderMap,
    middleware::{self},
    routing::post,
};
use color_eyre::eyre::{self, Context};
use reqwest::{Client as ReqwestClient, Method, RequestBuilder, Url};
use secrecy::SecretString;
use tokio::{net::TcpListener, signal};
use tracing_error::ErrorLayer;
use tracing_subscriber::{
    EnvFilter, Layer as _, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

use crate::{error::Error, keys::KeyRepo};

mod error;
mod keys;
mod routes;

const PROXY_ADDRESS: &str = "PROXY_ADDRESS";
const PROXY_AUTH_KEY: &str = "PROXY_AUTH_KEY";
const PROXY_USER_AGENT: &str = "PROXY_USER_AGENT";

const PRETTY_LOGS: &str = "PRETTY_LOGS";

const ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const ANTHROPIC_AUTH_KEY: &str = "ANTHROPIC_AUTH_KEY";

const OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
const OPENAI_AUTH_KEY: &str = "OPENAI_AUTH_KEY";

#[derive(Debug)]
pub enum Provider {
    Anthropic {
        base_url: Url,
        keys: Mutex<KeyRepo<String>>,
    },
    OpenAI {
        base_url: Url,
        keys: Mutex<KeyRepo<String>>,
    },
}

impl Display for Provider {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Provider::Anthropic { base_url, .. } => {
                write!(f, "{{ base_url: {} }}", base_url.as_str())
            }
            Provider::OpenAI { base_url, .. } => {
                write!(f, "{{ base_url: {} }}", base_url.as_str())
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct HttpClient {
    http_client: ReqwestClient,
    config: Arc<Provider>,
}

impl HttpClient {
    pub fn request(
        &self,
        path: &str,
        query: &HashMap<String, String>,
        method: Method,
        headers: &HeaderMap,
    ) -> eyre::Result<RequestBuilder> {
        // Строится базовый URL
        let mut url = match self.config.as_ref() {
            Provider::Anthropic { base_url, .. } => base_url.join(path)?,
            Provider::OpenAI { base_url, .. } => base_url.join(path)?,
        };

        fn query_to_str(query: &HashMap<String, String>) -> Option<String> {
            if query.is_empty() {
                return None;
            }

            let query = query
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");

            Some(query)
        }

        // К базовому URL добавляется query в правильном формате
        url.set_query(query_to_str(query).as_deref());

        // Собирается новый запрос
        let request = self.http_client.request(method, url);

        // Запросу добавляются ключи
        let mut request = match self.config.as_ref() {
            Provider::Anthropic { keys, .. } => match keys.lock().unwrap().next() {
                Some(key) => request.header("x-api-key", key),
                None => eyre::bail!("no keys available"),
            },
            Provider::OpenAI { keys, .. } => match keys.lock().unwrap().next() {
                Some(key) => request.bearer_auth(key),
                None => eyre::bail!("no keys available"),
            },
        };

        // Пробрасываемый запрос чистится от сторонних хедеров
        const ALLOWED_OUTGOING_HEADERS: [&str; 6] = [
            "content-type",
            "cache-control",
            "anthropic-version",
            "anthropic-beta",
            "openai-beta",
            "openai-organization",
        ];

        for (name, value) in headers {
            if ALLOWED_OUTGOING_HEADERS.contains(&name.as_str()) {
                request = request.header(name, value);
            }
        }

        Ok(request)
    }
}

fn env_to_str(key: &str) -> eyre::Result<String> {
    std::env::var(key).with_context(|| key.to_owned())
}

fn env_to_url(key: &str) -> eyre::Result<Url> {
    let mut s = env_to_str(key)?;

    if !s.ends_with("/") {
        s.push('/');
    }

    Url::parse(&s).with_context(|| key.to_owned())
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

    let anthropic_config = Provider::Anthropic {
        base_url: env_to_url(ANTHROPIC_BASE_URL)?,
        keys: Mutex::new(KeyRepo::try_from_str(&env_to_str(ANTHROPIC_AUTH_KEY)?)?),
    };

    tracing::info!("Using Anthropic as: {anthropic_config}");

    let openai_config = Provider::OpenAI {
        base_url: env_to_url(OPENAI_BASE_URL)?,
        keys: Mutex::new(KeyRepo::try_from_str(&env_to_str(OPENAI_AUTH_KEY)?)?),
    };

    tracing::info!("Using OpenAI as: {openai_config}");

    let http_client = ReqwestClient::builder()
        .user_agent(
            std::env::var(PROXY_USER_AGENT)
                .as_deref()
                .unwrap_or("litellm/1.81.14"),
        )
        .build()?;

    let anthropic_state = HttpClient {
        http_client: http_client.clone(),
        config: Arc::new(anthropic_config),
    };

    let openai_state = HttpClient {
        http_client,
        config: Arc::new(openai_config),
    };

    // TODO: user tokens
    let proxy_auth_key: SecretString = env_to_str(PROXY_AUTH_KEY)?.into();

    let router = Router::new()
        .route("/anthropic/{*p}", post(routes::proxy).get(routes::proxy).delete(routes::proxy))
        .with_state(anthropic_state)
        .route("/openai/{*p}", post(routes::proxy).get(routes::proxy).delete(routes::proxy))
        .with_state(openai_state)
        .layer(middleware::from_fn(move |request, next| {
            routes::middleware::authorize(request, next, proxy_auth_key.clone())
        }))
        .layer(middleware::from_fn(routes::middleware::log_request));

    let app = router.into_make_service();
    let tcp_listener = TcpListener::bind(env_to_str(PROXY_ADDRESS)?).await?;

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
