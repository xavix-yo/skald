use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use serde_json::Value;

use crate::core::tools::tool_names as tn;
use super::{ChatSessionHandler, call_agent_tool_def, update_scratchpad_tool_def};
use super::interface_tools::{AgentRunConfig, InterfaceTool, ToolFuture};

/// Returns a `show_mcp_tools` OpenAI tool definition.
pub(super) fn show_mcp_tools_tool_def() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": tn::SHOW_MCP_TOOLS,
            "description": "Activate one or more MCP servers so their tools become available. \
                            Pass an array of MCP server names (e.g. [\"gmail\", \"gcal\"]). \
                            Once activated, the server's tools are available from the next \
                            tool-call round onward.",
            "parameters": {
                "type": "object",
                "properties": {
                    "mcp_names": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Names of the MCP servers to activate (e.g. [\"gmail\", \"tavily\"])."
                    }
                },
                "required": ["mcp_names"]
            }
        }
    })
}

impl ChatSessionHandler {
    /// Resolves the LLM client and assembles `AgentRunConfig` for a top-level turn
    /// (depth = 0). Extracted to avoid duplicating the same ~15 lines in both
    /// `handle_message` and `resume_turn`.
    pub(super) async fn build_agent_config(
        &self,
        client_name:          Option<String>,
        extra_system:         Option<String>,
        extra_system_dynamic: Option<String>,
        mut interface_tools:  Vec<InterfaceTool>,
        system_substitutions: HashMap<String, String>,
    ) -> anyhow::Result<AgentRunConfig> {
        let meta = crate::core::agents::load_meta(&self.agent_id).ok();
        let (key, _) = self.llm_manager.resolve(
            client_name.as_deref(),
            meta.as_ref().and_then(|m| m.scope.as_deref()),
            meta.as_ref().and_then(|m| m.strength),
        ).await?;

        let mut base_tool_defs = self.tools.openai_definitions();
        base_tool_defs.push(call_agent_tool_def());
        base_tool_defs.push(update_scratchpad_tool_def());
        // Background sessions (cron, tic) get ask_user_clarification so the worker
        // can pause for user input. Interactive sessions get it inline via AgentQuestion.
        if !self.is_interactive {
            base_tool_defs.push(super::ask_user_clarification_tool_def());
        }

        // Per-agent allow_tools whitelist (from agent meta.json).
        if let Some(allowed) = meta.as_ref().and_then(|m| m.allow_tools.as_ref()) {
            base_tool_defs.retain(|def| {
                let name = def["function"]["name"].as_str().unwrap_or("");
                allowed.iter().any(|a| a == name)
            });
        }

        // Approval-rules visibility filter: hide tools whose effective action for
        // this session's permission group is Deny. Rules are loaded once and applied
        // synchronously; the execution-time gate in ApprovalManager remains as a
        // second layer of enforcement.
        {
            let group_id   = self.tool_group_id().await;
            let gid        = group_id.as_deref().unwrap_or("default");
            let group_rules = crate::core::db::approval_rules::list_for_group(
                &self.db, Some(gid),
            ).await.unwrap_or_default();
            base_tool_defs.retain(|def| {
                let name = def["function"]["name"].as_str().unwrap_or("");
                self.approval.is_tool_visible(&group_rules, name)
            });
        }

        // ── MCP grant initialisation ────────────────────────────────────────────
        //
        // Load persisted session grants from DB, then inject `show_mcp_tools` so
        // the LLM can activate additional MCP servers on demand.
        let persisted = crate::core::db::session_mcp_grants::list_for_session(
            &self.db, self.session_id,
        ).await.unwrap_or_default();

        let active_mcp_grants: Arc<RwLock<HashSet<String>>> =
            Arc::new(RwLock::new(persisted.into_iter().collect()));

        {
            let pool_clone   = Arc::clone(&self.db);
            let session_id   = self.session_id;
            let mcp_clone    = Arc::clone(&self.mcp);
            let grants_clone = Arc::clone(&active_mcp_grants);

            let show_tool = crate::core::tools::show_mcp_tools::ShowMcpTools {
                pool:              pool_clone,
                session_id,
                stack_id:          None,
                mcp:               mcp_clone,
                active_mcp_grants: grants_clone,
            };

            let show_tool = Arc::new(show_tool);
            interface_tools.push(InterfaceTool {
                definition: show_mcp_tools_tool_def(),
                handler: Arc::new(move |args| -> ToolFuture {
                    use crate::core::tools::Tool as _;
                    let tool = Arc::clone(&show_tool);
                    Box::pin(async move {
                        tokio::task::spawn_blocking(move || tool.execute(args))
                            .await
                            .map_err(|e| anyhow::anyhow!("show_mcp_tools task panicked: {e}"))?
                    })
                }),
            });
        }
        // ── End MCP grant initialisation ────────────────────────────────────────

        let root_only_tool_names: Vec<String> = self.tools.root_agent_only_names();

        let memory_tools = self.memory_manager.tools().await;
        let image_tools  = Arc::clone(&self.image_generator_manager).tools().await;

        Ok(AgentRunConfig {
            agent_id:             self.agent_id.clone(),
            client_name:          key,
            depth:                0,
            base_tool_defs,
            extra_system,
            extra_system_dynamic,
            tail_reminder:        None,
            system_substitutions,
            interface_tools,
            memory_tools,
            image_tools,
            mcp:                  Arc::clone(&self.mcp),
            active_mcp_grants,
            root_only_tool_names,
        })
    }
}
