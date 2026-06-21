pub mod approval_rules;
pub mod project_tickets;
pub mod projects;
pub mod chat_history;
pub mod chat_llm_tools;
pub mod chat_sessions;
pub mod chat_sessions_stack;
pub mod chat_summaries;
pub mod config;
pub mod job_runs;
pub mod llm_requests;
pub mod mcp_events;
pub mod mcp_servers;
pub mod plugins;
pub mod scheduled_jobs;
pub mod scratchpad;
pub mod session_mcp_grants;
pub mod sources;
pub mod stack_mcp_grants;
pub mod tool_permission_groups;

use anyhow::Result;
use sqlx::{SqlitePool, sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous}};
use std::str::FromStr;
use std::time::Duration;

pub async fn init_pool(path: &str) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(path)?
        .create_if_missing(true)
        // WAL lets readers run alongside a single writer, and `busy_timeout`
        // makes a writer *wait* for the lock instead of failing immediately with
        // SQLITE_BUSY ("database is locked"). Without these, concurrent writers —
        // e.g. the mobile-connector persisting its E2E `send_counter` while the
        // chat loop / cron write history — abort mid-operation, which silently
        // drops outbound mobile messages (inbox_update never reaches the device).
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePool::connect_with(opts).await?;
    create_tables(&pool).await?;
    migrate_tables(&pool).await?;
    Ok(pool)
}

