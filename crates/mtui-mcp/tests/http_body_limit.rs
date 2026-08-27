//! HTTP request-body limit wiring for the http transport.
//!
//! `serve_http` mounts rmcp behind an axum `DefaultBodyLimit` derived from
//! `config.mcp_max_request_bytes`, so an unauthenticated pre-session request
//! cannot be buffered until memory exhaustion. End-to-end 413 behaviour needs a
//! live-socket harness; the offline property proven here is that the runner's
//! `DefaultBodyLimit` layer composes onto a router carrying a `tower::Service`
//! fallback (the shape rmcp's `StreamableHttpService` has), capped and disabled.

#![cfg(feature = "mcp")]

use axum::Router;
use axum::extract::DefaultBodyLimit;

/// A minimal fallback service standing in for rmcp's `StreamableHttpService`
/// (both are `tower::Service<Request>` mounted via `fallback_service`).
fn ok_router() -> Router {
    Router::new().fallback(|| async { "ok" })
}

/// A positive `mcp_max_request_bytes` composes as a `DefaultBodyLimit::max`
/// layer over the fallback service — the capped path the runner takes.
#[test]
fn capped_limit_layers_onto_fallback_service() {
    let _app: Router = ok_router().layer(DefaultBodyLimit::max(8192));
}

/// `mcp_max_request_bytes == 0` composes as `DefaultBodyLimit::disable()` —
/// the disabled path (drops even axum's implicit 2 MB floor).
#[test]
fn disabled_limit_layers_onto_fallback_service() {
    let _app: Router = ok_router().layer(DefaultBodyLimit::disable());
}
