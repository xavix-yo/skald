//! Approval gate — centralised human-in-the-loop for tool execution.
//!
//! ## Architecture
//!
//! `ApprovalManager` is a top-level service (in `AppState`) shared by all
//! sessions. Its job is threefold:
//!
//! 1. **Rules** — decide, given (agent_id, session source, tool_name, args),
//!    whether a tool call needs human approval, is always allowed, or always
//!    denied. Rules live in `approval_rules` (SQLite) and are evaluated in
//!    priority order; the first match wins. If no rule matches the default is
//!    `Allow`.
//!
//! 2. **Pending registry** — when a tool is gated, an in-memory entry tracks
//!    the waiting session. The web "Pending Approvals" page reads this list
//!    to show the human what needs deciding.
//!
//! 3. **Resolution** — `resolve(request_id, decision)` is called by the WS
//!    handler (or the Telegram approval bot in the future) to unblock the
//!    waiting session.
//!
//! ## Rule evaluation order
//!
//! Rules are sorted by `priority` ASC (lower = evaluated first). Within the
//! same priority, more-specific rules should be given a lower priority number.
//! The first rule whose `agent_id`, `source`, and `tool_pattern` all match
//! determines the action; if none matches the tool is allowed through.
//!
//! ## Hardcoded exception
//!
//! File-write tools targeting `memory/` paths bypass the rule engine and are
//! always allowed (this mirrors the original behaviour and can be replaced by
//! an explicit `allow` rule later).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, Mutex, oneshot};
use tracing::{debug, info, warn};

use crate::core::session::handler::ApprovalDecision;
use crate::core::tools::tool_names as tn;
use crate::core::tools::ToolCategory;
use crate::core::events::{GlobalEvent, ServerEvent};

// ── Public types ──────────────────────────────────────────────────────────────

/// What the guardian concludes for a given tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateResult {
    /// No rule fired (or an explicit `allow` rule matched). Execute freely.
    Allow,
    /// An explicit `deny` rule matched. Reject immediately without asking.
    Deny,
    /// A `require` rule matched. Ask the human before executing.
    Require,
}

/// The action stored in an `approval_rules` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    /// Always require human approval.
    Require,
    /// Always allow (whitelist — skip the gate even if another rule would require it).
    Allow,
    /// Always deny (blacklist — tool call is rejected immediately).
    Deny,
}

impl RuleAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuleAction::Require => "require",
            RuleAction::Allow   => "allow",
            RuleAction::Deny    => "deny",
        }
    }
}

impl std::str::FromStr for RuleAction {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "require" => Ok(RuleAction::Require),
            "allow"   => Ok(RuleAction::Allow),
            "deny"    => Ok(RuleAction::Deny),
            other     => anyhow::bail!("unknown RuleAction: {other}"),
        }
    }
}

/// One row from `approval_rules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRule {
    pub id:           i64,
    /// `None` matches any agent.
    pub agent_id:     Option<String>,
    /// `None` matches any source (`web`, `telegram`, `cron`, …).
    pub source:       Option<String>,
    /// Exact tool name or glob suffix: `mcp__gmail__*` matches all Gmail tools.
    pub tool_pattern: String,
    /// Optional glob on the normalised file path (e.g. `data/*`).
    /// Only meaningful for tools that carry a `path` argument.
    /// `None` = no path filter (matches any path, or tools without a path arg).
    /// `Some("data/*")` = only fires when `args["path"]` starts with `data/`.
    pub path_pattern: Option<String>,
    pub action:       RuleAction,
    pub note:         Option<String>,
    /// Lower priority number = evaluated first.
    pub priority:     i64,
    /// Permission group this rule belongs to. `None` is treated as `"default"`.
    pub group_id:     Option<String>,
}

/// Input for creating a new rule.
#[derive(Debug, Clone, Deserialize)]
pub struct NewApprovalRule {
    pub agent_id:     Option<String>,
    pub source:       Option<String>,
    pub tool_pattern: String,
    /// Optional glob on the normalised file path (e.g. `data/*`).
    pub path_pattern: Option<String>,
    pub action:       RuleAction,
    pub note:         Option<String>,
    pub priority:     Option<i64>,
    /// Permission group for this rule. Defaults to `"default"` when omitted.
    pub group_id:     Option<String>,
}

