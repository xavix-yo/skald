use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::{error, info};

use crate::core::chat_hub::ChatHub;
use crate::core::db::scheduled_jobs::{self, ScheduledJob};
use crate::core::session::manager::ChatSessionManager;

pub struct TaskManager {
    pool:    Arc<SqlitePool>,
    tz:      Option<Tz>,
    session: std::sync::OnceLock<Arc<ChatSessionManager>>,
    hub:     std::sync::OnceLock<Arc<ChatHub>>,
}

/// Returns `(next_utc, is_single)` where `is_single` is `true` when the
/// schedule has no second fire time after the first — i.e. the expression
/// can only ever fire once.  Falls back to system local time when `tz` is `None`.
fn next_fire_and_single(schedule: &Schedule, tz: Option<Tz>) -> Option<(DateTime<Utc>, bool)> {
    if let Some(tz) = tz {
        let mut it = schedule.upcoming(tz);
        let first = it.next()?.with_timezone(&Utc);
        Some((first, it.next().is_none()))
    } else {
        let mut it = schedule.upcoming(Local);
        let first = it.next()?.with_timezone(&Utc);
        Some((first, it.next().is_none()))
    }
}

fn next_fire(schedule: &Schedule, tz: Option<Tz>) -> Option<DateTime<Utc>> {
    next_fire_and_single(schedule, tz).map(|(dt, _)| dt)
}

impl TaskManager {
    pub fn new(pool: Arc<SqlitePool>, tz: Option<Tz>) -> Arc<Self> {
        Arc::new(Self {
            pool,
            tz,
            session: std::sync::OnceLock::new(),
            hub:     std::sync::OnceLock::new(),
        })
    }

    /// Called once after ChatSessionManager is built, breaking the circular dep.
    pub fn set_session(&self, session: Arc<ChatSessionManager>) {
        let _ = self.session.set(session);
    }

    /// Called once after ChatHub is built. Used for completion notifications.
    pub fn set_hub(&self, hub: Arc<ChatHub>) {
        let _ = self.hub.set(hub);
    }

    fn session(&self) -> Result<&Arc<ChatSessionManager>> {
        self.session.get().ok_or_else(|| anyhow::anyhow!("cron: session manager not initialized"))
    }

