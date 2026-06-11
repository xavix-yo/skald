pub mod config;
pub mod http_server;
pub mod server;

use async_trait::async_trait;
use serde_json::{Value, json};

pub use config::{McpServerConfig, McpTransport};
pub use server::McpNotification;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct McpTool {
    pub server_name:  String,
    pub name:         String,
    pub description:  String,
    pub input_schema: Value,
}

impl McpTool {
    pub fn tool_id(&self) -> String {
        format!("mcp__{}__{}", self.server_name, self.name)
    }

    pub fn to_openai_definition(&self) -> Value {
        let params = if self.input_schema.is_object() {
            self.input_schema.clone()
        } else {
            json!({ "type": "object", "properties": {} })
        };
        json!({
            "type": "function",
            "function": {
                "name":        self.tool_id(),
                "description": format!("[{}] {}", self.server_name, self.description),
                "parameters":  params,
            }
        })
    }
}

/// Status of a configured MCP server.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpServerStatus {
    Running { tools: Vec<String> },
    Error   { message: String },
    Disabled,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServerInfo {
    pub name:      String,
    pub transport: String,
    pub description:   Option<String>,
    pub friendly_name: Option<String>,
    #[serde(flatten)]
    pub status:    McpServerStatus,
}

// ── Transport trait ───────────────────────────────────────────────────────────

#[async_trait]
pub trait McpServerClient: Send + Sync {
    fn tools(&self) -> &[McpTool];
    async fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<String>;
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parses `mcp__<server>__<tool>` → `(server, tool)`.
pub fn parse_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let sep = rest.find("__")?;
    Some((&rest[..sep], &rest[sep + 2..]))
}

/// Extracts text content from an MCP tool result value.
pub(crate) fn extract_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Interpolates `${VAR}` references in a string from the process environment.
pub(crate) fn interpolate_env(s: &str) -> String {
    let mut result = s.to_string();
    loop {
        let Some(start) = result.find("${") else { break };
        let Some(rel_end) = result[start..].find('}') else { break };
        let var_name = result[start + 2..start + rel_end].to_string();
        let value = std::env::var(&var_name).unwrap_or_else(|_| {
            tracing::warn!("MCP env var ${{{var_name}}} not set");
            String::new()
        });
        result = format!("{}{}{}", &result[..start], value, &result[start + rel_end + 1..]);
    }
    result
}