/// Public view of a pending approval request (no channel, safe to clone/serialize).
#[derive(Debug, Clone, Serialize)]
pub struct PendingApprovalInfo {
    pub request_id:    i64,
    pub session_id:    i64,
    pub tool_call_id:  i64,
    pub tool_name:     String,
    pub arguments:     Value,
    pub agent_id:      String,
    pub source:        String,
    /// Human-readable label for the origin (e.g. "CronJob: Daily Digest"). Null for web sessions.
    pub context_label: Option<String>,
    /// ISO-8601 timestamp string (UTC).
    pub created_at:    String,
    /// Registered tool category (None for MCP and unknown tools).
    pub tool_category: Option<ToolCategory>,
    /// MCP server name extracted from the tool name (e.g. "gmail" from "mcp__gmail__search").
    /// None for non-MCP tools.
    pub mcp_server:    Option<String>,
}

// ── Session bypass ────────────────────────────────────────────────────────────

/// What a session bypass entry applies to.
pub enum BypassScope {
    /// Covers every tool regardless of category.
    All,
    /// Covers only tools of the given registered category.
    Category(ToolCategory),
    /// Covers only tools belonging to the named MCP server
    /// (matched by the `mcp__<server>__` prefix in the tool name).
    McpServer(String),
}

/// A single in-memory bypass entry for a session.
///
/// Converts `GateResult::Require` → `Allow` for the matching scope.
/// `Deny` rules are never bypassed.
struct ApprovalBypass {
    scope:      BypassScope,
    /// `None` = no expiry (lasts until session ends / `clear_session_bypass` is called).
    expires_at: Option<Instant>,
}

// ── Internal entry ────────────────────────────────────────────────────────────

struct PendingEntry {
    info: PendingApprovalInfo,
    tx:   oneshot::Sender<ApprovalDecision>,
}

// ── ApprovalManager ───────────────────────────────────────────────────────────

pub struct ApprovalManager {
    db:               Arc<SqlitePool>,
    pending:          Mutex<HashMap<i64, PendingEntry>>,
    next_id:          AtomicI64,
    session_bypasses: Mutex<HashMap<i64, Vec<ApprovalBypass>>>,
    event_tx:         broadcast::Sender<GlobalEvent>,
}

impl ApprovalManager {
    pub fn new(db: Arc<SqlitePool>, event_tx: broadcast::Sender<GlobalEvent>) -> Self {
        Self {
            db,
            pending:          Mutex::new(HashMap::new()),
            next_id:          AtomicI64::new(1),
            session_bypasses: Mutex::new(HashMap::new()),
            event_tx,
        }
    }

    // ── Seeding ───────────────────────────────────────────────────────────────

    /// Inserts the default rules (equivalent to the old hardcoded `needs_approval`)
    /// only if the table is currently empty. Safe to call at every startup.
    pub async fn seed_defaults(&self) -> Result<()> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM approval_rules")
            .fetch_one(self.db.as_ref())
            .await?;

        if count > 0 {
            return Ok(());
        }

        let defaults: &[(&str, &str)] = &[
            (tn::EXECUTE_CMD, "require"),
            (tn::RESTART,     "require"),
            ("write_file",     "require"),
            ("edit_file",      "require"),
            ("insert_at_line", "require"),
            ("replace_lines",  "require"),
            // Opening a mobile pairing window emits a secret (the QR) into chat:
            // it must be a deliberate human action, not LLM-triggerable (plugin.md §11).
            ("mobile_start_pairing", "require"),
        ];

        for (pattern, action) in defaults {
            sqlx::query(
                "INSERT INTO approval_rules (tool_pattern, action, note, priority, group_id)
                 VALUES (?, ?, 'default rule', 10, 'default')",
            )
            .bind(pattern)
            .bind(action)
            .execute(self.db.as_ref())
            .await?;
        }

        // Catch-all allow at the bottom of the default group so unmatched tools stay
        // permitted for standard sessions; groups without this rule are restrictive.
        sqlx::query(
            "INSERT INTO approval_rules (tool_pattern, action, note, priority, group_id)
             VALUES ('*', 'allow', 'Allow all tools by default (catch-all)', 9999, 'default')",
        )
        .execute(self.db.as_ref())
        .await?;

