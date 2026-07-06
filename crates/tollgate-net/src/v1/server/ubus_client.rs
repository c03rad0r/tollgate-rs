#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

//! ubus HTTP JSON-RPC client for OpenWrt.
//!
//! # Go v1 vs Rust comparison
//!
//! Go v1 does NOT use ubus — it relies entirely on shell commands.
//! Rust adds ubus as an alternative transport, enabling:
//! - Remote configuration without SSH
//! - JSON-RPC API for integration with other tools
//! - Better error handling than shell command parsing
//!
//! Based on conwrt's Python `ubus_utils.py` pattern.
//!
//! # JSON-RPC protocol
//!
//! All calls use OpenWrt's ubus JSON-RPC over HTTP:
//! ```json
//! {
//!   "jsonrpc": "2.0",
//!   "id": 1,
//!   "method": "call",
//!   "params": ["<token>", "uci", "set", {"config": "wireless", "section": "radio0", "values": {"disabled": "0"}}]
//! }
//! ```
//!
//! Response:
//! ```json
//! {"jsonrpc": "2.0", "id": 1, "result": [0, {}]}
//! ```
//!
//! `result[0]` is the status code (0 = success). `result[1]` is the
//! response payload.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::uci_ops::{OpValue, UciOp, UciOpError};

/// Default timeout for ubus HTTP requests (30 seconds).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// ubus JSON-RPC errors.
#[derive(Debug, Error)]
pub enum UbusError {
    /// Authentication failed (wrong username/password, or rpcd not running).
    #[error("authentication failed: {0}")]
    AuthFailed(String),
    /// ubus call returned a non-zero status code.
    #[error("call failed with code {code}: {message}")]
    CallFailed { code: i64, message: String },
    /// HTTP transport error (connection refused, timeout, etc.).
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    /// Connection-level error (malformed URL, etc.).
    #[error("connection error: {0}")]
    Connection(String),
}

/// JSON-RPC request envelope.
#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: serde_json::Value,
}

/// JSON-RPC response envelope.
#[derive(Deserialize, Debug)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: u64,
    result: Option<serde_json::Value>,
    error: Option<serde_json::Value>,
}

/// JSON-RPC response for login (returns `[0, {"ubus_rpc_session": "..."}]`).
#[derive(Deserialize, Debug)]
struct LoginResult {
    #[serde(rename = "ubus_rpc_session")]
    session: String,
}

/// WiFi radio information from `wireless.radio.*` UCI sections.
#[derive(Debug, Clone, Deserialize)]
pub struct RadioInfo {
    /// Radio device name (e.g. `radio0`, `radio1`).
    pub name: String,
    /// Band (`"2g"` or `"5g"`).
    pub band: String,
    /// Channel number as string (e.g. `"36"`, `"auto"`).
    pub channel: String,
}

/// Async ubus HTTP JSON-RPC client for OpenWrt.
///
/// Connects to OpenWrt's `uhttpd` ubus endpoint (typically at
/// `http://{host}/ubus`) and provides methods for UCI configuration,
/// service management, and system introspection.
pub struct UbusClient {
    /// Base URL for ubus endpoint (e.g. `http://192.168.1.1/ubus`).
    url: String,
    /// Authentication token (set after successful `login`).
    token: Option<String>,
    /// HTTP request timeout.
    timeout: Duration,
    /// JSON-RPC request ID counter.
    id_counter: AtomicUsize,
    /// HTTP client.
    client: reqwest::Client,
}

