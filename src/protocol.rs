//! MCP Protocol types - JSON-RPC 2.0 messages for Model Context Protocol.
//! We implement the protocol directly (no SDK) for smallest binary and fastest startup.

use serde::{Deserialize, Serialize};
use serde_json::Value;

const SAFE_INLINE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/jpg", "image/gif"];
const CURSOR_RICH_INLINE_MIME_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/gif",
    "image/webp",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaProfile {
    Safe,
    CursorRich,
    Raw,
}

impl MediaProfile {
    pub fn from_str(value: &str) -> Self {
        let normalized = value.trim().to_lowercase();
        match normalized.as_str() {
            "cursor-rich" | "cursor_rich" | "cursorrich" => Self::CursorRich,
            "raw" => Self::Raw,
            _ => Self::Safe,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::CursorRich => "cursor-rich",
            Self::Raw => "raw",
        }
    }
}

// ─── JSON-RPC 2.0 Base Types ─────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    /// Optional structured payload (per JSON-RPC 2.0 spec). McpHub uses this to attach
    /// the failed tool's inputSchema, args received, and remediation hints so the caller
    /// can recover without an extra round trip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcResponse {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<Value>, code: i64, message: String) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError { code, message, data: None }),
        }
    }

    /// Like `error`, but attaches a structured `data` payload (inputSchema, suggestions, etc.)
    /// so the caller can fix the request without an extra round trip.
    pub fn error_with_data(id: Option<Value>, code: i64, message: String, data: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError { code, message, data: Some(data) }),
        }
    }

    #[allow(dead_code)]
    pub fn notification(method: &str, params: Value) -> String {
        serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .unwrap()
    }
}

pub fn normalize_child_result(result: Value, media_profile: &MediaProfile) -> Value {
    match result {
        Value::Object(mut object) => {
            if let Some(content) = object
                .get_mut("content")
                .and_then(|value| value.as_array_mut())
            {
                for item in content {
                    normalize_content_item(item, media_profile);
                }
            }
            Value::Object(object)
        }
        other => other,
    }
}

fn normalize_content_item(item: &mut Value, media_profile: &MediaProfile) {
    let Value::Object(object) = item else {
        return;
    };

    let Some(data) = object.get("data").and_then(|value| value.as_str()) else {
        return;
    };

    let content_type = object
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("binary");
    let mime_type = object.get("mimeType").and_then(|value| value.as_str());

    if !should_normalize_inline_block(content_type, mime_type, media_profile) {
        return;
    }

    let mime_label = mime_type.unwrap_or("unknown");
    let summary = format!(
        "McpHub normalized inline {content_type} content with mime type `{mime_label}` because some MCP clients cannot decode it safely. Original payload length: {data_len} base64 chars. Use any file path or URI mentioned elsewhere in the tool output when available.",
        data_len = data.len()
    );
    *item = serde_json::json!({
        "type": "text",
        "text": summary,
    });
}

fn should_normalize_inline_block(
    content_type: &str,
    mime_type: Option<&str>,
    media_profile: &MediaProfile,
) -> bool {
    if *media_profile == MediaProfile::Raw {
        return false;
    }

    if content_type == "text" {
        return false;
    }

    match mime_type {
        Some(mime_type) => !is_inline_mime_type_allowed(mime_type, media_profile),
        None => true,
    }
}

fn is_inline_mime_type_allowed(mime_type: &str, media_profile: &MediaProfile) -> bool {
    let allowlist = match media_profile {
        MediaProfile::Safe => SAFE_INLINE_MIME_TYPES,
        MediaProfile::CursorRich => CURSOR_RICH_INLINE_MIME_TYPES,
        MediaProfile::Raw => return true,
    };

    allowlist
        .iter()
        .any(|safe_mime_type| safe_mime_type.eq_ignore_ascii_case(mime_type))
}

// ─── MCP Tool Types ──────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
    /// Titre lisible (MCP 2025-06-18), relayé tel quel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Annotations (readOnlyHint, destructiveHint, idempotentHint, openWorldHint...).
    /// Auparavant perdues à la désérialisation : elles servent maintenant aux permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    /// Schéma de sortie (structuredContent), relayé tel quel.
    #[serde(rename = "outputSchema", default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