async fn create_tables(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chat_sessions (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            title            TEXT,
            source           TEXT    NOT NULL DEFAULT 'web',
            agent_id         TEXT    NOT NULL DEFAULT 'main',
            is_interactive   INTEGER NOT NULL DEFAULT 1,
            is_ephemeral     INTEGER NOT NULL DEFAULT 0,
            run_context_id   TEXT,
            created_at       TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chat_sessions_stack (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id          INTEGER NOT NULL REFERENCES chat_sessions(id),
            agent_id            TEXT    NOT NULL DEFAULT 'main',
            agent_prompt        TEXT,
            depth               INTEGER NOT NULL DEFAULT 0,
            parent_tool_call_id INTEGER,
            terminated_at       TEXT,
            created_at          TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chat_history (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            session_stack_id INTEGER NOT NULL REFERENCES chat_sessions_stack(id),
            role             TEXT    NOT NULL CHECK(role IN ('user', 'assistant', 'agent')),
            content          TEXT    NOT NULL DEFAULT '',
            status           TEXT    NOT NULL DEFAULT 'ok' CHECK(status IN ('ok', 'failed')),
            input_tokens     INTEGER,
            output_tokens    INTEGER,
            duration_ms      INTEGER,
            model_db_id      INTEGER REFERENCES llm_models(id),
            is_synthetic     INTEGER NOT NULL DEFAULT 0,
            reasoning_content TEXT,
            cost             REAL,
            created_at       TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chat_llm_tools (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            message_id INTEGER NOT NULL REFERENCES chat_history(id),
            name       TEXT    NOT NULL,
            arguments  TEXT,
            result     TEXT,
            status     TEXT    NOT NULL DEFAULT 'running' CHECK(status IN ('running', 'pending', 'done', 'failed')),
            created_at TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_stack_session   ON chat_sessions_stack(session_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_history_stack   ON chat_history(session_stack_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_tools_message   ON chat_llm_tools(message_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_servers (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            name          TEXT    NOT NULL UNIQUE,
            transport     TEXT    NOT NULL DEFAULT 'stdio',
            command       TEXT,
            args_json     TEXT,
            env_json      TEXT,
            url           TEXT,
            api_key       TEXT,
            description   TEXT,
            friendly_name TEXT,
            enabled       INTEGER NOT NULL DEFAULT 1,
            created_at    TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    let _ = sqlx::query("ALTER TABLE mcp_servers ADD COLUMN description TEXT").execute(pool).await;
    let _ = sqlx::query("ALTER TABLE mcp_servers ADD COLUMN friendly_name TEXT").execute(pool).await;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS llm_providers (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT    NOT NULL UNIQUE,
            type        TEXT    NOT NULL,
            api_key     TEXT,
            base_url    TEXT,
            description TEXT,
            removed_at  TEXT,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS llm_models (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id       INTEGER NOT NULL REFERENCES llm_providers(id) ON DELETE CASCADE,
            model_id          TEXT    NOT NULL,
            name              TEXT    NOT NULL UNIQUE,
            strength          TEXT,
            scope             TEXT    NOT NULL DEFAULT '[]',
            is_default        INTEGER NOT NULL DEFAULT 0,
            priority          INTEGER NOT NULL DEFAULT 100,
            extra_params      TEXT,
            removed_at        TEXT,
            context_length    INTEGER,
            max_output_tokens INTEGER,
            knowledge_cutoff  TEXT,
            capabilities      TEXT    NOT NULL DEFAULT '[]',
            created_at        TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(provider_id, model_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS scheduled_jobs (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            title              TEXT    NOT NULL,
            description        TEXT    NOT NULL DEFAULT '',
            cron               TEXT    NOT NULL,
            prompt             TEXT    NOT NULL,
            agent_id           TEXT    NOT NULL DEFAULT 'main',
            session_id         INTEGER REFERENCES chat_sessions(id),
            enabled            INTEGER NOT NULL DEFAULT 1,
            last_run_at        TEXT,
            next_run_at        TEXT,
            single_run         INTEGER NOT NULL DEFAULT 0,
            running_session_id INTEGER,
            kind               TEXT    NOT NULL DEFAULT 'cron',
            created_at         TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS plugins (
            id         TEXT    PRIMARY KEY,
            enabled    INTEGER NOT NULL DEFAULT 0,
            config     TEXT    NOT NULL DEFAULT '{}',
            created_at TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS session_scratchpad (
            session_id INTEGER NOT NULL REFERENCES chat_sessions(id),
            key        TEXT    NOT NULL,
            value      TEXT    NOT NULL,
            PRIMARY KEY (session_id, key)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tool_permission_groups (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            description TEXT,
            created_at  TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS approval_rules (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id     TEXT,
            source       TEXT,
            tool_pattern TEXT    NOT NULL,
            action       TEXT    NOT NULL DEFAULT 'require'
                             CHECK(action IN ('require', 'allow', 'deny')),
            note         TEXT,
            priority     INTEGER NOT NULL DEFAULT 100,
            path_pattern TEXT,
            group_id     TEXT    REFERENCES tool_permission_groups(id),
            created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS transcribe_models (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id INTEGER NOT NULL REFERENCES llm_providers(id),
            model_id    TEXT    NOT NULL,
            name        TEXT    NOT NULL UNIQUE,
            language    TEXT,
            priority    INTEGER NOT NULL DEFAULT 100,
            removed_at  TEXT,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(provider_id, model_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS image_generate_models (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id INTEGER NOT NULL REFERENCES llm_providers(id),
            model_id    TEXT    NOT NULL,
            name        TEXT    NOT NULL UNIQUE,
            priority    INTEGER NOT NULL DEFAULT 100,
            removed_at  TEXT,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(provider_id, model_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tts_models (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id  INTEGER NOT NULL REFERENCES llm_providers(id),
            model_id     TEXT    NOT NULL,
            name         TEXT    NOT NULL UNIQUE,
            description  TEXT,
            instructions TEXT,
            priority     INTEGER NOT NULL DEFAULT 100,
            removed_at   TEXT,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(provider_id, model_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sources (
            id                TEXT    PRIMARY KEY,
            active_session_id INTEGER REFERENCES chat_sessions(id),
            updated_at        TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS config (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS secrets (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_events (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            source       TEXT    NOT NULL,
            method       TEXT    NOT NULL,
            payload      TEXT    NOT NULL,
            processed    INTEGER NOT NULL DEFAULT 0,
            processed_at TEXT,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_mcp_events_pending
         ON mcp_events (processed, created_at)
         WHERE processed = 0",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS session_mcp_grants (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id INTEGER NOT NULL,
            mcp_name   TEXT    NOT NULL,
            granted_at TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(session_id, mcp_name)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS stack_mcp_grants (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            stack_id   INTEGER NOT NULL,
            mcp_name   TEXT    NOT NULL,
            granted_at TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(stack_id, mcp_name)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS llm_requests (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id       INTEGER,
            stack_id         INTEGER,
            model_name       TEXT    NOT NULL,
            request_json     TEXT    NOT NULL DEFAULT '',
            request_headers  TEXT,
            response_json    TEXT,
            response_headers TEXT,
            error_text       TEXT,
            input_tokens     INTEGER,
            output_tokens    INTEGER,
            duration_ms      INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_llm_requests_created
         ON llm_requests (created_at)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chat_summaries (
            id                      INTEGER PRIMARY KEY AUTOINCREMENT,
            stack_id                INTEGER NOT NULL REFERENCES chat_sessions_stack(id),
            content                 TEXT    NOT NULL,
            covers_up_to_message_id INTEGER NOT NULL,
            created_at              TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_chat_summaries_stack
         ON chat_summaries (stack_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS job_runs (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id         INTEGER NOT NULL REFERENCES scheduled_jobs(id),
            session_id     INTEGER,
            started_at     TEXT    NOT NULL,
            completed_at   TEXT,
            duration_ms    INTEGER,
            status         TEXT    NOT NULL
                               CHECK(status IN ('completed', 'failed', 'cancelled')),
            final_response TEXT,
            error          TEXT,
            created_at     TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_job_runs_job_id
         ON job_runs (job_id, created_at DESC)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS projects (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT    NOT NULL,
            path        TEXT    NOT NULL,
            description TEXT    NOT NULL DEFAULT '',
            run_context TEXT,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            updated_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS project_tickets (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id   INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            title        TEXT    NOT NULL,
            description  TEXT    NOT NULL DEFAULT '',
            status       TEXT    NOT NULL DEFAULT 'todo'
                             CHECK(status IN ('todo','pending','in_progress','done','failed')),
            agent_id     TEXT    NOT NULL DEFAULT 'main',
            run_context  TEXT,
            job_id       INTEGER REFERENCES scheduled_jobs(id),
            result       TEXT,
            error        TEXT,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
            started_at   TEXT,
            completed_at TEXT
        )",
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Incremental migrations tracked via the `config` table (key = `schema_version`).
/// Each migration runs exactly once. New migrations increment the version number.
async fn migrate_tables(pool: &SqlitePool) -> Result<()> {
    let version: u32 = sqlx::query_scalar::<_, String>(
        "SELECT value FROM config WHERE key='schema_version'",
    )
    .fetch_optional(pool)
    .await?
    .and_then(|v| v.parse().ok())
    .unwrap_or(0);

    if version < 1 {
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '1', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 2 {
        sqlx::query(
            "ALTER TABLE tts_models ADD COLUMN voice_id TEXT",
        )
        .execute(pool)
        .await
        .ok(); // ok() — column may already exist if re-running on a new DB
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '2', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 3 {
        sqlx::query("ALTER TABLE llm_requests ADD COLUMN cache_read_tokens     INTEGER")
            .execute(pool).await.ok();
        sqlx::query("ALTER TABLE llm_requests ADD COLUMN cache_creation_tokens INTEGER")
            .execute(pool).await.ok();
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '3', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 4 {
        sqlx::query(
            "ALTER TABLE scheduled_jobs ADD COLUMN kind TEXT NOT NULL DEFAULT 'cron'",
        )
        .execute(pool)
        .await
        .ok();
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '4', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 5 {
        sqlx::query("ALTER TABLE approval_rules ADD COLUMN group_id TEXT")
            .execute(pool).await.ok();
        sqlx::query("ALTER TABLE chat_sessions ADD COLUMN run_context_id TEXT")
            .execute(pool).await.ok();
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '5', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 6 {
        // Rename legacy kind 'immediate' → 'sync'
        sqlx::query("UPDATE scheduled_jobs SET kind = 'sync' WHERE kind = 'immediate'")
            .execute(pool).await.ok();
        // Add parent_session_id for async tasks (which session to inject the result into)
        sqlx::query(
            "ALTER TABLE scheduled_jobs ADD COLUMN parent_session_id INTEGER REFERENCES chat_sessions(id)",
        )
        .execute(pool).await.ok();
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '6', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 7 {
        sqlx::query(
            "ALTER TABLE scheduled_jobs ADD COLUMN run_context_id TEXT",
        )
        .execute(pool).await.ok();
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '7', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 8 {
        sqlx::query(
            "ALTER TABLE scheduled_jobs ADD COLUMN running_since TEXT",
        )
        .execute(pool).await.ok();
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '8', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 9 {
        // Flatten run_context_id: copy tool_group_id from run_contexts into the
        // referencing columns, then drop the now-obsolete run_contexts table.
        // run_context_id now stores a tool_permission_groups id directly.
        sqlx::query(
            "UPDATE chat_sessions
             SET run_context_id = (
                 SELECT tool_group_id FROM run_contexts
                 WHERE  id = chat_sessions.run_context_id
             )
             WHERE run_context_id IS NOT NULL",
        )
        .execute(pool)
        .await
        .ok();

        sqlx::query(
            "UPDATE scheduled_jobs
             SET run_context_id = (
                 SELECT tool_group_id FROM run_contexts
                 WHERE  id = scheduled_jobs.run_context_id
             )
             WHERE run_context_id IS NOT NULL",
        )
        .execute(pool)
        .await
        .ok();

        sqlx::query("DROP TABLE IF EXISTS run_contexts")
            .execute(pool)
            .await
            .ok();

        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '9', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 10 {
        // Rename run_context_id → run_context on both tables and convert
        // the column to a JSON blob (RunContext struct). Existing values are
        // cleared because the old plain group-id string is not valid JSON.
        sqlx::query("ALTER TABLE chat_sessions  RENAME COLUMN run_context_id TO run_context")
            .execute(pool).await.ok();
        sqlx::query("ALTER TABLE scheduled_jobs RENAME COLUMN run_context_id TO run_context")
            .execute(pool).await.ok();
        sqlx::query("UPDATE chat_sessions  SET run_context = NULL")
            .execute(pool).await.ok();
        sqlx::query("UPDATE scheduled_jobs SET run_context = NULL")
            .execute(pool).await.ok();

        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '10', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 11 {
        sqlx::query("ALTER TABLE scheduled_jobs ADD COLUMN origin_ref TEXT")
            .execute(pool).await.ok();
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '11', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 12 {
        sqlx::query("ALTER TABLE projects DROP COLUMN agent_id")
            .execute(pool).await.ok();
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '12', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 13 {
        // Agent ids were renamed to explicit forms and `worker` was deleted; reassign
        // existing job/stack rows so they don't orphan to a now-missing agent dir.
        // `worker` and `tinker` both fold into the new generic executor `generalist`.
        for (old, new) in [
            ("engineer",  "software-engineer"),
            ("architect", "software-architect"),
            ("explorer",  "code-explorer"),
            ("blueprint", "spec-writer"),
            ("tinker",    "generalist"),
            ("worker",    "generalist"),
        ] {
            sqlx::query("UPDATE scheduled_jobs      SET agent_id = ?1 WHERE agent_id = ?2")
                .bind(new).bind(old).execute(pool).await.ok();
            sqlx::query("UPDATE chat_sessions_stack SET agent_id = ?1 WHERE agent_id = ?2")
                .bind(new).bind(old).execute(pool).await.ok();
        }
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '13', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    if version < 14 {
        // Per-request cost in USD, reported by providers that bill per-call
        // (OpenRouter returns it under usage.cost). NULL for providers that don't.
        sqlx::query("ALTER TABLE chat_history ADD COLUMN cost REAL")
            .execute(pool).await.ok();
        sqlx::query(
            "INSERT OR REPLACE INTO config(key, value, updated_at)
             VALUES('schema_version', '14', datetime('now'))",
        )
        .execute(pool)
        .await?;
    }

    Ok(())
}
