use std::time::Instant;

use axum::{
    body::Body,
    http::{Request, StatusCode, header::AUTHORIZATION},
    middleware::Next,
    response::{IntoResponse as _, Response},
};
use secrecy::{ExposeSecret as _, SecretString};
use tracing::Instrument as _;

use crate::error::Error;

pub async fn log_request(request: Request<Body>, next: Next) -> Response {
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

pub async fn authorize(
    request: Request<Body>,
    next: Next,
    expected_key: SecretString,
) -> Result<Response, Error> {
    let headers = request.headers();

    let provided_key = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| {
            let h = h.trim();
            h.strip_prefix("Bearer ")
                .or_else(|| h.strip_prefix("bearer "))
                .or(Some(h))
        })
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
        });

    match provided_key {
        Some(key) if key == expected_key.expose_secret() => Ok(next.run(request).await),
        _ => {
            tracing::warn!("Unauthorized request attempt");
            Ok(StatusCode::UNAUTHORIZED.into_response())
        }
    }
}
