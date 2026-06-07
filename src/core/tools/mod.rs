/// Tools that write or modify files on disk.
/// Used by the approval gate (diff preview logic) and the LLM loop (FileChanged events).
/// Update this list whenever a new file-write tool is added.
pub const FILE_WRITE_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "insert_at_line",
    "replace_lines",
];

/// Returns `true` if `name` is a file-write tool (i.e. it modifies files on disk).
pub fn is_file_write_tool(name: &str) -> bool {
    FILE_WRITE_TOOLS.contains(&name)
}

pub mod tool_names;
pub mod ast_outline;
pub mod configure_plugin;
pub mod cron_jobs;
pub mod exec;
pub mod fs;
pub mod image_generate;
pub mod list_agents;
pub mod list_mcp;
pub mod list_plugins;
pub mod list_secrets;
pub mod notify;
pub mod set_secret;
pub mod read_notification;
pub mod register_mcp;
pub mod restart;
pub mod show_mcp_tools;
pub mod toggle_mcp;
pub mod toggle_plugin;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

pub use core_api::tool::{Tool, ToolCategory, ToolDescriptionLength, truncate_label};
use crate::core::tools::tool_names as tn;

pub const MAX_LABEL_SHORT: usize = 60;
pub const MAX_LABEL_FULL: usize = 120;

/// Registry of all available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
    }

    /// Tool definitions for the root agent (depth = 0): excludes sub_agents_only tools.
    pub fn openai_definitions(&self) -> Vec<Value> {
        self.tools.values()
            .filter(|t| !t.sub_agents_only())
            .map(|t| t.openai_definition())
            .collect()
    }

    /// Tool definitions that are marked sub_agents_only. Used in dispatch_call_agent
    /// to augment the child config's base_tool_defs.
    pub fn openai_definitions_sub_agents_only(&self) -> Vec<Value> {
        self.tools.values()
            .filter(|t| t.sub_agents_only())
            .map(|t| t.openai_definition())
            .collect()
    }

    /// Returns the names of all tools marked `root_agent_only`.
    pub fn root_agent_only_names(&self) -> Vec<String> {
        self.tools.values()
            .filter(|t| t.root_agent_only())
            .map(|t| t.name().to_string())
            .collect()
    }

    /// Returns `(name, description)` for every registered tool.
    pub fn list_all(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self.tools.values()
            .map(|t| (t.name().to_string(), t.description().to_string()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Human-readable label for any tool call, including non-registry tools (call_agent, MCP, …).
    pub fn describe_call(&self, name: &str, args: &Value, length: ToolDescriptionLength) -> String {
        if let Some(tool) = self.tools.get(name) {
            return tool.describe(args, length);
        }
        // Non-registry tools handled inline.
        match name {
            tn::CALL_AGENT => {
                let agent = args["agent_id"].as_str().unwrap_or("?");
                match length {
                    ToolDescriptionLength::Short => {
                        truncate_label(&format!("call_agent `{agent}`"), MAX_LABEL_SHORT)
                    }
                    ToolDescriptionLength::Full => {
                        let prompt = args["prompt"].as_str().unwrap_or("");
                        let first = prompt.lines().next().unwrap_or(prompt);
                        truncate_label(&format!("call_agent `{agent}`: {first}"), MAX_LABEL_FULL)
                    }
                }
            }
            _ => name.to_string(),
        }
    }

    /// Returns the category of a registered tool, or `None` for unknown tools
    /// (MCP tools, interface tools, call_agent, etc.).
    pub fn category_of(&self, name: &str) -> Option<ToolCategory> {
        self.tools.get(name).map(|t| t.category())
    }

    /// Dispatch a tool call by name.
    pub fn dispatch(&self, name: &str, args: Value) -> Result<String> {
        match self.tools.get(name) {
            Some(tool) => tool.execute(args),
            None       => anyhow::bail!("Unknown tool: {name}"),
        }
    }
}