    /// Start the background loops. Must be called after set_session().
    /// Returns join handles so the caller can await them during shutdown.
    pub fn start(self: Arc<Self>, shutdown: tokio_util::sync::CancellationToken) -> Vec<tokio::task::JoinHandle<()>> {
        // Main scheduler loop.
        let me = Arc::clone(&self);
        let sd1 = shutdown.clone();
        let h1 = tokio::spawn(async move {
            if let Err(e) = me.recover_interrupted().await {
                error!("cron: startup recovery failed: {e}");
            }
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = sd1.cancelled() => { info!("cron: scheduler loop stopping"); break; }
                    _ = interval.tick() => {
                        if let Err(e) = me.tick().await {
                            error!("cron tick error: {e}");
                        }
                    }
                }
            }
        });

        // Cleanup loop: removes single_run jobs completed more than 7 days ago.
        let pool = Arc::clone(&self.pool);
        let sd2 = shutdown.clone();
        let h2 = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tokio::select! {
                    _ = sd2.cancelled() => { info!("cron: cleanup loop stopping"); break; }
                    _ = interval.tick() => {
                        if let Err(e) = cleanup_expired_single_runs(&pool).await {
                            error!("cron: cleanup error: {e}");
                        }
                    }
                }
            }
        });

        vec![h1, h2]
    }

    async fn recover_interrupted(&self) -> Result<()> {
        let session = self.session()?;
        let jobs = scheduled_jobs::list_interrupted(&self.pool).await?;
        if jobs.is_empty() { return Ok(()); }
        info!("cron: recovering {} interrupted job(s)", jobs.len());
        for job in jobs {
            let pool    = Arc::clone(&self.pool);
            let session = Arc::clone(session);
            let hub     = self.hub.get().cloned();
            let tz      = self.tz;
            tokio::spawn(async move {
                if let Err(e) = run_job(&pool, &session, hub.as_ref(), &job, tz).await {
                    error!("cron: recovery of job {} ('{}') failed: {e}", job.id, job.title);
                }
            });
        }
        Ok(())
    }

    async fn tick(&self) -> Result<()> {
        let session = self.session()?;
        let now = Utc::now().to_rfc3339();
        let jobs = scheduled_jobs::list_due(&self.pool, &now).await?;
        for job in jobs {
            let pool    = Arc::clone(&self.pool);
            let session = Arc::clone(session);
            let hub     = self.hub.get().cloned();
            let job     = job.clone();
            let tz      = self.tz;
            tokio::spawn(async move {
                if let Err(e) = run_job(&pool, &session, hub.as_ref(), &job, tz).await {
                    error!("cron job {} ('{}') failed: {e}", job.id, job.title);
                }
            });
        }
        Ok(())
    }

    // ── Sync wrappers (called from LLM tools via block_in_place) ─────────────

    pub fn list_jobs(&self) -> Result<Vec<ScheduledJob>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(scheduled_jobs::list(&self.pool))
        })
    }

    pub fn add_job(
        &self,
        title:       &str,
        description: &str,
        cron:        &str,
        prompt:      &str,
        agent_id:    &str,
        single_run:  bool,
        kind:        &str,
    ) -> Result<ScheduledJob> {
        let (first_fire, _is_single, single_run) = if kind == "immediate" {
            (None, true, true)
        } else {
            let schedule = Schedule::from_str(cron).map_err(|_| {
                anyhow::anyhow!(
                    "Invalid cron expression: '{cron}'. Use 7-field format: \
                     sec min hour dom month dow year  (e.g. '0 0 9 * * * *' = every day at 9:00)"
                )
            })?;
            let (first, single) = next_fire_and_single(&schedule, self.tz)
                .ok_or_else(|| anyhow::anyhow!("Cron expression '{cron}' has no upcoming fire times"))?;
            let single_run = single_run || single;
            (Some(first.to_rfc3339()), single, single_run)
        };
        let next_run_at: Option<&str> = first_fire.as_deref();
        let job = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(scheduled_jobs::create(
                &self.pool, title, description, cron, prompt, agent_id,
                single_run, next_run_at, kind,
            ))
        })?;

        if kind == "immediate" {
            let pool    = Arc::clone(&self.pool);
            let session = self.session()?.clone();
            let hub     = self.hub.get().cloned();
            let tz      = self.tz;
            let job_run = job.clone();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    if let Err(e) = run_job(&pool, &session, hub.as_ref(), &job_run, tz).await {
                        tracing::error!("immediate task {} ('{}') failed: {e}", job_run.id, job_run.title);
                    }
                });
            });
        }

        Ok(job)
    }

    pub fn delete_job(&self, id: i64) -> Result<bool> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(scheduled_jobs::delete(&self.pool, id))
        })
    }

    pub fn toggle_job(&self, id: i64, enabled: bool) -> Result<bool> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let found = scheduled_jobs::set_enabled(&self.pool, id, enabled).await?;
                if found && enabled {
                    // Recalculate next_run_at when re-enabling so a stale timestamp
                    // doesn't cause an immediate spurious fire.
                    let jobs = scheduled_jobs::list(&self.pool).await?;
                    if let Some(job) = jobs.iter().find(|j| j.id == id) {
                        let tz = self.tz;
                        if let Some(next) = Schedule::from_str(&job.cron)
                            .ok()
                            .and_then(|s| next_fire(&s, tz))
                            .map(|t| t.to_rfc3339())
                        {
                            scheduled_jobs::set_next_run_at(&self.pool, id, &next).await?;
                        }
                    }
                }
                Ok(found)
            })
        })
    }
}

// ── Job execution ─────────────────────────────────────────────────────────────

