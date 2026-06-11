use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::core::mcp::McpManager;
use crate::core::tools::Tool;

pub struct ListMcp {
    mcp: Arc<McpManager>,
}

impl ListMcp {
    pub fn new(mcp: Arc<McpManager>) -> Self { Self { mcp } }
}

impl Tool for ListMcp {
    fn name(&self) -> &str { "list_mcp" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Introspection }

    fn description(&self) -> &str {
        "List all configured MCP servers with their status (running, error, disabled), \
         description, friendly_name (if set), and the tools they expose. \
         Use this to discover available MCP integrations."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn execute(&self, _args: Value) -> Result<String> {
        let infos = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.mcp.list())
        })?;
        Ok(serde_json::to_string_pretty(&infos)?)
    }
}
