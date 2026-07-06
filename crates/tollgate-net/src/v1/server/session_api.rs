#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::merchant;
use super::merchant_provider::add_allotment;
use super::{CustomerSession, ExternalUsageSnapshot, ServerState};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BootstrapRequest {
    pub access_method: String,
    pub token: String,
    pub mac: String,
    pub metric: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UsageReport {
    pub input_octets: Option<u64>,
    pub output_octets: Option<u64>,
    pub session_time: Option<u64>,
    pub source: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TopupRequest {
    pub token: String,
    pub source: Option<String>,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionResponse {
    pub session_id: String,
    pub access_level: String,
    pub allotment: u64,
    pub remaining_quota: i64,
    pub metric: String,
    pub next_checkin_ms: u64,
    pub is_final: bool,
    pub created_at: i64,
    pub last_usage_update: Option<String>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct TerminateResponse {
    status: String,
    session_id: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn json_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response()
}

fn error_json(status: StatusCode, error: &str, session_id: Option<&str>) -> Response {
    let body = serde_json::to_string(&ErrorResponse {
        error: error.to_owned(),
        session_id: session_id.map(std::borrow::ToOwned::to_owned),
    })
    .unwrap_or_default();
    json_response(status, body)
}

/// Build a `SessionResponse` from a loaded session, computing remaining quota
/// and access level from the current wall-clock / valve usage.
async fn build_session_response(
    session: &CustomerSession,
    state: &Arc<ServerState>,
    last_usage_update: Option<&str>,
) -> SessionResponse {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let (remaining, access_level) = if session.metric == "milliseconds" {
        let elapsed_ms = (now - session.start_time) * 1000;
        let rem = session.allotment as i64 - elapsed_ms;
        let level = if rem <= 0 { "suspended" } else { "active" };
        (rem, level.to_owned())
    } else {
        let usage = if let Some(ref ext) = session.last_external_usage {
            ext.input_octets + ext.output_octets
        } else {
            state
                .valve
                .get_client_usage_since_baseline(&session.mac_address)
                .await
                .unwrap_or(0)
        };
        let rem = session.allotment as i64 - usage as i64;
        let level = if rem <= 0 { "suspended" } else { "active" };
        (rem, level.to_owned())
    };

    // next_checkin_ms: reasonable default interval (5 seconds)
    let next_checkin_ms = 5_000u64;
    let is_final = remaining <= 0;

    SessionResponse {
        session_id: session.mac_address.clone(),
        access_level,
        allotment: session.allotment,
        remaining_quota: remaining,
        metric: session.metric.clone(),
        next_checkin_ms,
        is_final,
        created_at: session.start_time,
        last_usage_update: last_usage_update.map(std::borrow::ToOwned::to_owned),
    }
}

/// Check `X-API-Key` header against env var `TOLLGATE_API_KEY`.
/// Returns `true` when authentication passes (or when no key is configured).
fn check_api_key(headers: &HeaderMap) -> bool {
    let configured = match std::env::var("TOLLGATE_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return true, // no key configured → allow all
    };

    match headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        Some(provided) => provided == configured,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn build_session_router() -> axum::Router<Arc<ServerState>> {
    axum::Router::new()
        .route("/sessions/{mac}", get(handle_get_session))
        .route("/sessions/{mac}/usage", post(handle_post_usage))
        .route("/sessions/{mac}/topups", post(handle_post_topup))
        .route("/sessions/{mac}", delete(handle_delete_session))
        .route("/sessions/bootstrap", post(handle_post_bootstrap))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Normalize MAC address to lowercase with colon separators.
/// FreeRADIUS sends uppercase (B6-95-54-46-E0-27), session store uses lowercase.
fn normalize_mac(mac: &str) -> String {
    mac.to_lowercase().replace('-', ":").replace('.', ":")
}

/// GET /v1/sessions/{mac}
#[allow(clippy::too_many_lines)]
async fn handle_get_session(
    State(state): State<Arc<ServerState>>,
    Path(mac): Path<String>,
) -> Response {
    let mac = normalize_mac(&mac);
    let session = match state.sessions.get(&mac).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return error_json(StatusCode::NOT_FOUND, "session not found", Some(&mac));
        }
        Err(e) => {
            tracing::error!("Session store error for {mac}: {e}");
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session store error",
                Some(&mac),
            );
        }
    };

    let last_usage_update = session
        .last_external_usage
        .as_ref()
        .map(|u| u.reported_at.to_string());
    let resp = build_session_response(&session, &state, last_usage_update.as_deref()).await;
    json_response(
        StatusCode::OK,
        serde_json::to_string(&resp).unwrap_or_default(),
    )
}

/// POST /v1/sessions/{mac}/usage
#[allow(clippy::too_many_lines)]
async fn handle_post_usage(
    headers: HeaderMap,
    State(state): State<Arc<ServerState>>,
    Path(mac): Path<String>,
    Json(report): Json<UsageReport>,
) -> Response {
    if !check_api_key(&headers) {
        return error_json(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let mac = normalize_mac(&mac);

    let session = match state.sessions.get(&mac).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return error_json(StatusCode::NOT_FOUND, "session not found", Some(&mac));
        }
        Err(e) => {
            tracing::error!("Session store error for {mac}: {e}");
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session store error",
                Some(&mac),
            );
        }
    };

    // Log the usage data — the existing valve/janitor handles actual metering.
    tracing::info!(
        mac = %mac,
        source = %report.source,
        input_octets = ?report.input_octets,
        output_octets = ?report.output_octets,
        session_time = ?report.session_time,
        "External usage report received"
    );

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let snapshot = ExternalUsageSnapshot {
        input_octets: report.input_octets.unwrap_or(0),
        output_octets: report.output_octets.unwrap_or(0),
        session_time: report.session_time.unwrap_or(0),
        reported_at: now_secs,
    };

    let mut updated_session = session.clone();
    updated_session.last_external_usage = Some(snapshot);
    if let Err(e) = state.sessions.update(&mac, updated_session).await {
        tracing::error!("Failed to update external usage for {mac}: {e}");
    }

    // Use the provided timestamp or generate one
    let last_usage_update = report.timestamp.or_else(|| Some(now_secs.to_string()));

    let resp = build_session_response(&session, &state, last_usage_update.as_deref()).await;

    json_response(
        StatusCode::OK,
        serde_json::to_string(&resp).unwrap_or_default(),
    )
}

/// POST /v1/sessions/{mac}/topups
#[allow(clippy::too_many_lines)]
async fn handle_post_topup(
    headers: HeaderMap,
    State(state): State<Arc<ServerState>>,
    Path(mac): Path<String>,
    Json(req): Json<TopupRequest>,
) -> Response {
    if !check_api_key(&headers) {
        return error_json(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let mac = normalize_mac(&mac);

    // Verify session exists
    let existing = match state.sessions.get(&mac).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return error_json(StatusCode::NOT_FOUND, "session not found", Some(&mac));
        }
        Err(e) => {
            tracing::error!("Session store error for {mac}: {e}");
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session store error",
                Some(&mac),
            );
        }
    };

    // Redeem token
    let wallet = state.merchant.get();
    let amount = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        wallet.receive_token(req.token.as_bytes()),
    )
    .await
    {
        Ok(Ok(amount)) => amount,
        Ok(Err(e)) => {
            tracing::warn!("Token rejected for topup {mac}: {e}");
            return error_json(
                StatusCode::BAD_REQUEST,
                &format!("payment rejected: {e}"),
                Some(&mac),
            );
        }
        Err(_) => {
            tracing::warn!("Payment processing timed out for topup {mac}");
            return error_json(
                StatusCode::BAD_REQUEST,
                "payment processing timed out",
                Some(&mac),
            );
        }
    };

    let mint_url = state
        .config
        .accepted_mints
        .first()
        .map(|m| m.url.clone())
        .unwrap_or_default();

    let allotment = match merchant::calculate_allotment(amount.0, &mint_url, &state.config) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("Allotment calculation failed for topup {mac}: {e}");
            return error_json(
                StatusCode::BAD_REQUEST,
                &format!("allotment calculation failed: {e}"),
                Some(&mac),
            );
        }
    };

    let _prior = Some(existing);

    let session = match add_allotment(&*state.sessions, &mac, &state.config.metric, allotment).await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Session upsert failed for topup {mac}: {e}");
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to update session",
                Some(&mac),
            );
        }
    };

    // Open valve
    if let Err(e) = super::handlers::open_gate_for_session_pub(
        &*state.valve,
        &mac,
        &session.metric,
        session.start_time,
        session.allotment,
    )
    .await
    {
        tracing::error!("Valve open failed for topup {mac}: {e}");
        return error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "valve open failed",
            Some(&mac),
        );
    }

    let resp = build_session_response(&session, &state, None).await;
    json_response(
        StatusCode::OK,
        serde_json::to_string(&resp).unwrap_or_default(),
    )
}

