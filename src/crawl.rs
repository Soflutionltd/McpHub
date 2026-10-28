/// Built-in Cloudflare Browser Rendering /crawl tool.
/// Crawls a URL and returns content in HTML, Markdown, or JSON.
use serde_json::Value;

#[allow(dead_code)]
const CRAWL_TOOL_NAME: &str = "crawl";

/// Returns the tool definition for the crawl tool.
#[allow(dead_code)]
pub fn crawl_tool_def() -> Value {
    serde_json::json!({
        "name": CRAWL_TOOL_NAME,
        "description": "Crawl a website using Cloudflare Browser Rendering. Returns the full page content in your preferred format (markdown, html, or json). One API call, entire site crawled. No scripts, no browser management.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to crawl (e.g. https://example.com)"
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "html", "json"],
                    "description": "Output format (default: markdown)",
                    "default": "markdown"
                }
            },
            "required": ["url"]
        }
    })
}

/// Execute the crawl tool. Calls Cloudflare Browser Rendering API.
pub async fn execute_crawl(args: Value) -> Result<String, String> {
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("Missing required 'url' argument")?;

    let format = args
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("markdown");

    // Read Cloudflare credentials from environment
    let account_id = std::env::var("CLOUDFLARE_ACCOUNT_ID")
        .map_err(|_| "CLOUDFLARE_ACCOUNT_ID env var not set. Set it in your McpHub config.")?;
    let api_token = std::env::var("CLOUDFLARE_API_TOKEN")
        .map_err(|_| "CLOUDFLARE_API_TOKEN env var not set. Set it in your McpHub config.")?;

    let endpoint = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/browser-rendering/crawl",
        account_id
    );

    let body = serde_json::json!({
        "url": url,
        "outputFormat": format
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&endpoint)
        .header("Authorization", format!("Bearer {}", api_token))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("HTTP request failed: {}", e))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))?;

    if !status.is_success() {
        return Err(format!("Cloudflare API error ({}): {}", status, text));
    }

    Ok(text)
}
