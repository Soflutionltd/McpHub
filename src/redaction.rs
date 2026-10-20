use serde_json::{Map, Value};

const SENSITIVE_KEYWORDS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "apikey",
    "apikey",
    "auth",
    "authorization",
    "bearer",
    "cookie",
    "session",
    "license",
    "webhook",
    "clientsecret",
];

const SENSITIVE_PREFIXES: &[&str] = &[
    "sk_",
    "rk_",
    "pk_",
    "ghp_",
    "gho_",
    "ghu_",
    "github_pat_",
    "sg.",
    "xoxb-",
    "xoxp-",
    "eyj",
    "aiza",
];

pub fn redact_command_preview(command: &str, args: &[String]) -> String {
    let mut preview_parts = Vec::with_capacity(args.len() + 1);
    preview_parts.push(command.to_string());

    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();

        if let Some((key, value)) = argument.split_once('=') {
            if is_sensitive_key(key) || looks_like_secret_value(value) {
                preview_parts.push(format!("{key}={}", redact_value(value)));
            } else {
                preview_parts.push(argument.to_string());
            }
            index += 1;
            continue;
        }

        if is_sensitive_flag(argument) {
            preview_parts.push(argument.to_string());
            if let Some(next_argument) = args.get(index + 1) {
                preview_parts.push(redact_value(next_argument));
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }

        if looks_like_secret_value(argument) {
            preview_parts.push(redact_value(argument));
        } else {
            preview_parts.push(argument.to_string());
        }

        index += 1;
    }

    preview_parts.join(" ")
}

pub fn redact_text(text: &str) -> String {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    if tokens.is_empty() {
        return text.to_string();
    }

    let mut redacted = Vec::with_capacity(tokens.len());
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];

        if token.eq_ignore_ascii_case("bearer") {
            redacted.push(token.to_string());
            if let Some(next_token) = tokens.get(index + 1) {
                redacted.push(redact_value(next_token));
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }

        if let Some((key, value)) = token.split_once('=') {
            if is_sensitive_key(key) || looks_like_secret_value(value) {
                redacted.push(format!("{key}={}", redact_value(value)));
            } else {
                redacted.push(token.to_string());
            }
            index += 1;
            continue;
        }

        if looks_like_secret_value(token) {
            redacted.push(redact_value(token));
        } else {
            redacted.push(token.to_string());
        }

        index += 1;
    }

    redacted.join(" ")
}

pub fn redact_json_value(value: &Value) -> Value {
    redact_json_value_with_parent(None, value)
}

fn redact_json_value_with_parent(parent_key: Option<&str>, value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut redacted = Map::with_capacity(object.len());
            for (key, value) in object {
                if is_sensitive_key(key) {
                    redacted.insert(key.clone(), Value::String(redact_value_from_json(value)));
                } else {
                    redacted.insert(key.clone(), redact_json_value_with_parent(Some(key), value));
                }
            }
            Value::Object(redacted)
        }
        Value::Array(array) => {
            if parent_key == Some("args") {
                Value::Array(
                    array
                        .iter()
                        .map(|item| match item {
                            Value::String(argument) => Value::String(redact_text(argument)),
                            other => redact_json_value(other),
                        })
                        .collect(),
                )
            } else {
                Value::Array(
                    array
                        .iter()
                        .map(|item| redact_json_value_with_parent(parent_key, item))
                        .collect(),
                )
            }
        }
        Value::String(string_value) => {
            if parent_key.map(is_sensitive_key).unwrap_or(false)
                || (parent_key == Some("args") && looks_like_secret_value(string_value))
            {
                Value::String(redact_value(string_value))
            } else {
                Value::String(string_value.clone())
            }
        }
        other => other.clone(),
    }
}

fn redact_value_from_json(value: &Value) -> String {
    match value {
        Value::String(string_value) => redact_value(string_value),
        Value::Null => "[REDACTED]".to_string(),
        other => format!("[REDACTED:{}]", other.to_string().len()),
    }
}

fn redact_value(value: &str) -> String {
    if value.is_empty() {
        return "[REDACTED]".to_string();
    }

    format!("[REDACTED:{}]", value.len())
}

fn is_sensitive_flag(flag: &str) -> bool {
    let trimmed = flag.trim_start_matches('-');
    is_sensitive_key(trimmed)
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|char| char.is_ascii_alphanumeric())
        .flat_map(|char| char.to_lowercase())
        .collect();

    if normalized.is_empty() {
        return false;
    }

    SENSITIVE_KEYWORDS
        .iter()
        .any(|keyword| normalized.contains(keyword))
}

fn looks_like_secret_value(value: &str) -> bool {
    let trimmed = value.trim_matches(|char: char| {
        matches!(
            char,
            '"' | '\'' | ',' | ';' | ')' | '(' | '[' | ']' | '{' | '}'
        )
    });
    let lower = trimmed.to_lowercase();

    if trimmed.starts_with("Bearer ") {
        return true;
    }

    if SENSITIVE_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return true;
    }

    if trimmed.contains('/')
        || trimmed.contains("://")
        || trimmed.starts_with('@')
        || lower.ends_with("@latest")
        || lower.ends_with(".js")
        || lower.ends_with(".json")
    {
        return false;
    }

    trimmed.len() >= 24
        && !trimmed.contains(' ')
        && trimmed.chars().any(|char| char.is_ascii_digit())
        && trimmed.chars().any(|char| char.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_redact_command_preview_masks_sensitive_assignments() {
        let preview = redact_command_preview(
            "npx",
            &[
                "-y".to_string(),
                "@stripe/mcp".to_string(),
                "--api-key=sk_live_1234567890".to_string(),
            ],
        );

        assert!(preview.contains("--api-key=[REDACTED:"));
        assert!(!preview.contains("sk_live_1234567890"));
    }

    #[test]
    fn test_redact_text_masks_bearer_tokens() {
        let redacted = redact_text("Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9");
        assert!(redacted.contains("Bearer [REDACTED:36]"));
        assert!(!redacted.contains("eyJhbGci"));
    }

    #[test]
    fn test_redact_json_value_masks_env_values() {
        let redacted = redact_json_value(&json!({
            "servers": {
                "stripe": {
                    "args": ["--api-key=sk_live_secret"],
                    "env": {
                        "STRIPE_SECRET_KEY": "sk_live_secret"
                    }
                }
            }
        }));

        assert_eq!(
            redacted["servers"]["stripe"]["env"]["STRIPE_SECRET_KEY"],
            json!("[REDACTED:14]")
        );
        assert_eq!(
            redacted["servers"]["stripe"]["args"][0],
            json!("--api-key=[REDACTED:14]")
        );
    }

    #[test]
    fn test_redaction_keeps_paths_and_package_names_visible() {
        let preview = redact_command_preview(
            "/Users/me/.nvm/bin/npx",
            &[
                "-y".to_string(),
                "@upstash/context7-mcp@latest".to_string(),
                "/Users/me/server/index.js".to_string(),
            ],
        );

        assert!(preview.contains("@upstash/context7-mcp@latest"));
        assert!(preview.contains("/Users/me/server/index.js"));

        let redacted = redact_json_value(&json!({
            "command": "/Users/me/.nvm/bin/npx",
            "args": ["-y", "@upstash/context7-mcp@latest"]
        }));

        assert_eq!(redacted["command"], json!("/Users/me/.nvm/bin/npx"));
        assert_eq!(redacted["args"][1], json!("@upstash/context7-mcp@latest"));
    }
}