        info!("approval_rules seeded with {} default rules + catch-all allow", defaults.len());
        Ok(())
    }

    /// Idempotent migration: ensures the Default group has an allow-all catch-all rule
    /// at priority 9999. Needed when upgrading from the old default-open policy.
    pub async fn seed_allow_all_default(&self) -> Result<()> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM approval_rules
             WHERE tool_pattern = '*' AND action = 'allow' AND group_id = 'default'",
        )
        .fetch_one(self.db.as_ref())
        .await?;

        if count > 0 {
            return Ok(());
        }

        sqlx::query(
            "INSERT INTO approval_rules (tool_pattern, action, note, priority, group_id)
             VALUES ('*', 'allow', 'Allow all tools by default (catch-all)', 9999, 'default')",
        )
        .execute(self.db.as_ref())
        .await?;

        info!("approval_rules: migrated — seeded allow-all catch-all for default group (priority 9999)");
        Ok(())
    }

    /// One-time migration: inserts `allow` rules for `data/*` at priority 5
    /// (before the default `require` rules at priority 10) if they don't exist yet.
    ///
    /// Call this at startup; once the rules are in the DB, remove this call.
    pub async fn seed_data_path_rules(&self) -> Result<()> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM approval_rules WHERE path_pattern = 'data/*' AND action = 'allow'",
        )
        .fetch_one(self.db.as_ref())
        .await?;

        if count > 0 {
            return Ok(());
        }

        let file_write_tools = &["write_file", "edit_file", "insert_at_line", "replace_lines"];
        for tool in file_write_tools {
            sqlx::query(
                "INSERT INTO approval_rules (tool_pattern, path_pattern, action, note, priority, group_id)
                 VALUES (?, 'data/*', 'allow', 'auto-allow data/ writes', 5, 'default')",
            )
            .bind(tool)
            .execute(self.db.as_ref())
            .await?;
        }

        info!("approval_rules: seeded {} allow rules for data/*", file_write_tools.len());
        Ok(())
    }

    /// Idempotent: seeds `deny` rules so the read tools cannot read the `secrets/`
    /// directory. Reading a secret would leak its value into the LLM context, chat
    /// history, the compactor's summaries and the WS stream — a read is effectively
    /// worse than a write here, hence `deny` (not `require`). `Deny` is non-bypassable,
    /// so this holds even under an active session bypass.
    ///
    /// Two patterns per tool: `secrets` (a tool rooted *at* the dir, e.g. recursive
    /// `grep_files`/`list_files`) and `secrets/*` (a file inside it). This complements
    /// the hardcoded `secrets` entry in the read tools' `SKIP_DIRS`, which prevents
    /// recursive descent into `secrets/` when rooted higher up.
    ///
    /// Matches the cwd-relative `secrets/` (Skald's own). Skipped once present.
    pub async fn seed_secrets_deny_rules(&self) -> Result<()> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM approval_rules
             WHERE path_pattern IN ('secrets', 'secrets/*') AND action = 'deny'",
        )
        .fetch_one(self.db.as_ref())
        .await?;

        if count > 0 {
            return Ok(());
        }

        let read_tools = &["read_file", "grep_files", "list_files", "search_file", "get_ast_outline"];
        for tool in read_tools {
            for pattern in ["secrets", "secrets/*"] {
                sqlx::query(
                    "INSERT INTO approval_rules (tool_pattern, path_pattern, action, note, priority, group_id)
                     VALUES (?, ?, 'deny', 'deny reading secrets/', 5, 'default')",
                )
                .bind(tool)
                .bind(pattern)
                .execute(self.db.as_ref())
                .await?;
            }
        }

        info!("approval_rules: seeded secrets/ deny rules for {} read tools", read_tools.len());
        Ok(())
    }

    // ── Guardian ──────────────────────────────────────────────────────────────

    /// Main gate check: returns what the guardian has decided for this tool call.
    ///
    /// Evaluation order:
    /// 1. Hardcoded memory-path exception → `Allow`.
    /// 2. Rules for `group_id` first, then "default" group as fallback, sorted by priority ASC.
    ///    First match wins.
    /// 3. Session bypass: if a matching bypass is active, `Require` → `Allow`.
    ///    `Deny` is never bypassed.
    /// 4. No match → `Require` (default-closed policy).
    ///    The Default group has a seeded `allow * priority=9999` catch-all that keeps the
    ///    permissive behaviour for standard sessions; groups without that rule are restrictive.
    pub async fn check(
        &self,
        session_id: i64,
        category:   Option<ToolCategory>,
        agent_id:   &str,
        source:     &str,
        tool_name:  &str,
        args:       &Value,
        group_id:   Option<&str>,
    ) -> GateResult {
        // Hardcoded exception: memory/ file writes are always auto-approved.
        if is_memory_path(tool_name, args) {
            debug!(tool = tool_name, "approval: memory-path exception → allow");
            return GateResult::Allow;
        }

        let rules = match crate::core::db::approval_rules::list_for_group(&self.db, group_id).await {
            Ok(r)  => r,
            Err(e) => {
                warn!("approval: failed to load rules: {e} — defaulting to Allow");
                return GateResult::Allow;
            }
        };

        let result = 'rules: {
            for rule in &rules {
                if !rule_matches(rule, agent_id, source, tool_name, args) {
                    continue;
                }
                debug!(
                    tool = tool_name, agent = agent_id, source,
                    rule_id = rule.id, action = ?rule.action,
                    rule_group = rule.group_id.as_deref().unwrap_or("default"),
                    "approval: rule matched"
                );
                break 'rules match rule.action {
                    RuleAction::Require => GateResult::Require,
                    RuleAction::Allow   => GateResult::Allow,
                    RuleAction::Deny    => GateResult::Deny,
                };
            }
            debug!(tool = tool_name, "approval: no rule matched → require");
            GateResult::Require
        };

        // Session bypass: converts Require → Allow when an active bypass matches.
        // Deny is intentionally not bypassable.
        if matches!(result, GateResult::Require) {
            let mut bypasses = self.session_bypasses.lock().await;
            if let Some(entries) = bypasses.get_mut(&session_id) {
                // Prune expired entries lazily.
                entries.retain(|b| b.expires_at.map_or(true, |t| Instant::now() < t));
                let active = entries.iter().any(|b| bypass_matches(b, category, tool_name));
                if active {
                    debug!(
                        tool = tool_name, session_id,
                        "approval: session bypass active → allow"
                    );
                    return GateResult::Allow;
                }
            }
        }

        result
    }

    // ── Pending registry ──────────────────────────────────────────────────────

    /// Registers a pending approval and returns `(request_id, rx)`.
    /// The caller should send `ApprovalRequired` / `PendingWrite` events to
    /// the frontend and then `await rx` to block until the human responds.
    pub async fn register(
        &self,
        session_id:    i64,
        tool_call_id:  i64,
        tool_name:     &str,
        arguments:     Value,
        agent_id:      &str,
        source:        &str,
        context_label: Option<&str>,
        category:      Option<ToolCategory>,
    ) -> (i64, oneshot::Receiver<ApprovalDecision>) {
        let request_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx)   = oneshot::channel();

        let entry = PendingEntry {
            info: PendingApprovalInfo {
                request_id,
                session_id,
                tool_call_id,
                tool_name:     tool_name.to_string(),
                arguments,
                agent_id:      agent_id.to_string(),
                source:        source.to_string(),
                context_label: context_label.map(str::to_string),
                created_at:    chrono::Utc::now().to_rfc3339(),
                tool_category: category,
                mcp_server:    mcp_server_from_tool_name(tool_name),
            },
            tx,
        };

        let info_source = source.to_string();
        self.pending.lock().await.insert(request_id, entry);
        info!(
            session_id, tool = tool_name, agent = agent_id, source, request_id,
            "approval: pending registered"
        );
        // Broadcast on the global bus so Inbox subscribers (e.g. the
        // mobile-connector plugin) can re-snapshot. This is the bus counterpart
        // of the per-session `ApprovalRequired` WS event.
        let _ = self.event_tx.send(GlobalEvent {
            source:     Some(info_source),
            session_id: Some(session_id),
            event:      ServerEvent::ApprovalRequested {
                request_id,
                tool_call_id,
                tool_name: tool_name.to_string(),
            },
        });
        (request_id, rx)
    }

    /// Resolves a pending approval, unblocks the waiting session, and broadcasts
    /// `ApprovalResolved` to all WebSocket subscribers.
    pub async fn resolve(&self, request_id: i64, decision: ApprovalDecision) -> Option<PendingApprovalInfo> {
        if let Some(entry) = self.pending.lock().await.remove(&request_id) {
            let approved = matches!(decision, ApprovalDecision::Approved);
            let verb = if approved { "approved" } else { "rejected" };
            info!(
                request_id, tool = entry.info.tool_name,
                session_id = entry.info.session_id, verb,
                "approval: resolved"
            );
            let _ = entry.tx.send(decision);
            let _ = self.event_tx.send(GlobalEvent {
                source:     Some(entry.info.source.clone()),
                session_id: Some(entry.info.session_id),
                event:      ServerEvent::ApprovalResolved {
                    request_id,
                    tool_call_id: entry.info.tool_call_id,
                    approved,
                },
            });
            Some(entry.info)
        } else {
            warn!(request_id, "approval: resolve called for unknown/already-resolved request_id");
            None
        }
    }

    /// Convenience wrapper: approve a pending request.
    pub async fn approve(&self, request_id: i64) -> Option<PendingApprovalInfo> {
        self.resolve(request_id, ApprovalDecision::Approved).await
    }

    /// Convenience wrapper: reject a pending request with an optional note.
    pub async fn reject(&self, request_id: i64, note: String) -> Option<PendingApprovalInfo> {
        self.resolve(request_id, ApprovalDecision::Rejected { note }).await
    }

    /// Resolves a pending approval by `tool_call_id` (used by the REST endpoint,
    /// which knows the DB tool id but not the in-memory `request_id`).
    /// Returns `true` if an active pending entry was found and resolved,
    /// `false` if no matching entry exists (e.g. the app was restarted).
    pub async fn resolve_for_tool_call(
        &self,
        tool_call_id: i64,
        decision:     ApprovalDecision,
    ) -> bool {
        let mut map = self.pending.lock().await;
        let request_id = map.values()
            .find(|e| e.info.tool_call_id == tool_call_id)
            .map(|e| e.info.request_id);

        if let Some(request_id) = request_id {
            if let Some(entry) = map.remove(&request_id) {
                let verb = match &decision {
                    ApprovalDecision::Approved        => "approved",
                    ApprovalDecision::Rejected { .. } => "rejected",
                };
                info!(
                    request_id, tool_call_id, tool = entry.info.tool_name,
                    session_id = entry.info.session_id, verb,
                    "approval: resolved by tool_call_id"
                );
                // Mirror `resolve()`: broadcast on the global bus so Inbox
                // subscribers (e.g. the mobile-connector plugin) re-snapshot.
                // Without this, approving the inline copilot card leaves mobile
                // clients showing a stale pending item.
                let approved = matches!(decision, ApprovalDecision::Approved);
                let _ = self.event_tx.send(GlobalEvent {
                    source:     Some(entry.info.source.clone()),
                    session_id: Some(entry.info.session_id),
                    event:      ServerEvent::ApprovalResolved {
                        request_id,
                        tool_call_id: entry.info.tool_call_id,
                        approved,
                    },
                });
                let _ = entry.tx.send(decision);
                return true;
            }
        }
        false
    }

    /// Drops all pending entries for a session (called when WS disconnects).
    /// The waiting `await rx` in `llm_loop` will get `Err(RecvError)` and
    /// return `TurnOutcome::Cancelled`.
    pub async fn cancel_for_session(&self, session_id: i64) {
        let mut map = self.pending.lock().await;
        let before  = map.len();
        map.retain(|_, e| e.info.session_id != session_id);
        let dropped = before - map.len();
        if dropped > 0 {
            info!(session_id, dropped, "approval: cancelled pending entries (WS disconnected)");
        }
        self.session_bypasses.lock().await.remove(&session_id);
    }

    // ── Session bypass ────────────────────────────────────────────────────────

    /// Returns a snapshot of a single pending approval by `request_id`, without resolving it.
    pub async fn get_pending(&self, request_id: i64) -> Option<PendingApprovalInfo> {
        self.pending.lock().await.get(&request_id).map(|e| e.info.clone())
    }

    /// Bypasses all approval prompts for `session_id` for the rest of the session.
    pub async fn bypass_session(&self, session_id: i64) {
        self.session_bypasses.lock().await
            .entry(session_id)
            .or_default()
            .push(ApprovalBypass { scope: BypassScope::All, expires_at: None });
        info!(session_id, "approval: bypass active (session)");
    }

    /// Bypasses all approval prompts for `session_id` for `duration`.
    pub async fn bypass_session_for(&self, session_id: i64, duration: Duration) {
        self.session_bypasses.lock().await
            .entry(session_id)
            .or_default()
            .push(ApprovalBypass { scope: BypassScope::All, expires_at: Some(Instant::now() + duration) });
        info!(session_id, secs = duration.as_secs(), "approval: bypass active (timed)");
    }

    /// Bypasses approval prompts for a specific tool `category`.
    /// `duration` is `None` for an indefinite (session-scoped) bypass.
    pub async fn bypass_session_for_category(
        &self,
        session_id: i64,
        category:   ToolCategory,
        duration:   Option<Duration>,
    ) {
        let expires_at = duration.map(|d| Instant::now() + d);
        self.session_bypasses.lock().await
            .entry(session_id)
            .or_default()
            .push(ApprovalBypass { scope: BypassScope::Category(category), expires_at });
        info!(session_id, ?category, secs = duration.map(|d| d.as_secs()), "approval: bypass active (category)");
    }

    /// Bypasses approval prompts for all tools belonging to `mcp_server`.
    /// `duration` is `None` for an indefinite (session-scoped) bypass.
    pub async fn bypass_session_for_mcp(
        &self,
        session_id: i64,
        mcp_server: String,
        duration:   Option<Duration>,
    ) {
        let expires_at = duration.map(|d| Instant::now() + d);
        self.session_bypasses.lock().await
            .entry(session_id)
            .or_default()
            .push(ApprovalBypass { scope: BypassScope::McpServer(mcp_server.clone()), expires_at });
        info!(session_id, mcp_server, secs = duration.map(|d| d.as_secs()), "approval: bypass active (mcp_server)");
    }

    /// Removes all bypass entries for a session.
    pub async fn clear_session_bypass(&self, session_id: i64) {
        self.session_bypasses.lock().await.remove(&session_id);
        info!(session_id, "approval: bypass cleared");
    }

    /// Returns a snapshot of all currently-pending approvals (for the web page).
    pub async fn list_pending(&self) -> Vec<PendingApprovalInfo> {
        self.pending.lock().await.values()
            .map(|e| e.info.clone())
            .collect()
    }

    // ── Tool visibility ───────────────────────────────────────────────────────

    /// Returns `false` only when the first matching rule (by tool_pattern) is `Deny`.
    /// Path/agent/source filters are intentionally ignored — this is a static
    /// "is the tool offered to the LLM at all?" check, not an execution-time gate.
    /// Rules must already be loaded via `list_for_group`.
    pub fn is_tool_visible(&self, rules: &[ApprovalRule], tool_name: &str) -> bool {
        for rule in rules {
            if pattern_matches(&rule.tool_pattern, tool_name) {
                return rule.action != RuleAction::Deny;
            }
        }
        true // no matching rule → visible (backward compatible)
    }

    /// Resolves the effective `RuleAction` for `tool_name` in `group_id`,
    /// evaluating only `tool_pattern` (no path/agent/source).
    /// Returns `None` when no rule matches (tool is implicitly visible).
    pub async fn check_tool_visibility(
        &self,
        group_id:  &str,
        tool_name: &str,
    ) -> Option<RuleAction> {
        let rules = crate::core::db::approval_rules::list_for_group(&self.db, Some(group_id))
            .await
            .unwrap_or_default();
        for rule in &rules {
            if pattern_matches(&rule.tool_pattern, tool_name) {
                return Some(rule.action.clone());
            }
        }
        None
    }

    // ── Rule management ───────────────────────────────────────────────────────

    pub async fn list_rules(&self) -> Result<Vec<ApprovalRule>> {
        crate::core::db::approval_rules::list(&self.db).await
    }

    pub async fn add_rule(&self, r: NewApprovalRule) -> Result<i64> {
        crate::core::db::approval_rules::insert(&self.db, r).await
    }

    pub async fn delete_rule(&self, id: i64) -> Result<()> {
        crate::core::db::approval_rules::delete(&self.db, id).await
    }

    pub async fn update_rule(&self, id: i64, r: NewApprovalRule) -> Result<()> {
        crate::core::db::approval_rules::update(&self.db, id, r).await
    }

    /// Approve + register a session bypass so future tool calls of the same
    /// category / MCP server are auto-approved.
    ///
    /// - `bypass_secs = Some(n)`: bypass lasts `n` seconds (0 is treated as indefinite)
    /// - `bypass_secs = None`: bypass lasts until the session ends
    ///
    /// Scope is auto-detected from the pending request's tool metadata,
    /// mirroring the web-inbox logic in `src/frontend/api/inbox.rs`.
    pub async fn approve_with_bypass(&self, request_id: i64, bypass_secs: Option<u64>) {
        let info = self.get_pending(request_id).await;
        self.approve(request_id).await;
        let Some(info) = info else { return };
        let duration = bypass_secs
            .filter(|&s| s > 0)
            .map(Duration::from_secs);
        if let Some(cat) = info.tool_category {
            self.bypass_session_for_category(info.session_id, cat, duration).await;
        } else if let Some(srv) = info.mcp_server {
            self.bypass_session_for_mcp(info.session_id, srv, duration).await;
        } else {
            match duration {
                Some(d) => self.bypass_session_for(info.session_id, d).await,
                None    => self.bypass_session(info.session_id).await,
            }
        }
    }
}