impl UbusClient {
    /// Create a new ubus client targeting the given host and port.
    ///
    /// The client is usable immediately for unauthenticated calls,
    /// but most UCI operations require calling [`login`](Self::login) first.
    pub fn new(host: &str, port: u16) -> Self {
        let url = format!("http://{host}:{port}/ubus");
        Self {
            url,
            token: None,
            timeout: DEFAULT_TIMEOUT,
            id_counter: AtomicUsize::new(0),
            client: reqwest::Client::builder()
                .timeout(DEFAULT_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }

    /// Set a custom timeout for HTTP requests.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self.client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_default();
        self
    }

    /// Authenticate with the OpenWrt router via rpcd.
    ///
    /// On success, stores the session token for subsequent calls.
    /// On failure, returns [`UbusError::AuthFailed`].
    pub async fn login(&mut self, username: &str, password: &str) -> Result<(), UbusError> {
        let params = serde_json::json!([username, password]);
        let response = self.rpc_call_raw("session", "login", params).await?;

        // response is the `result` array: [0, {"ubus_rpc_session": "...", ...}]
        let login_data: LoginResult = serde_json::from_value(response)
            .map_err(|e| UbusError::AuthFailed(format!("failed to parse login response: {e}")))?;

        tracing::debug!(
            "ubus login successful, session length={}",
            login_data.session.len()
        );
        self.token = Some(login_data.session);
        Ok(())
    }

    /// Get UCI configuration data.
    ///
    /// Returns the full section data as a JSON value.
    /// Maps to ubus `uci get` call.
    pub async fn uci_get(
        &self,
        config: &str,
        section: &str,
    ) -> Result<serde_json::Value, UbusError> {
        let params = serde_json::json!({
            "config": config,
            "section": section,
        });
        self.rpc_call("uci", "get", params).await
    }

    /// Set UCI option values on a section.
    ///
    /// `values` should be a JSON object mapping option names to values.
    /// Maps to ubus `uci set` call.
    pub async fn uci_set(
        &self,
        config: &str,
        section: &str,
        values: serde_json::Value,
    ) -> Result<(), UbusError> {
        let params = serde_json::json!({
            "config": config,
            "section": section,
            "values": values,
        });
        self.rpc_call("uci", "set", params).await?;
        Ok(())
    }

    /// Add a new UCI section.
    ///
    /// Returns the name of the newly created section.
    /// Maps to ubus `uci add` call.
    pub async fn uci_add(
        &self,
        config: &str,
        type_name: &str,
        values: serde_json::Value,
    ) -> Result<String, UbusError> {
        let params = serde_json::json!({
            "config": config,
            "type": type_name,
            "values": values,
        });
        let result = self.rpc_call("uci", "add", params).await?;

        // Response is {"section": "cfg012345"}
        result
            .get("section")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| {
                UbusError::Connection("missing 'section' in uci.add response".to_owned())
            })
    }

    /// Delete a UCI section or a specific option within a section.
    ///
    /// If `option` is `None`, the entire section is deleted.
    /// Maps to ubus `uci delete` call.
    pub async fn uci_delete(
        &self,
        config: &str,
        section: &str,
        option: Option<&str>,
    ) -> Result<(), UbusError> {
        let mut params = serde_json::json!({
            "config": config,
            "section": section,
        });
        if let Some(opt) = option {
            params["option"] = serde_json::Value::String(opt.to_owned());
        }
        self.rpc_call("uci", "delete", params).await?;
        Ok(())
    }

    /// Commit pending UCI changes for a config.
    ///
    /// Maps to ubus `uci commit` call.
    pub async fn uci_commit(&self, config: &str) -> Result<(), UbusError> {
        let params = serde_json::json!({
            "config": config,
        });
        self.rpc_call("uci", "commit", params).await?;
        Ok(())
    }

    /// Execute a service management action.
    ///
    /// `action` should be one of `"start"`, `"stop"`, `"restart"`,
    /// `"enable"`, `"disable"`.
    pub async fn service_action(&self, name: &str, action: &str) -> Result<(), UbusError> {
        let ubus_object = format!("rc.{action}");
        let params = serde_json::json!({
            "name": name,
        });
        self.rpc_call(&ubus_object, action, params).await?;
        Ok(())
    }

    /// Get board information from the router.
    ///
    /// Returns kernel version, hostname, model, etc.
    pub async fn board(&self) -> Result<serde_json::Value, UbusError> {
        self.rpc_call("system", "board", serde_json::json!({}))
            .await
    }

