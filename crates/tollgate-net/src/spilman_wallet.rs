//! Spilman channel keyset utilities.
//!
//! Provides [`fetch_active_keyset_info`] for fetching the active sat keyset
//! from a Cashu mint, used during channel setup. The parsing logic is split
//! into pure helpers ([`select_active_sat_keyset`], [`assemble_keyset_info`])
//! so it can be unit-tested without network access.

use std::time::Duration;

use cdk_spilman::{parse_keyset_info_from_json, KeysetInfo};
use serde_json::Value;

use crate::spilman_service::SpilmanError;

/// Select the active `sat` keyset from a `/v1/keysets` response body.
///
/// Returns `(keyset_id, input_fee_ppk)`.
///
/// # Errors
///
/// Returns [`SpilmanError::InvalidResponse`] if the body is not valid JSON or has
/// no `keysets` array, and [`SpilmanError::Keyset`] if no active sat keyset is
/// present or it is missing its id/fee fields.
pub fn select_active_sat_keyset(keysets_body: &str) -> Result<(String, u64), SpilmanError> {
    let body: Value = serde_json::from_str(keysets_body)?;

    let keysets = body["keysets"]
        .as_array()
        .ok_or_else(|| SpilmanError::InvalidResponse("missing keysets array".to_string()))?;

    let active_sat = keysets
        .iter()
        .find(|ks| ks["unit"].as_str() == Some("sat") && ks["active"].as_bool() == Some(true))
        .ok_or_else(|| SpilmanError::Keyset("no active sat keyset".to_string()))?;

    let keyset_id = active_sat["id"]
        .as_str()
        .ok_or_else(|| SpilmanError::Keyset("missing keyset id".to_string()))?
        .to_owned();

    let input_fee_ppk = active_sat["input_fee_ppk"]
        .as_u64()
        .or_else(|| active_sat["inputFeePpk"].as_u64())
        .unwrap_or(0);

    Ok((keyset_id, input_fee_ppk))
}

/// Assemble a `KeysetInfo` (and its JSON form) from a keyset id, its fee, and the
/// `/v1/keys/{id}` response body.
///
/// # Errors
///
/// Returns [`SpilmanError::InvalidResponse`] if the keys body is malformed or has
/// no keyset entry, or [`SpilmanError::Keyset`] if `cdk_spilman` rejects the
/// assembled keyset info JSON.
pub fn assemble_keyset_info(
    keyset_id: &str,
    input_fee_ppk: u64,
    keys_body: &str,
) -> Result<(String, KeysetInfo), SpilmanError> {
    let keys_body_val: Value = serde_json::from_str(keys_body)?;

    let keyset_data = keys_body_val["keysets"]
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| {
            SpilmanError::InvalidResponse("missing keyset in keys response".to_string())
        })?;

    let keyset_info_json = serde_json::json!({
        "keysetId": keyset_id,
        "unit": "sat",
        "keys": keyset_data["keys"],
        "inputFeePpk": input_fee_ppk
    })
    .to_string();

    let keyset_info = parse_keyset_info_from_json(&keyset_info_json).map_err(SpilmanError::from)?;
    Ok((keyset_info_json, keyset_info))
}

/// Fetch the active sat keyset from the given mint URL.
///
/// Returns both the raw keyset JSON string and the parsed [`KeysetInfo`].
///
/// # Errors
///
/// Returns [`SpilmanError::Network`] if the mint is unreachable,
/// [`SpilmanError::MintStatus`] if it returns a non-success HTTP status, or a
/// parse/keyset error if the response is malformed or has no active sat keyset.
#[allow(clippy::missing_errors_doc)]
pub async fn fetch_active_keyset_info(
    mint_url: &str,
) -> Result<(String, KeysetInfo), SpilmanError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    // 1. /v1/keysets → pick the active sat keyset.
    let resp = client.get(format!("{mint_url}/v1/keysets")).send().await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(SpilmanError::MintStatus { status, body });
    }
    let keysets_body = resp.text().await?;
    let (keyset_id, input_fee_ppk) = select_active_sat_keyset(&keysets_body)?;

    // 2. /v1/keys/{id} → assemble KeysetInfo.
    let resp = client
        .get(format!("{mint_url}/v1/keys/{keyset_id}"))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        return Err(SpilmanError::MintStatus { status, body });
    }
    let keys_body = resp.text().await?;

    assemble_keyset_info(&keyset_id, input_fee_ppk, &keys_body)
}