// ── ApprovalApi trait impl ────────────────────────────────────────────────────

#[async_trait::async_trait]
impl core_api::approval::ApprovalApi for ApprovalManager {
    async fn approve(&self, request_id: i64) {
        self.approve(request_id).await;
    }

    async fn reject(&self, request_id: i64, note: String) {
        self.reject(request_id, note).await;
    }

    async fn approve_with_bypass(&self, request_id: i64, bypass_secs: Option<u64>) {
        self.approve_with_bypass(request_id, bypass_secs).await;
    }
}

// ── Matching helpers ──────────────────────────────────────────────────────────

/// Returns `true` if the rule applies to the given (agent_id, source, tool_name, args).
fn rule_matches(rule: &ApprovalRule, agent_id: &str, source: &str, tool_name: &str, args: &Value) -> bool {
    // agent_id filter
    if let Some(ref ra) = rule.agent_id {
        if ra != agent_id {
            return false;
        }
    }
    // source filter
    if let Some(ref rs) = rule.source {
        if rs != source {
            return false;
        }
    }
    // tool pattern (exact or glob suffix)
    if !pattern_matches(&rule.tool_pattern, tool_name) {
        return false;
    }
    // path_pattern filter: if set, args["path"] must match
    if let Some(ref pp) = rule.path_pattern {
        let path = args["path"].as_str().unwrap_or("");
        let norm = normalize_path(path);
        if !pattern_matches(pp, &norm) {
            return false;
        }
    }
    true
}