/// DELETE /v1/sessions/{mac}
async fn handle_delete_session(
    headers: HeaderMap,
    State(state): State<Arc<ServerState>>,
    Path(mac): Path<String>,
) -> Response {
    if !check_api_key(&headers) {
        return error_json(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let mac = normalize_mac(&mac);

    match state.sessions.remove(&mac).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return error_json(StatusCode::NOT_FOUND, "session not found", Some(&mac));
        }
        Err(e) => {
            tracing::error!("Session removal failed for {mac}: {e}");
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session store error",
                Some(&mac),
            );
        }
    }

    // Close valve
    if let Err(e) = state.valve.close_gate(&mac).await {
        tracing::warn!("Failed to close valve for {mac}: {e}");
    }

    let resp = TerminateResponse {
        status: "terminated".to_owned(),
        session_id: mac,
    };
    json_response(
        StatusCode::OK,
        serde_json::to_string(&resp).unwrap_or_default(),
    )
}

/// POST /v1/sessions/bootstrap
#[allow(clippy::too_many_lines)]
async fn handle_post_bootstrap(
    headers: HeaderMap,
    State(state): State<Arc<ServerState>>,
    Json(req): Json<BootstrapRequest>,
) -> Response {
    if !check_api_key(&headers) {
        return error_json(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }

    let mac = normalize_mac(&req.mac);
    let metric = req.metric.unwrap_or_else(|| state.config.metric.clone());

    // Redeem token
    let wallet = state.merchant.get();
    let amount = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        wallet.receive_token(req.token.as_bytes()),
    )
    .await
    {
        Ok(Ok(amount)) => amount,
        Ok(Err(e)) => {
            tracing::warn!("Token rejected for bootstrap {mac}: {e}");
            return error_json(
                StatusCode::BAD_REQUEST,
                &format!("payment rejected: {e}"),
                Some(&mac),
            );
        }
        Err(_) => {
            tracing::warn!("Payment processing timed out for bootstrap {mac}");
            return error_json(
                StatusCode::BAD_REQUEST,
                "payment processing timed out",
                Some(&mac),
            );
        }
    };

    let mint_url = state
        .config
        .accepted_mints
        .first()
        .map(|m| m.url.clone())
        .unwrap_or_default();

    let allotment = match merchant::calculate_allotment(amount.0, &mint_url, &state.config) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("Allotment calculation failed for bootstrap {mac}: {e}");
            return error_json(
                StatusCode::BAD_REQUEST,
                &format!("allotment calculation failed: {e}"),
                Some(&mac),
            );
        }
    };

    let prior_session = state.sessions.get(&mac).await.ok().flatten();

    let session = match add_allotment(&*state.sessions, &mac, &metric, allotment).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Session upsert failed for bootstrap {mac}: {e}");
            return error_json(
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to create session",
                Some(&mac),
            );
        }
    };

    // Open valve
    if let Err(e) = super::handlers::open_gate_for_session_pub(
        &*state.valve,
        &mac,
        &session.metric,
        session.start_time,
        session.allotment,
    )
    .await
    {
        tracing::error!("Valve open failed for bootstrap {mac}, rolling back: {e}");
        // Rollback
        match prior_session {
            Some(old) => {
                if let Err(re) = state.sessions.update(&mac, old).await {
                    tracing::error!("Failed to restore prior session for {mac}: {re}");
                }
            }
            None => {
                if let Err(re) = state.sessions.remove(&mac).await {
                    tracing::error!("Failed to remove new session for {mac}: {re}");
                }
            }
        }
        return error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            "valve open failed, session rolled back",
            Some(&mac),
        );
    }

    tracing::info!(
        mac = %mac,
        access_method = %req.access_method,
        allotment,
        "Session bootstrapped via HTTP-04 API"
    );

    let resp = build_session_response(&session, &state, None).await;
    json_response(
        StatusCode::OK,
        serde_json::to_string(&resp).unwrap_or_default(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockWallet;
    use crate::v1::server::{
        AcceptedMint, InMemoryLightningQuoteStore, InMemorySessionStore, MerchantProvider,
        NoopValve, StubMacResolver, V1ServerConfig,
    };
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use nostr::prelude::*;
    use tollgate_core::wallet::Wallet;
    use tower::ServiceExt;

    fn test_server_state() -> Arc<ServerState> {
        let wallet: Arc<dyn Wallet> = Arc::new(MockWallet::new(0));
        let merchant = Arc::new(MerchantProvider::new(wallet));
        let nostr_keys = Keys::generate();
        let config = V1ServerConfig {
            metric: "milliseconds".to_owned(),
            step_size: 60_000,
            accepted_mints: vec![AcceptedMint {
                url: "https://test.example.com".to_owned(),
                price_per_step: 1,
                unit: "sat".to_owned(),
                min_steps: 1,
            }],
            nostr_keys,
            port: 2121,
        };
        let advertisement = merchant::build_advertisement(&config).expect("build ad");
        Arc::new(ServerState {
            merchant,
            config,
            sessions: Arc::new(InMemorySessionStore::new()),
            mac_resolver: Arc::new(StubMacResolver::new("00:11:22:33:44:55")),
            valve: Arc::new(NoopValve),
            mint_quote_wallet: None,
            lightning_quotes: Arc::new(InMemoryLightningQuoteStore::new()),
            advertisement,
        })
    }

    /// Produce a token string whose `.as_bytes()` first 8 bytes decode to
    /// the requested amount in big-endian (what MockWallet expects).
    fn mock_token_string(amount: u64) -> String {
        let bytes = amount.to_be_bytes();
        bytes.map(|b| b as char).iter().collect()
    }

    async fn read_body(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    fn test_app(state: Arc<ServerState>) -> axum::Router {
        build_session_router().with_state(state)
    }

    static API_KEY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    async fn with_api_key<F, Fut>(key: &str, f: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _guard = API_KEY_LOCK.lock().unwrap();
        std::env::set_var("TOLLGATE_API_KEY", key);
        f().await;
        std::env::remove_var("TOLLGATE_API_KEY");
    }

    #[tokio::test]
    async fn test_get_session_not_found() {
        let state = test_server_state();
        let app = test_app(state);
        let req = Request::builder()
            .uri("/sessions/aa:bb:cc:dd:ee:ff")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = read_body(response).await;
        assert!(body.contains("session not found"));
    }

    #[tokio::test]
    async fn test_bootstrap_creates_session() {
        with_api_key("session-api-test-key", || async {
            let state = test_server_state();
            let app = test_app(state.clone());

            let token_str = mock_token_string(8);
            let body = serde_json::json!({
                "access_method": "ssh",
                "token": token_str,
                "mac": "aa:bb:cc:dd:ee:ff",
            });

            let req = Request::builder()
                .method("POST")
                .uri("/sessions/bootstrap")
                .header("content-type", "application/json")
                .header("x-api-key", "session-api-test-key")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = read_body(response).await;
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(parsed["session_id"], "aa:bb:cc:dd:ee:ff");
            assert_eq!(parsed["access_level"], "active");
            assert!(parsed["allotment"].as_u64().unwrap() > 0);

            let app2 = test_app(state);
            let req = Request::builder()
                .uri("/sessions/aa:bb:cc:dd:ee:ff")
                .body(Body::empty())
                .unwrap();
            let response = app2.oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        })
        .await;
    }

    #[tokio::test]
    async fn test_bootstrap_unauthorized_without_api_key() {
        with_api_key("session-api-test-key-unauth", || async {
            let state = test_server_state();
            let app = test_app(state);

            let body = serde_json::json!({
                "access_method": "ssh",
                "token": "deadbeef",
                "mac": "aa:bb:cc:dd:ee:ff",
            });

            let req = Request::builder()
                .method("POST")
                .uri("/sessions/bootstrap")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        })
        .await;
    }

    #[tokio::test]
    async fn test_delete_session() {
        with_api_key("session-api-test-key-del", || async {
            let state = test_server_state();

            let token_str = mock_token_string(8);
            let bootstrap_body = serde_json::json!({
                "access_method": "ssh",
                "token": token_str,
                "mac": "aa:bb:cc:dd:ee:ff",
            });
            let app = test_app(state.clone());
            let req = Request::builder()
                .method("POST")
                .uri("/sessions/bootstrap")
                .header("content-type", "application/json")
                .header("x-api-key", "session-api-test-key-del")
                .body(Body::from(serde_json::to_vec(&bootstrap_body).unwrap()))
                .unwrap();
            let response = app.oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let app2 = test_app(state.clone());
            let req = Request::builder()
                .method("DELETE")
                .uri("/sessions/aa:bb:cc:dd:ee:ff")
                .header("x-api-key", "session-api-test-key-del")
                .body(Body::empty())
                .unwrap();
            let response = app2.oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = read_body(response).await;
            assert!(body.contains("terminated"));

            let app3 = test_app(state);
            let req = Request::builder()
                .uri("/sessions/aa:bb:cc:dd:ee:ff")
                .body(Body::empty())
                .unwrap();
            let response = app3.oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        })
        .await;
    }

    #[tokio::test]
    async fn test_post_usage_logs_and_returns_session() {
        with_api_key("session-api-test-key-usage", || async {
            let state = test_server_state();

            let token_str = mock_token_string(8);
            let bootstrap_body = serde_json::json!({
                "access_method": "radius",
                "token": token_str,
                "mac": "11:22:33:44:55:66",
            });
            let app = test_app(state.clone());
            let req = Request::builder()
                .method("POST")
                .uri("/sessions/bootstrap")
                .header("content-type", "application/json")
                .header("x-api-key", "session-api-test-key-usage")
                .body(Body::from(serde_json::to_vec(&bootstrap_body).unwrap()))
                .unwrap();
            let response = app.oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let usage_body = serde_json::json!({
                "input_octets": 1024,
                "output_octets": 2048,
                "session_time": 60,
                "source": "radius",
            });
            let app2 = test_app(state);
            let req = Request::builder()
                .method("POST")
                .uri("/sessions/11:22:33:44:55:66/usage")
                .header("content-type", "application/json")
                .header("x-api-key", "session-api-test-key-usage")
                .body(Body::from(serde_json::to_vec(&usage_body).unwrap()))
                .unwrap();
            let response = app2.oneshot(req).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = read_body(response).await;
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(parsed["session_id"], "11:22:33:44:55:66");
            assert!(parsed["last_usage_update"].is_string());
        })
        .await;
    }
}