impl ToolDef {
    fn hint(&self, key: &str) -> Option<bool> {
        self.annotations.as_ref()?.get(key)?.as_bool()
    }
    /// Le serveur déclare explicitement l'outil en lecture seule.
    pub fn read_only_hint(&self) -> Option<bool> {
        self.hint("readOnlyHint")
    }
    /// Le serveur déclare explicitement l'outil destructeur (et pas en lecture seule).
    pub fn declared_destructive(&self) -> bool {
        self.hint("destructiveHint") == Some(true) && self.read_only_hint() != Some(true)
    }
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct ToolContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

#[allow(dead_code)]
impl ToolContent {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content_type: "text".into(),
            text: s.into(),
        }
    }
}

// ─── MCP Initialize Types ────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Serialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: Capabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Capabilities {
    pub tools: ToolsCapability,
}

#[derive(Debug, Serialize)]
pub struct ToolsCapability {
    #[serde(rename = "listChanged")]
    pub list_changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_jsonrpc_request_parsing() {
        let req_str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let req: JsonRpcRequest = serde_json::from_str(req_str).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, Some(json!(1)));
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.params, json!(null));
    }

    #[test]
    fn test_jsonrpc_response_success() {
        let resp = JsonRpcResponse::success(Some(json!(1)), json!({"status": "ok"}));
        let resp_str = serde_json::to_string(&resp).unwrap();
        assert!(resp_str.contains(r#""jsonrpc":"2.0""#));
        assert!(resp_str.contains(r#""id":1"#));
        assert!(resp_str.contains(r#""result":{"status":"ok"}"#));
        assert!(!resp_str.contains("error"));
    }

    #[test]
    fn test_jsonrpc_response_error() {
        let resp = JsonRpcResponse::error(Some(json!(2)), -32601, "Method not found".to_string());
        let resp_str = serde_json::to_string(&resp).unwrap();
        assert!(resp_str.contains(r#""jsonrpc":"2.0""#));
        assert!(resp_str.contains(r#""id":2"#));
        assert!(resp_str.contains(r#""error":{"code":-32601,"message":"Method not found"}"#));
        assert!(!resp_str.contains("result"));
    }

    #[test]
    fn test_capabilities_serialization() {
        let init_result = InitializeResult {
            protocol_version: "2024-11-05".to_string(),
            capabilities: Capabilities {
                tools: ToolsCapability { list_changed: true },
            },
            server_info: ServerInfo {
                name: "McpHub".to_string(),
                version: "2.0.0".to_string(),
            },
            instructions: None,
        };

        let result_str = serde_json::to_string(&init_result).unwrap();
        assert!(result_str.contains(r#""capabilities":{"tools":{"listChanged":true}}"#));
        assert!(!result_str.contains("prompts"));
        assert!(!result_str.contains("resources"));
        assert!(result_str.contains(r#""serverInfo":{"name":"McpHub","version":"2.0.0"}"#));
    }

    #[test]
    fn test_normalize_child_result_keeps_safe_png_inline_images() {
        let result = json!({
            "content": [
                { "type": "image", "mimeType": "image/png", "data": "abcd" }
            ]
        });

        let normalized = normalize_child_result(result, &MediaProfile::Safe);
        assert_eq!(
            normalized["content"][0]["mimeType"].as_str(),
            Some("image/png")
        );
        assert_eq!(normalized["content"][0]["data"].as_str(), Some("abcd"));
    }

    #[test]
    fn test_normalize_child_result_rewrites_unsupported_inline_images() {
        let result = json!({
            "content": [
                { "type": "image", "mimeType": "image/webp", "data": "abcd" }
            ]
        });

        let normalized = normalize_child_result(result, &MediaProfile::Safe);
        assert_eq!(normalized["content"][0]["type"].as_str(), Some("text"));
        assert!(normalized["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("image/webp"));
    }

    #[test]
    fn test_normalize_child_result_keeps_webp_in_cursor_rich_profile() {
        let result = json!({
            "content": [
                { "type": "image", "mimeType": "image/webp", "data": "abcd" }
            ]
        });

        let normalized = normalize_child_result(result, &MediaProfile::CursorRich);
        assert_eq!(
            normalized["content"][0]["mimeType"].as_str(),
            Some("image/webp")
        );
        assert_eq!(normalized["content"][0]["data"].as_str(), Some("abcd"));
    }

    #[test]
    fn test_media_profile_from_str() {
        assert_eq!(MediaProfile::from_str("safe"), MediaProfile::Safe);
        assert_eq!(
            MediaProfile::from_str("cursor-rich"),
            MediaProfile::CursorRich
        );
        assert_eq!(MediaProfile::from_str("raw"), MediaProfile::Raw);
        assert_eq!(MediaProfile::from_str("unknown"), MediaProfile::Safe);
    }
}