    /// Discover WiFi radio devices configured on the router.
    ///
    /// Reads the `wireless` UCI config and extracts radio sections,
    /// returning their name, band, and channel.
    pub async fn discover_radios(&self) -> Result<Vec<RadioInfo>, UbusError> {
        let wireless = self.uci_get("wireless", "").await?;

        // wireless config is an object with section names as keys
        let obj = wireless
            .as_object()
            .ok_or_else(|| UbusError::Connection("wireless config is not an object".to_owned()))?;

        let mut radios = Vec::new();
        for (name, section) in obj {
            // Radio sections have `.type` field (e.g. "mac80211") or name starts with "radio"
            let is_radio = name.starts_with("radio")
                || section
                    .get(".type")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t == "wifi-device");
            if !is_radio {
                continue;
            }

            let band = section
                .get("band")
                .or_else(|| section.get("hwmode"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_owned();

            let channel = section
                .get("channel")
                .and_then(|v| v.as_str())
                .unwrap_or("auto")
                .to_owned();

            radios.push(RadioInfo {
                name: name.clone(),
                band,
                channel,
            });
        }

        tracing::debug!("discovered {} radios", radios.len());
        Ok(radios)
    }

    /// Find a radio device matching a specific band (e.g. `"2g"` or `"5g"`).
    ///
    /// Returns the radio name (e.g. `"radio0"`) or `None` if no match.
    pub async fn find_radio_for_band(&self, band: &str) -> Result<Option<String>, UbusError> {
        let radios = self.discover_radios().await?;
        let target = band.to_lowercase();
        Ok(radios
            .iter()
            .find(|r| r.band.to_lowercase() == target)
            .map(|r| r.name.clone()))
    }

    /// Make a raw JSON-RPC call with authentication token.
    ///
    /// This is the core method — all public methods delegate to it.
    /// Sends a JSON-RPC `call` request with the session token, ubus
    /// object, method, and parameters.
    async fn rpc_call(
        &self,
        ubus_obj: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, UbusError> {
        let token = self.token.as_deref().ok_or_else(|| {
            UbusError::Connection("not authenticated — call login() first".to_owned())
        })?;

        let rpc_params = serde_json::json!([token, ubus_obj, method, params]);

        self.rpc_call_raw(ubus_obj, method, rpc_params).await
    }

    /// Low-level JSON-RPC call that returns the result data.
    ///
    /// Handles the JSON-RPC envelope, status code checking, and error
    /// extraction. Returns the inner result data on success.
    async fn rpc_call_raw(
        &self,
        ubus_obj: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, UbusError> {
        let id = self.id_counter.fetch_add(1, Ordering::Relaxed) as u64 + 1;
        let request = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: "call",
            params,
        };

        tracing::trace!("ubus call {ubus_obj}.{method} (id={})", request.id);

        let response = self.client.post(&self.url).json(&request).send().await?;

        let rpc_response: JsonRpcResponse = response.json().await?;

        // Check for JSON-RPC level error
        if let Some(error) = rpc_response.error {
            let code = error
                .get("code")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(-1);
            let message = error
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_owned();
            return Err(UbusError::CallFailed { code, message });
        }

        // Parse ubus result array: [status_code, result_data]
        let result = rpc_response.result.ok_or_else(|| {
            UbusError::Connection("missing result in JSON-RPC response".to_owned())
        })?;

        let result_arr = result
            .as_array()
            .ok_or_else(|| UbusError::Connection("result is not an array".to_owned()))?;

        if result_arr.is_empty() {
            return Err(UbusError::Connection("empty result array".to_owned()));
        }

        let status_code = result_arr[0].as_i64().unwrap_or(-1);

        if status_code != 0 {
            let message = result_arr
                .get(1)
                .and_then(|v| v.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown ubus error")
                .to_owned();
            return Err(UbusError::CallFailed {
                code: status_code,
                message,
            });
        }

        // Return the result data (second element), or empty object
        Ok(result_arr
            .get(1)
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::default())))
    }
}

/// Execute a list of [`UciOp`]s via ubus.
///
/// Shell-only ops (`Shell`, `Comment`) are logged and skipped since
/// they don't map to ubus RPC calls. All other ops are executed
/// sequentially. Returns one result per input op.
///
/// # Errors
/// Returns individual [`UciOpError`] per failed op without stopping
/// execution of subsequent ops.
#[allow(clippy::too_many_lines)]
pub async fn execute_ubus(client: &UbusClient, ops: &[UciOp]) -> Vec<Result<(), UciOpError>> {
    let mut results = Vec::with_capacity(ops.len());

    for op in ops {
        let result = match op {
            UciOp::Set {
                config,
                section,
                values,
            } => {
                let mut json_values = serde_json::Map::new();
                for (key, value) in values {
                    match value {
                        OpValue::Single(v) => {
                            json_values.insert(key.clone(), serde_json::Value::String(v.clone()));
                        }
                        OpValue::List(items) => {
                            let arr: Vec<serde_json::Value> = items
                                .iter()
                                .map(|i| serde_json::Value::String(i.clone()))
                                .collect();
                            json_values.insert(key.clone(), serde_json::Value::Array(arr));
                        }
                    }
                }
                client
                    .uci_set(config, section, serde_json::Value::Object(json_values))
                    .await
                    .map_err(|e| UciOpError::UbusError(e.to_string()))
            }

            UciOp::Add {
                config,
                type_name,
                name: _,
                values,
            } => {
                let mut json_values = serde_json::Map::new();
                for (key, value) in values {
                    match value {
                        OpValue::Single(v) => {
                            json_values.insert(key.clone(), serde_json::Value::String(v.clone()));
                        }
                        OpValue::List(items) => {
                            let arr: Vec<serde_json::Value> = items
                                .iter()
                                .map(|i| serde_json::Value::String(i.clone()))
                                .collect();
                            json_values.insert(key.clone(), serde_json::Value::Array(arr));
                        }
                    }
                }
                match client
                    .uci_add(config, type_name, serde_json::Value::Object(json_values))
                    .await
                {
                    Ok(_section_name) => Ok(()),
                    Err(e) => Err(UciOpError::UbusError(e.to_string())),
                }
            }

            UciOp::Delete {
                config,
                section,
                option,
            } => client
                .uci_delete(config, section, option.as_deref())
                .await
                .map_err(|e| UciOpError::UbusError(e.to_string())),

            UciOp::AddList {
                config,
                section,
                option,
                value,
            } => {
                // ubus doesn't have add_list — use uci_set with array value
                let values = serde_json::json!({
                    option: [value]
                });
                client
                    .uci_set(config, section, values)
                    .await
                    .map_err(|e| UciOpError::UbusError(e.to_string()))
            }

            UciOp::Commit { config } => client
                .uci_commit(config)
                .await
                .map_err(|e| UciOpError::UbusError(e.to_string())),

            UciOp::Service { name, action } => {
                let action_str = match action {
                    super::uci_ops::ServiceAction::Start => "start",
                    super::uci_ops::ServiceAction::Stop => "stop",
                    super::uci_ops::ServiceAction::Restart => "restart",
                    super::uci_ops::ServiceAction::Enable => "enable",
                    super::uci_ops::ServiceAction::Disable => "disable",
                };
                client
                    .service_action(name, action_str)
                    .await
                    .map_err(|e| UciOpError::UbusError(e.to_string()))
            }

            UciOp::Shell { command } => {
                tracing::debug!("skipping shell-only op via ubus: {command}");
                Ok(())
            }

            UciOp::Comment { text } => {
                tracing::debug!("comment: {text}");
                Ok(())
            }
        };

        results.push(result);
    }

    results
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;

    /// Helper that creates a mockito server with a fixed response.
    ///
    /// Returns `(client, server_guard, mock)`. The mock MUST be kept alive
    /// for the duration of the test — dropping it unregisters the handler.
    async fn mock_ubus(body: &str) -> (UbusClient, mockito::ServerGuard, mockito::Mock) {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/ubus")
            .match_header("content-type", "application/json")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create_async()
            .await;

        let mut client = UbusClient::new("0.0.0.0", 0);
        client.url = format!("{}/ubus", server.url());
        client.token = Some("test-token-12345".to_owned());

        (client, server, mock)
    }

    /// Build a client without a token for auth-failure tests.
    fn unauthenticated_client() -> UbusClient {
        UbusClient::new("192.168.1.1", 80)
    }

    #[tokio::test]
    async fn test_login_success() {
        let (mut client, _server, _mock) = mock_ubus(
            r#"{"jsonrpc":"2.0","id":1,"result":[0,{"ubus_rpc_session":"abc123def456","timeout":300,"expires":300}]}"#,
        )
        .await;
        client.token = None;

        let result = client.login("root", "password").await;
        assert!(result.is_ok(), "login should succeed: {result:?}");
        assert_eq!(client.token.as_deref(), Some("abc123def456"));
    }

    #[tokio::test]
    async fn test_login_failure() {
        let (mut client, _server, _mock) =
            mock_ubus(r#"{"jsonrpc":"2.0","id":1,"result":[6,{"message":"Access denied"}]}"#).await;
        client.token = None;

        let result = client.login("root", "wrongpassword").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_uci_set_success() {
        let (client, _server, _mock) =
            mock_ubus(r#"{"jsonrpc":"2.0","id":1,"result":[0,{}]}"#).await;

        let result = client
            .uci_set("wireless", "radio0", serde_json::json!({"disabled": "0"}))
            .await;
        assert!(result.is_ok(), "uci_set should succeed: {result:?}");
    }

    #[tokio::test]
    async fn test_uci_get_success() {
        let (client, _server, _mock) = mock_ubus(
            r#"{"jsonrpc":"2.0","id":1,"result":[0,{".type":"wifi-device","disabled":"0","channel":"36"}]}"#,
        )
        .await;

        let result = client.uci_get("wireless", "radio0").await;
        assert!(result.is_ok(), "uci_get should succeed: {result:?}");

        let value = result.unwrap();
        assert_eq!(value["disabled"], "0");
        assert_eq!(value["channel"], "36");
    }

    #[tokio::test]
    async fn test_uci_add_success() {
        let (client, _server, _mock) =
            mock_ubus(r#"{"jsonrpc":"2.0","id":1,"result":[0,{"section":"cfg012345"}]}"#).await;

        let result = client
            .uci_add(
                "wireless",
                "wifi-iface",
                serde_json::json!({"ssid": "TestAP"}),
            )
            .await;
        assert!(result.is_ok(), "uci_add should succeed: {result:?}");
        assert_eq!(result.unwrap(), "cfg012345");
    }

    #[tokio::test]
    async fn test_uci_delete_section() {
        let (client, _server, _mock) =
            mock_ubus(r#"{"jsonrpc":"2.0","id":1,"result":[0,{}]}"#).await;

        let result = client.uci_delete("wireless", "old_iface", None).await;
        assert!(result.is_ok(), "uci_delete should succeed: {result:?}");
    }

    #[tokio::test]
    async fn test_uci_delete_option() {
        let (client, _server, _mock) =
            mock_ubus(r#"{"jsonrpc":"2.0","id":1,"result":[0,{}]}"#).await;

        let result = client
            .uci_delete("wireless", "radio0", Some("disabled"))
            .await;
        assert!(
            result.is_ok(),
            "uci_delete option should succeed: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_uci_commit_success() {
        let (client, _server, _mock) =
            mock_ubus(r#"{"jsonrpc":"2.0","id":1,"result":[0,{}]}"#).await;

        let result = client.uci_commit("wireless").await;
        assert!(result.is_ok(), "uci_commit should succeed: {result:?}");
    }

    #[tokio::test]
    async fn test_service_action_success() {
        let (client, _server, _mock) =
            mock_ubus(r#"{"jsonrpc":"2.0","id":1,"result":[0,{}]}"#).await;

        let result = client.service_action("network", "restart").await;
        assert!(result.is_ok(), "service_action should succeed: {result:?}");
    }

    #[tokio::test]
    async fn test_board() {
        let (client, _server, _mock) = mock_ubus(
            r#"{"jsonrpc":"2.0","id":1,"result":[0,{"kernel":"5.15.134","hostname":"OpenWrt","model":"Ubiquiti EdgeRouter X"}]}"#,
        )
        .await;

        let result = client.board().await;
        assert!(result.is_ok(), "board should succeed: {result:?}");

        let board = result.unwrap();
        assert_eq!(board["hostname"], "OpenWrt");
        assert_eq!(board["model"], "Ubiquiti EdgeRouter X");
    }

    #[tokio::test]
    async fn test_discover_radios() {
        let (client, _server, _mock) = mock_ubus(
            r#"{"jsonrpc":"2.0","id":1,"result":[0,{"radio0":{".type":"wifi-device","band":"2g","channel":"1"},"radio1":{".type":"wifi-device","band":"5g","channel":"36"}}]}"#,
        )
        .await;

        let result = client.discover_radios().await;
        assert!(result.is_ok(), "discover_radios should succeed: {result:?}");

        let radios = result.unwrap();
        assert_eq!(radios.len(), 2);

        let radio0 = radios
            .iter()
            .find(|r| r.name == "radio0")
            .expect("radio0 should exist");
        assert_eq!(radio0.band, "2g");
        assert_eq!(radio0.channel, "1");

        let radio1 = radios
            .iter()
            .find(|r| r.name == "radio1")
            .expect("radio1 should exist");
        assert_eq!(radio1.band, "5g");
        assert_eq!(radio1.channel, "36");
    }

    #[tokio::test]
    async fn test_find_radio_for_band() {
        let (client, _server, _mock) = mock_ubus(
            r#"{"jsonrpc":"2.0","id":1,"result":[0,{"radio0":{".type":"wifi-device","band":"2g","channel":"1"},"radio1":{".type":"wifi-device","band":"5g","channel":"36"}}]}"#,
        )
        .await;

        let found = client.find_radio_for_band("5g").await;
        assert!(found.is_ok());
        assert_eq!(found.unwrap(), Some("radio1".to_owned()));

        // Second client for "not found" case
        let (client2, _server2, _mock2) = mock_ubus(
            r#"{"jsonrpc":"2.0","id":1,"result":[0,{"radio0":{".type":"wifi-device","band":"2g","channel":"1"}}]}"#,
        )
        .await;

        let not_found = client2.find_radio_for_band("6g").await;
        assert!(not_found.is_ok());
        assert_eq!(not_found.unwrap(), None);
    }

    #[tokio::test]
    async fn test_not_authenticated_error() {
        let client = unauthenticated_client();
        assert!(client.token.is_none());

        let result = client.uci_get("wireless", "radio0").await;
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not authenticated"),
            "expected auth error, got: {err_msg}"
        );
    }

    #[tokio::test]
    async fn test_ubus_call_failed() {
        let (client, _server, _mock) =
            mock_ubus(r#"{"jsonrpc":"2.0","id":1,"result":[6,{"message":"Access denied"}]}"#).await;

        let result = client.uci_get("wireless", "radio0").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_json_rpc_level_error() {
        let (client, _server, _mock) = mock_ubus(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"Server error"}}"#,
        )
        .await;

        let result = client.uci_get("wireless", "radio0").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_execute_ubus_ops() {
        let (client, _server, _mock) =
            mock_ubus(r#"{"jsonrpc":"2.0","id":1,"result":[0,{}]}"#).await;

        let ops = vec![
            UciOp::Set {
                config: "wireless".to_owned(),
                section: "radio0".to_owned(),
                values: vec![("disabled".to_owned(), OpValue::Single("0".to_owned()))],
            },
            UciOp::Commit {
                config: "wireless".to_owned(),
            },
            UciOp::Shell {
                command: "wifi reload".to_owned(),
            },
            UciOp::Comment {
                text: "done".to_owned(),
            },
        ];

        let results = execute_ubus(&client, &ops).await;
        assert_eq!(results.len(), 4);
        assert!(results[0].is_ok(), "Set should succeed: {:?}", results[0]);
        assert!(
            results[1].is_ok(),
            "Commit should succeed: {:?}",
            results[1]
        );
        assert!(
            results[2].is_ok(),
            "Shell should be skipped (Ok): {:?}",
            results[2]
        );
        assert!(
            results[3].is_ok(),
            "Comment should be skipped (Ok): {:?}",
            results[3]
        );
    }
}
