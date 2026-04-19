//! A2A v0.3 compatibility layer for a2a-sdk 0.3.x clients.
//!
//! turul-a2a is canonically A2A v1.0 (proto-defined PascalCase methods,
//! `/message:send` HTTP paths, `A2A-Version: 1.0` header).
//!
//! This module provides narrowly scoped, **conditional** compatibility for
//! a2a-sdk 0.3.x clients (used by Strands Agents SDK) which speak A2A v0.3.
//!
//! ## Detection
//!
//! `CompatMode` is determined **once** at the transport boundary per request:
//!
//! - `V10` (default): request is treated as canonical v1.0
//! - `V03`: request is positively identified as v0.3-era
//!
//! Detection signals:
//! - `A2A-Version: 1.0` header present → V10 (strict)
//! - JSONRPC method uses slash form (`message/send`) → V03
//! - Method is PascalCase + no version header → V10 (canonical default)
//!
//! ## What activates conditionally (only in V03 mode)
//!
//! - JSONRPC method name normalization (`"message/send"` → `"SendMessage"`)
//!
//! ## What is unconditionally additive (safe for both versions)
//!
//! - Agent card: additive v0.3 fields (`url`, `protocolVersion`, `additionalInterfaces`)
//! - `parse_first_data_or_text()`: tolerant input parsing (data preferred, text fallback)
//! - Dual-part response artifacts: data + text parts
//!
//! These are safe because they do not alter valid v1.0 behavior.
//!
//! ## Removal condition
//!
//! When a2a-sdk ships v1.0 support (tracks
//! <https://github.com/google/a2a-sdk-python>), this module can be removed
//! and the v0.3 route alias in `router.rs` deleted.
//!
//! ## Scope
//!
//! Transport layer only. No compat logic leaks into executors, skill handlers,
//! or business code.

use turul_a2a_types::wire::jsonrpc as methods;

// ---------------------------------------------------------------------------
// CompatMode detection
// ---------------------------------------------------------------------------

/// Detected A2A protocol compatibility mode for a single request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatMode {
    /// Canonical A2A v1.0 — no normalization applied.
    V10,
    /// A2A v0.3 compatibility — method names normalized, logging emitted.
    V03,
}

/// Detect compat mode from the JSONRPC method name and HTTP headers.
///
/// Called once per JSONRPC request at the transport boundary.
///
/// Detection logic:
/// 1. If `A2A-Version: 1.0` header is present → V10 (strict, no compat)
/// 2. If method name is slash-separated (contains `/`) → V03
/// 3. Otherwise → V10 (canonical default; ambiguous prefers v1.0)
pub fn detect_compat_mode(method: &str, headers: &http::HeaderMap) -> CompatMode {
    // Explicit v1.0 header takes priority — never enter compat mode
    if let Some(version) = headers.get("a2a-version").and_then(|v| v.to_str().ok()) {
        if version == "1.0" {
            return CompatMode::V10;
        }
    }

    // Slash in method name is positive v0.3 identification
    if method.contains('/') {
        return CompatMode::V03;
    }

    // Default: canonical v1.0
    CompatMode::V10
}

/// Route-based implicit detection: `POST /` is only reachable by v0.3 clients.
///
/// v1.0 clients POST to `/message:send`, `/message:stream`, etc.
/// Arriving at the root path IS the detection signal.
pub const fn root_post_compat_mode() -> CompatMode {
    CompatMode::V03
}

// ---------------------------------------------------------------------------
// Conditional method normalization
// ---------------------------------------------------------------------------

/// Normalize the JSONRPC method name, but only if compat mode is V03.
///
/// In V10 mode, the method passes through unchanged — if it's not a valid
/// v1.0 method, the dispatcher will return MethodNotFound clearly.
///
/// In V03 mode, slash-separated names are mapped to PascalCase equivalents.
pub fn maybe_normalize_method(method: &str, mode: CompatMode) -> String {
    match mode {
        CompatMode::V10 => method.to_string(),
        CompatMode::V03 => normalize_jsonrpc_method(method),
    }
}

