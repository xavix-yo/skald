use std::sync::Arc;

use anyhow::{Result, bail};
use sqlx::SqlitePool;
use tracing::info;

pub use crate::core::db::run_contexts::RunContextRow;
pub use crate::core::db::tool_permission_groups::ToolPermissionGroup;
use crate::core::approval::{ApprovalManager, RuleAction};

pub struct RunContextManager {
    db:       Arc<SqlitePool>,
    approval: Arc<ApprovalManager>,
}

impl RunContextManager {
    pub fn new(db: Arc<SqlitePool>, approval: Arc<ApprovalManager>) -> Self {
        Self { db, approval }
    }

    /// Seeds the built-in "default" group and run_context if they don't exist yet,
    /// then migrates any legacy rules with NULL group_id to the "default" group.
    /// Safe to call at every startup (idempotent).
    pub async fn seed_defaults(&self) -> Result<()> {
        crate::core::db::tool_permission_groups::insert_or_ignore(
            &self.db, "default", "Default", Some("Built-in default permission group"),
        ).await?;

        crate::core::db::run_contexts::insert_or_ignore(
            &self.db, "default", "Default", Some("Built-in default run context"), Some("default"),
        ).await?;

        let migrated = sqlx::query("UPDATE approval_rules SET group_id = 'default' WHERE group_id IS NULL")
            .execute(self.db.as_ref())
            .await
            .map(|r| r.rows_affected())
            .unwrap_or(0);

        if migrated > 0 {
            info!(%migrated, "run_context: migrated approval rules to 'default' group");
        }

        Ok(())
    }

    // ── RunContext CRUD ────────────────────────────────────────────────────────

    pub async fn list_contexts(&self) -> Result<Vec<RunContextRow>> {
        crate::core::db::run_contexts::list(&self.db).await
    }

    pub async fn get_context(&self, id: &str) -> Result<Option<RunContextRow>> {
        crate::core::db::run_contexts::get(&self.db, id).await
    }

    pub async fn create_context(
        &self,
        id:            &str,
        name:          &str,
        description:   Option<&str>,
        tool_group_id: Option<&str>,
    ) -> Result<()> {
        if id == "default" {
            bail!("cannot create a run_context with reserved id 'default'");
        }
        crate::core::db::run_contexts::insert(&self.db, id, name, description, tool_group_id).await
    }

    pub async fn update_context(
        &self,
        id:            &str,
        name:          &str,
        description:   Option<&str>,
        tool_group_id: Option<&str>,
    ) -> Result<bool> {
        crate::core::db::run_contexts::update(&self.db, id, name, description, tool_group_id).await
    }

    pub async fn delete_context(&self, id: &str) -> Result<bool> {
        if id == "default" {
            bail!("cannot delete the built-in 'default' run_context");
        }
        crate::core::db::run_contexts::delete(&self.db, id).await
    }

    // ── ToolPermissionGroup CRUD ───────────────────────────────────────────────

    pub async fn list_groups(&self) -> Result<Vec<ToolPermissionGroup>> {
        crate::core::db::tool_permission_groups::list(&self.db).await
    }

    pub async fn get_group(&self, id: &str) -> Result<Option<ToolPermissionGroup>> {
        crate::core::db::tool_permission_groups::get(&self.db, id).await
    }

    pub async fn create_group(
        &self,
        id:          &str,
        name:        &str,
        description: Option<&str>,
    ) -> Result<()> {
        if id == "default" {
            bail!("cannot create a permission group with reserved id 'default'");
        }
        crate::core::db::tool_permission_groups::insert(&self.db, id, name, description).await
    }

    pub async fn update_group(
        &self,
        id:          &str,
        name:        &str,
        description: Option<&str>,
    ) -> Result<bool> {
        crate::core::db::tool_permission_groups::update(&self.db, id, name, description).await
    }

    pub async fn delete_group(&self, id: &str) -> Result<bool> {
        if id == "default" {
            bail!("cannot delete the built-in 'default' permission group");
        }
        crate::core::db::tool_permission_groups::delete(&self.db, id).await
    }

    /// Duplicates a permission group and all its rules atomically.
    pub async fn duplicate_group(
        &self,
        source_id: &str,
        new_id:    &str,
        new_name:  &str,
    ) -> Result<()> {
        if new_id == "default" {
            bail!("cannot create a permission group with reserved id 'default'");
        }
        let source = crate::core::db::tool_permission_groups::get(&self.db, source_id).await?
            .ok_or_else(|| anyhow::anyhow!("source group '{source_id}' not found"))?;

        let mut tx = self.db.begin().await?;

        sqlx::query(
            "INSERT INTO tool_permission_groups (id, name, description) VALUES (?, ?, ?)",
        )
        .bind(new_id)
        .bind(new_name)
        .bind(source.description.as_deref())
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO approval_rules \
                (agent_id, source, tool_pattern, path_pattern, action, note, priority, group_id) \
             SELECT agent_id, source, tool_pattern, path_pattern, action, note, priority, ? \
             FROM   approval_rules \
             WHERE  group_id = ?",
        )
        .bind(new_id)
        .bind(source_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    // ── Tool visibility ────────────────────────────────────────────────────────

    /// Returns the effective `RuleAction` for `tool_name` under the permission group
    /// bound to `run_context_id`. Falls back to the `"default"` group when the run
    /// context has no explicit group or `run_context_id` is `None`.
    /// Returns `None` when no rule matches (tool is implicitly visible).
    pub async fn check_tool_visibility(
        &self,
        run_context_id: Option<&str>,
        tool_name:      &str,
    ) -> Option<RuleAction> {
        let group_id = if let Some(rc_id) = run_context_id {
            self.get_context(rc_id).await
                .ok()
                .flatten()
                .and_then(|rc| rc.tool_group_id)
                .unwrap_or_else(|| "default".to_string())
        } else {
            "default".to_string()
        };
        self.approval.check_tool_visibility(&group_id, tool_name).await
    }

    // ── Session assignment ─────────────────────────────────────────────────────

    pub async fn set_session_run_context(
        &self,
        session_id:     i64,
        run_context_id: Option<&str>,
    ) -> Result<()> {
        crate::core::db::run_contexts::set_run_context_for_session(
            &self.db, session_id, run_context_id,
        ).await
    }
}