/// Matches an exact name or a `prefix*` glob.
pub(crate) fn pattern_matches(pattern: &str, tool_name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        tool_name.starts_with(prefix)
    } else {
        pattern == tool_name
    }
}

/// Returns `true` when an `ApprovalBypass` entry covers the given tool.
fn bypass_matches(bypass: &ApprovalBypass, category: Option<ToolCategory>, tool_name: &str) -> bool {
    match &bypass.scope {
        BypassScope::All              => true,
        BypassScope::Category(bc)     => category.map_or(false, |tc| tc == *bc),
        BypassScope::McpServer(server) => {
            mcp_server_from_tool_name(tool_name).map_or(false, |s| s == *server)
        }
    }
}

/// Extracts the MCP server name from a tool name following the `mcp__<server>__<tool>` pattern.
fn mcp_server_from_tool_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("mcp__")?;
    let end  = rest.find("__")?;
    Some(rest[..end].to_string())
}

/// Normalises a file path to a project-relative form for rule matching.
///
/// The path is canonicalized first (resolving `..` and symlinks via
/// `canonicalize_for_policy`) so that `docs/../secrets/x` or a symlink into `secrets/`
/// cannot evade a path-pattern rule. If the canonical path falls under the process
/// working directory it is made relative to it; otherwise the leading `/` is stripped
/// as a best-effort fallback.
pub(crate) fn normalize_path(path: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let canon = crate::core::tools::fs::canonicalize_for_policy(path, &cwd);
    if let Ok(rel) = canon.strip_prefix(&cwd) {
        return rel.to_string_lossy().into_owned();
    }
    canon.to_string_lossy().trim_start_matches('/').to_string()
}

/// Returns `true` when a file-write tool is targeting the `memory/` directory.
/// These are always auto-approved (the LLM manages memory autonomously).
fn is_memory_path(tool_name: &str, args: &Value) -> bool {
    if !crate::core::tools::is_file_write_tool(tool_name) {
        return false;
    }
    let path = args["path"].as_str().unwrap_or("");
    let norm = normalize_path(path);
    norm.starts_with("memory/")
}