async fn run_job(
    pool:    &SqlitePool,
    session: &ChatSessionManager,
    hub:     Option<&Arc<ChatHub>>,
    job:     &ScheduledJob,
    tz:      Option<Tz>,
) -> Result<()> {
    info!("running cron job {} ('{}')", job.id, job.title);

    let started_at = Utc::now();

    // Each run gets a fresh ephemeral session; running_session_id is solely
    // for detecting interruptions at restart.
    let (session_id, _) = session.create_session(&job.agent_id, "cron", false, true).await?;
    scheduled_jobs::set_running(pool, job.id, session_id).await?;

    let handler = session.get_or_create_handler(session_id).await?;
    handler.set_context_label(format!("CronJob: {}", job.title));

    // Inject job identity as dynamic system context so the prompt stays clean.
    let job_context = format!(
        "[Job context]\nJob ID: {} — {}\nTime: {} UTC",
        job.id, job.title,
        started_at.format("%Y-%m-%d %H:%M"),
    );

    let (tx, _rx) = mpsc::channel(64);
    let run_result = handler.handle_message(
        &job.prompt,
        None,                  // client_name
        None,                  // extra_system_context
        Some(job_context),     // extra_system_dynamic_override
        None,                  // tail_reminder
        vec![],                // interface_tools
        tx,
        false,                 // is_synthetic
    ).await;

    let completed_at = Utc::now();
    let duration_ms  = (completed_at - started_at).num_milliseconds();

    let final_response = last_assistant_message(pool, session_id).await.ok().flatten();

    let next_run_at: Option<String> = if job.single_run {
        None
    } else {
        Schedule::from_str(&job.cron).ok()
            .and_then(|s| next_fire(&s, tz))
            .map(|t| t.to_rfc3339())
    };

    match &run_result {
        Ok(_) => {
            crate::core::db::job_runs::insert(
                pool, job.id, Some(session_id),
                &started_at.to_rfc3339(), &completed_at.to_rfc3339(), duration_ms,
                "completed", final_response.as_deref(), None,
            ).await?;
            scheduled_jobs::finish_run(pool, job.id, next_run_at.as_deref()).await?;
            if let Some(hub) = hub {
                let outcome = final_response.as_deref().unwrap_or("(no output)");
                let msg = format!(
                    "CronJob ID {} ({}) has completed with the following outcome: {}",
                    job.id, job.title, outcome,
                );
                let _ = hub.notify(msg).await;
            }
            info!("cron job {} done", job.id);
        }
        Err(e) => {
            let err_str = e.to_string();
            crate::core::db::job_runs::insert(
                pool, job.id, Some(session_id),
                &started_at.to_rfc3339(), &completed_at.to_rfc3339(), duration_ms,
                "failed", None, Some(&err_str),
            ).await?;
            scheduled_jobs::finish_run(pool, job.id, next_run_at.as_deref()).await?;
            if let Some(hub) = hub {
                let msg = format!(
                    "CronJob ID {} ({}) has failed with the following error: {} (check the logs)",
                    job.id, job.title, err_str,
                );
                let _ = hub.notify(msg).await;
            }
        }
    }

    run_result
}

/// Returns the most recent successful assistant message in the given session.
async fn last_assistant_message(pool: &SqlitePool, session_id: i64) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT ch.content
         FROM   chat_history ch
         JOIN   chat_sessions_stack css ON ch.session_stack_id = css.id
         WHERE  css.session_id = ? AND ch.role = 'assistant' AND ch.status = 'ok'
         ORDER  BY ch.id DESC
         LIMIT  1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(c,)| c))
}

async fn cleanup_expired_single_runs(pool: &SqlitePool) -> Result<()> {
    let n = sqlx::query(
        "DELETE FROM scheduled_jobs
         WHERE single_run = 1
           AND enabled    = 0
           AND last_run_at < datetime('now', '-7 days')",
    )
    .execute(pool)
    .await?
    .rows_affected();
    if n > 0 {
        info!("cron: removed {n} expired single-run job(s)");
    }
    Ok(())
}