/// Map a2a-sdk v0.3 slash-separated JSONRPC method names to v1.0 PascalCase.
///
/// v1.0 (canonical): `"SendMessage"`, `"GetTask"`, etc.
/// v0.3 (a2a-sdk 0.3.x): `"message/send"`, `"tasks/get"`, etc.
///
/// PascalCase names pass through unchanged.
fn normalize_jsonrpc_method(method: &str) -> String {
    match method {
        "message/send" => methods::SEND_MESSAGE.to_string(),
        "message/stream" => methods::SEND_STREAMING_MESSAGE.to_string(),
        "tasks/get" => methods::GET_TASK.to_string(),
        "tasks/list" => methods::LIST_TASKS.to_string(),
        "tasks/cancel" => methods::CANCEL_TASK.to_string(),
        "tasks/subscribe" => methods::SUBSCRIBE_TO_TASK.to_string(),
        "tasks/pushNotificationConfig/set" => {
            methods::CREATE_TASK_PUSH_NOTIFICATION_CONFIG.to_string()
        }
        "tasks/pushNotificationConfig/get" => {
            methods::GET_TASK_PUSH_NOTIFICATION_CONFIG.to_string()
        }
        "tasks/pushNotificationConfig/list" => {
            methods::LIST_TASK_PUSH_NOTIFICATION_CONFIGS.to_string()
        }
        "tasks/pushNotificationConfig/delete" => {
            methods::DELETE_TASK_PUSH_NOTIFICATION_CONFIG.to_string()
        }
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Conditional params normalization (V03 only)
// ---------------------------------------------------------------------------

/// Normalize v0.3 message params to v1.0 proto wire format.
///
/// Fixes known v0.3 → v1.0 enum differences in the JSONRPC params:
/// - `"role": "user"` → `"role": "ROLE_USER"`
/// - `"role": "agent"` → `"role": "ROLE_AGENT"`
/// - part `"kind": "text"` / `"kind": "data"` → stripped (proto uses field presence)
///
/// Applied only when compat mode is V03. Does not modify V10 params.
pub fn maybe_normalize_params(params: &mut serde_json::Value, mode: CompatMode) {
    if mode != CompatMode::V03 {
        return;
    }
    normalize_roles(params);
}

/// Recursively normalize role enum values from v0.3 lowercase to v1.0 proto names.
fn normalize_roles(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(role) = map.get_mut("role") {
                if let Some(s) = role.as_str() {
                    let normalized = match s {
                        "user" => "ROLE_USER",
                        "agent" => "ROLE_AGENT",
                        other => other,
                    };
                    *role = serde_json::Value::String(normalized.to_string());
                }
            }
            for (_, v) in map.iter_mut() {
                normalize_roles(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                normalize_roles(v);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Conditional response normalization (V03 only)
// ---------------------------------------------------------------------------

/// Normalize a v1.0 JSONRPC success response to v0.3 shape.
///
/// v1.0 (proto): `{"result": {"task": {... "status": {"state": "TASK_STATE_COMPLETED"} ...}}}`
/// v0.3 (a2a-sdk): `{"result": {"id": "...", "status": {"state": "completed"}, ...}}`
///
/// Transformations:
/// - Unwrap `result.task` → `result` (flatten)
/// - `TASK_STATE_COMPLETED` → `completed`, etc.
/// - `ROLE_USER` → `user`, `ROLE_AGENT` → `agent`
pub fn maybe_normalize_response(response: &mut serde_json::Value, mode: CompatMode) {
    if mode != CompatMode::V03 {
        return;
    }
    if let Some(result) = response.get_mut("result") {
        // Unwrap result.task → result
        if let Some(task) = result.get("task").cloned() {
            *result = task;
        }
        // Normalize enum values (proto → v0.3 lowercase)
        normalize_task_state_enums(result);
        denormalize_roles(result);
    }
}

/// Convert proto role enum names back to v0.3 lowercase (for responses).
/// Reverse of normalize_roles which converts v0.3 → proto (for requests).
fn denormalize_roles(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(role) = map.get_mut("role") {
                if let Some(s) = role.as_str() {
                    let denormalized = match s {
                        "ROLE_USER" => "user",
                        "ROLE_AGENT" => "agent",
                        other => other,
                    };
                    *role = serde_json::Value::String(denormalized.to_string());
                }
            }
            for (_, v) in map.iter_mut() {
                denormalize_roles(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                denormalize_roles(v);
            }
        }
        _ => {}
    }
}

/// Convert proto task state enums to v0.3 lowercase names.
fn normalize_task_state_enums(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(state) = map.get_mut("state") {
                if let Some(s) = state.as_str() {
                    let normalized = match s {
                        "TASK_STATE_SUBMITTED" => "submitted",
                        "TASK_STATE_WORKING" => "working",
                        "TASK_STATE_COMPLETED" => "completed",
                        "TASK_STATE_CANCELED" => "canceled",
                        "TASK_STATE_FAILED" => "failed",
                        "TASK_STATE_INPUT_REQUIRED" => "input-required",
                        other => other,
                    };
                    *state = serde_json::Value::String(normalized.to_string());
                }
            }
            for (_, v) in map.iter_mut() {
                normalize_task_state_enums(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                normalize_task_state_enums(v);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Unconditionally additive: agent card fields
// ---------------------------------------------------------------------------

/// Inject v0.3 fields into a serialized agent card for a2a-sdk 0.3.x compatibility.
///
/// Adds `url`, `protocolVersion`, and `additionalInterfaces` derived from the
/// canonical `supportedInterfaces`. Additive only — no v1.0 fields are removed.
///
/// Applied unconditionally because it is a discovery endpoint that must work
/// for all client versions, and the added fields do not alter v1.0 semantics.
pub fn inject_agent_card_compat(mut card: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = card.as_object_mut() {
        let interfaces = obj
            .get("supportedInterfaces")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        if let Some(primary) = interfaces.first() {
            if !obj.contains_key("url") {
                if let Some(url) = primary.get("url") {
                    obj.insert("url".to_string(), url.clone());
                }
            }

            let additional: Vec<serde_json::Value> = interfaces
                .iter()
                .map(|iface| {
                    let mut ai = serde_json::Map::new();
                    if let Some(url) = iface.get("url") {
                        ai.insert("url".to_string(), url.clone());
                    }
                    // v0.3 renames protocolBinding → transport
                    if let Some(binding) = iface.get("protocolBinding") {
                        ai.insert("transport".to_string(), binding.clone());
                    }
                    serde_json::Value::Object(ai)
                })
                .collect();

            if !obj.contains_key("additionalInterfaces") {
                obj.insert(
                    "additionalInterfaces".to_string(),
                    serde_json::Value::Array(additional),
                );
            }
        }

        if !obj.contains_key("protocolVersion") {
            obj.insert(
                "protocolVersion".to_string(),
                serde_json::Value::String("0.3.0".to_string()),
            );
        }
    }
    card
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    // -- CompatMode detection -----------------------------------------------

    #[test]
    fn v03_compat_detect_v10_with_header() {
        let mut headers = HeaderMap::new();
        headers.insert("a2a-version", "1.0".parse().unwrap());
        assert_eq!(
            detect_compat_mode("message/send", &headers),
            CompatMode::V10,
            "Explicit A2A-Version: 1.0 forces V10 even with slash method"
        );
    }

    #[test]
    fn v03_compat_detect_v03_by_method() {
        let headers = HeaderMap::new();
        assert_eq!(
            detect_compat_mode("message/send", &headers),
            CompatMode::V03
        );
        assert_eq!(detect_compat_mode("tasks/get", &headers), CompatMode::V03);
    }

    #[test]
    fn v03_compat_detect_v10_default_for_pascal_case() {
        let headers = HeaderMap::new();
        assert_eq!(detect_compat_mode("SendMessage", &headers), CompatMode::V10);
        assert_eq!(detect_compat_mode("GetTask", &headers), CompatMode::V10);
    }

    #[test]
    fn v03_compat_detect_v10_for_unknown_method() {
        let headers = HeaderMap::new();
        assert_eq!(
            detect_compat_mode("SomethingNew", &headers),
            CompatMode::V10,
            "Unknown methods default to V10 — let dispatcher reject clearly"
        );
    }

    // -- Conditional method normalization -----------------------------------

    #[test]
    fn v03_compat_normalize_only_in_v03_mode() {
        assert_eq!(
            maybe_normalize_method("message/send", CompatMode::V03),
            "SendMessage"
        );
        assert_eq!(
            maybe_normalize_method("message/send", CompatMode::V10),
            "message/send",
            "V10 mode must NOT normalize — dispatcher rejects unknown methods"
        );
    }

    #[test]
    fn v03_compat_v10_methods_pass_through_in_both_modes() {
        assert_eq!(
            maybe_normalize_method("SendMessage", CompatMode::V10),
            "SendMessage"
        );
        assert_eq!(
            maybe_normalize_method("SendMessage", CompatMode::V03),
            "SendMessage"
        );
    }

    // -- Agent card (unconditionally additive) ------------------------------

    #[test]
    fn v03_compat_agent_card_injects_url() {
        let card = serde_json::json!({
            "name": "Test",
            "supportedInterfaces": [{
                "url": "https://example.com/agent",
                "protocolBinding": "JSONRPC",
                "protocolVersion": "1.0"
            }]
        });
        let result = inject_agent_card_compat(card);
        assert_eq!(result["url"], "https://example.com/agent");
        assert_eq!(result["protocolVersion"], "0.3.0");
        assert!(result["additionalInterfaces"].is_array());
        assert_eq!(result["additionalInterfaces"][0]["transport"], "JSONRPC");
        // v1.0 field preserved
        assert!(result["supportedInterfaces"].is_array());
    }

    #[test]
    fn v03_compat_agent_card_does_not_overwrite_existing_fields() {
        let card = serde_json::json!({
            "name": "Test",
            "url": "https://custom.example.com",
            "protocolVersion": "1.0",
            "supportedInterfaces": [{
                "url": "https://example.com/agent",
                "protocolBinding": "JSONRPC"
            }]
        });
        let result = inject_agent_card_compat(card);
        assert_eq!(result["url"], "https://custom.example.com");
        assert_eq!(result["protocolVersion"], "1.0");
    }

    // -- Params normalization (V03 only) ------------------------------------

    #[test]
    fn v03_compat_role_normalization() {
        let mut params = serde_json::json!({
            "message": {
                "messageId": "test",
                "role": "user",
                "parts": [{"kind": "text", "text": "hello"}]
            }
        });
        maybe_normalize_params(&mut params, CompatMode::V03);
        assert_eq!(params["message"]["role"], "ROLE_USER");
    }

    #[test]
    fn v03_compat_role_not_normalized_in_v10_mode() {
        let mut params = serde_json::json!({
            "message": {
                "messageId": "test",
                "role": "user",
                "parts": []
            }
        });
        maybe_normalize_params(&mut params, CompatMode::V10);
        assert_eq!(
            params["message"]["role"], "user",
            "V10 mode must NOT normalize — let proto reject invalid enums"
        );
    }
}
