use std::collections::HashMap;

use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderMap, Method},
    response::{IntoResponse as _, Response as OutgoingResponse},
};
use color_eyre::eyre;
use reqwest::Response as IncomingResponse;

use crate::{Error, HttpClient};

pub mod middleware;

fn proxy_response_stream(response: IncomingResponse) -> eyre::Result<OutgoingResponse> {
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

pub async fn proxy(
    http_client: State<HttpClient>,
    Path(path): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> Result<OutgoingResponse, Error> {
    let mut request = http_client.request(&path, &query, method, &headers)?;

    if !body.is_empty() {
        request = request.body(body);
    };

    let response = request.send().await?;
    proxy_response_stream(response).map_err(Into::into)
}
