use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::core::cron::TaskManager;
use crate::core::tools::Tool;

// ── list_cron_jobs ────────────────────────────────────────────────────────────

pub struct ListCronJobs(pub Arc<TaskManager>);

impl Tool for ListCronJobs {
    fn name(&self) -> &str { "list_cron_jobs" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Introspection }

    fn description(&self) -> &str {
        "List all scheduled cron jobs and immediate tasks. Returns id, title, description, cron expression, \
         agent_id, enabled status, single_run flag, kind (cron or immediate), last_run_at, and next_run_at for each."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn execute(&self, _args: Value) -> Result<String> {
        let jobs = self.0.list_jobs()?;
        if jobs.is_empty() {
            return Ok("No cron jobs configured.".into());
        }
        let arr: Vec<Value> = jobs.iter().map(|j| json!({
            "id":          j.id,
            "title":       j.title,
            "description": j.description,
            "cron":        j.cron,
            "agent_id":    j.agent_id,
            "enabled":     j.enabled,
            "single_run":  j.single_run,
            "kind":        j.kind,
            "last_run_at": j.last_run_at,
            "next_run_at": j.next_run_at,
            "created_at":  j.created_at,
        })).collect();
        Ok(serde_json::to_string_pretty(&arr)?)
    }
}

// ── add_cron_job ──────────────────────────────────────────────────────────────

pub struct AddCronJob(pub Arc<TaskManager>);

impl Tool for AddCronJob {
    fn name(&self) -> &str { "add_cron_job" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "Create a new scheduled cron job. \
         The cron expression uses 7 fields: sec min hour dom month dow year \
         (e.g. '0 0 9 * * * *' = every day at 09:00, '0 */30 * * * * *' = every 30 min). \
         Times are interpreted in the server timezone (Europe/London). \
         single_run is optional: if the cron expression can only fire once (e.g. a specific \
         date and time), the job is automatically marked as one-shot — you do not need to \
         set single_run=true in that case. \
         agent_id defaults to 'worker' if omitted."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["title", "cron", "prompt"],
            "properties": {
                "title":       { "type": "string",  "description": "Short name for this job" },
                "description": { "type": "string",  "description": "What this job does" },
                "cron":        { "type": "string",  "description": "7-field cron expression (times in server timezone: Europe/London)" },
                "prompt":      { "type": "string",  "description": "Prompt sent to the agent at each run" },
                "agent_id":    { "type": "string",  "description": "Agent to run (default: worker)" },
                "single_run":  { "type": "boolean", "description": "Force one-shot even if the expression repeats (auto-detected for expressions that can only fire once)" },
            }
        })
    }

    fn execute(&self, args: Value) -> Result<String> {
        let title       = args["title"].as_str().unwrap_or("").trim().to_string();
        let description = args["description"].as_str().unwrap_or("").trim().to_string();
        let cron        = args["cron"].as_str().unwrap_or("").trim().to_string();
        let prompt      = args["prompt"].as_str().unwrap_or("").trim().to_string();
        let agent_id    = args["agent_id"].as_str().unwrap_or("worker").trim().to_string();
        let single_run  = args["single_run"].as_bool().unwrap_or(false);

        if title.is_empty()  { anyhow::bail!("title is required"); }
        if cron.is_empty()   { anyhow::bail!("cron is required"); }
        if prompt.is_empty() { anyhow::bail!("prompt is required"); }

        let job = self.0.add_job(&title, &description, &cron, &prompt, &agent_id, single_run, "cron")?;
        let kind = if job.single_run { "one-shot" } else { "recurring" };
        Ok(format!(
            "Created {} cron job {} — '{}' (next run: {})",
            kind, job.id, job.title,
            job.next_run_at.as_deref().unwrap_or("unknown"),
        ))
    }
}

// ── delete_cron_job ───────────────────────────────────────────────────────────

pub struct DeleteCronJob(pub Arc<TaskManager>);

impl Tool for DeleteCronJob {
    fn name(&self) -> &str { "delete_cron_job" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "Permanently delete a cron job by its numeric id."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": { "type": "integer", "description": "Job id from list_cron_jobs" }
            }
        })
    }

    fn execute(&self, args: Value) -> Result<String> {
        let id = args["id"].as_i64().ok_or_else(|| anyhow::anyhow!("id must be an integer"))?;
        if self.0.delete_job(id)? {
            Ok(format!("Cron job {id} deleted."))
        } else {
            Ok(format!("No cron job with id {id}."))
        }
    }
}

// ── toggle_cron_job ───────────────────────────────────────────────────────────

pub struct ToggleCronJob(pub Arc<TaskManager>);

impl Tool for ToggleCronJob {
    fn name(&self) -> &str { "toggle_cron_job" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "Enable or disable a cron job without deleting it. \
         Re-enabling recalculates next_run_at from the cron expression."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["id", "enabled"],
            "properties": {
                "id":      { "type": "integer", "description": "Job id from list_cron_jobs" },
                "enabled": { "type": "boolean", "description": "true to enable, false to disable" }
            }
        })
    }

    fn execute(&self, args: Value) -> Result<String> {
        let id      = args["id"].as_i64().ok_or_else(|| anyhow::anyhow!("id must be an integer"))?;
        let enabled = args["enabled"].as_bool().ok_or_else(|| anyhow::anyhow!("enabled must be boolean"))?;
        if self.0.toggle_job(id, enabled)? {
            Ok(format!("Cron job {id} {}.", if enabled { "enabled" } else { "disabled" }))
        } else {
            Ok(format!("No cron job with id {id}."))
        }
    }
}