// ---------------------------------------------------------------------------
// Unit tests (no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const KEYSETS_BODY: &str = r#"{
        "keysets": [
            {"id": "deadbeef", "unit": "msat", "active": true},
            {"id": "00keysetid00", "unit": "sat", "active": true, "input_fee_ppk": 400},
            {"id": "inactivekeyset", "unit": "sat", "active": false}
        ]
    }"#;

    #[test]
    fn select_active_sat_keyset_picks_the_active_sat_entry() {
        let (id, fee) = select_active_sat_keyset(KEYSETS_BODY).expect("select keyset");
        assert_eq!(id, "00keysetid00");
        assert_eq!(fee, 400);
    }

    #[test]
    fn select_active_sat_keyset_reads_camel_case_fee() {
        let body = r#"{"keysets":[{"id":"abc","unit":"sat","active":true,"inputFeePpk":50}]}"#;
        let (_, fee) = select_active_sat_keyset(body).expect("select keyset");
        assert_eq!(fee, 50);
    }

    #[test]
    fn select_active_sat_keyset_defaults_fee_to_zero() {
        let body = r#"{"keysets":[{"id":"abc","unit":"sat","active":true}]}"#;
        let (_, fee) = select_active_sat_keyset(body).expect("select keyset");
        assert_eq!(fee, 0);
    }

    #[test]
    fn select_active_sat_keyset_errors_when_no_active_sat() {
        let body = r#"{"keysets":[{"id":"x","unit":"sat","active":false}]}"#;
        let err = select_active_sat_keyset(body).expect_err("should error");
        assert!(matches!(err, SpilmanError::Keyset(_)), "{err:?}");
    }

    #[test]
    fn select_active_sat_keyset_errors_on_missing_array() {
        let err = select_active_sat_keyset("{}").expect_err("should error");
        assert!(matches!(err, SpilmanError::InvalidResponse(_)), "{err:?}");
    }

    #[test]
    fn assemble_keyset_info_builds_json_and_parses() {
        // Use a real pubkey + a keyset id derived from those keys so the JSON
        // round-trips through cdk_spilman's parser (which validates pubkeys).
        use std::collections::BTreeMap;
        use std::str::FromStr;

        use cashu::nuts::{Id, Keys, PublicKey};

        let pk_hex = "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
        let pk = PublicKey::from_str(pk_hex).expect("valid pubkey");
        let mut keys_map = BTreeMap::new();
        keys_map.insert(cashu::Amount::from(1), pk);
        let id = Id::v1_from_keys(&Keys::new(keys_map));
        let id_str = id.to_string();

        let keys_body = format!(
            r#"{{"keysets":[{{"id":"{id_str}","unit":"sat","keys":{{"1":"{pk_hex}"}}}}]}}"#
        );

        let (json, info) = assemble_keyset_info(&id_str, 400, &keys_body).expect("assemble keyset");
        assert!(json.contains(&format!("\"keysetId\":\"{id_str}\"")));
        assert!(json.contains("\"inputFeePpk\":400"));
        assert_eq!(info.input_fee_ppk, 400);
        assert_eq!(info.keyset_id, id);
    }

    #[test]
    fn assemble_keyset_info_errors_on_malformed_keys_body() {
        let err = assemble_keyset_info("x", 0, "not json").expect_err("should error");
        assert!(matches!(err, SpilmanError::Serialization(_)), "{err:?}");
    }

    #[test]
    fn assemble_keyset_info_errors_when_no_keyset_entry() {
        let err = assemble_keyset_info("x", 0, r#"{"keysets":[]}"#).expect_err("should error");
        assert!(matches!(err, SpilmanError::InvalidResponse(_)), "{err:?}");
    }
}
