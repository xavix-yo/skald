# Cron Jobs & Immediate Tasks

## TaskManager

`TaskManager` (formerly `CronTaskManager`) manages both scheduled cron jobs and immediate (one-shot) tasks. It uses `std::sync::OnceLock` to hold two late-injected dependencies, breaking circular chains that would arise if they were required at construction time:

| Dependency | Injected via | Needed for |
|---|---|---|
| `ChatSessionManager` | `set_session()` | Creating ephemeral sessions per job run |
| `ChatHub` | `set_hub()` | Sending completion/failure notifications |

In `main.rs`:
1. `TaskManager::new(pool)` — created first, OnceLocks empty
2. `ChatSessionManager::new(...)` — created second
3. `cron.set_session(Arc::clone(&manager))` — fills first OnceLock
4. `cron.start()` — background tasks begin (tick every 30 s)
5. `ChatHub::new()` — created after cron starts
6. `cron.set_hub(Arc::clone(&chat_hub))` — fills second OnceLock

The cron tick loop first fires 30 s after `start()`, so both OnceLocks are guaranteed to be filled before any job dispatch.

---

## Background Tasks

`start()` spawns two independent background tasks:

### Scheduler Loop

- Ticks every **30 seconds**
- Calls `db::scheduled_jobs::list_due(pool, &Utc::now().to_rfc3339())`
- Any job with `enabled=1`, `next_run_at <= now`, and `running_session_id IS NULL` is returned
- Each due job is spawned as an independent `tokio::task` via `run_job()`

### Cleanup Loop

- Waits 15 s at startup, then runs hourly
- Calls `cleanup_expired_single_runs(pool)`: DELETE single-run jobs that are disabled and older than 7 days

---

## 7-Field Cron Expression Format

**Format**: `sec min hour dom month dow year`

This is the format of the [`cron`](https://crates.io/crates/cron) crate — **not** standard Unix crontab (which uses 5 fields without seconds or year).

| Field | Values |
|---|---|
| sec | 0–59 |
| min | 0–59 |
| hour | 0–23 |
| dom | 1–31 or `*` |
| month | 1–12 or `*` |
| dow | 0–6 (Sun=0) or `*` |
| year | 4-digit year or `*` |

Examples:

| Expression | Meaning |
|---|---|
| `0 0 9 * * * *` | Every day at 09:00:00 |
| `0 */30 * * * * *` | Every 30 minutes |
| `0 0 8 * * 1 *` | Every Monday at 08:00 |

`add_cron_job` validates the expression with `Schedule::from_str()` before saving.

---

## Timezone

Cron expressions are evaluated in the timezone configured under `timezone` in `config.yml` (top-level IANA name, e.g. `Europe/Rome`). When omitted, the server's system local timezone is used as fallback. The same setting also controls the timestamp injected into the LLM context each turn.

The timezone is loaded at startup, logged at `INFO` level, and passed into `TaskManager`. All three points where `next_run_at` is computed (`add_job`, `toggle_job`, `run_job`) use the same `next_fire(schedule, tz)` helper which converts the result to UTC before storing.

---

## `next_run_at` (pre-computed fire time)

Rather than a sliding look-back window, the scheduler uses a **pre-computed `next_run_at` timestamp** stored in the DB:

- Set at job creation (first upcoming fire time after now, in the configured timezone)
- Advanced to the next fire time after each successful run
- Cleared when a job is disabled
- Recalculated from the cron expression when `toggle_cron_job` re-enables a job

This means: a tick simply does `WHERE next_run_at <= now` — no expression evaluation in the hot path. A missed tick is automatically covered because `next_run_at` stays in the past until the job actually runs.

---

## Job Lifecycle

1. LLM calls `add_cron_job(title, description, cron, prompt, agent_id, single_run?)` → inserted in DB with `enabled=1`, `next_run_at` set to first upcoming fire time
2. Scheduler tick → `list_due()` returns the job
3. `run_job()` spawned:
   a. New ephemeral session created (`is_ephemeral=1, is_interactive=0`) for `agent_id` (default: `"worker"`)
   b. `set_running(pool, job.id, session_id)` — marks job in-flight
   c. `handler.set_context_label("CronJob: <title>")` — used for Agent Inbox labels
   d. Job context injected via `extra_system_dynamic_override`
   e. `handler.handle_message(job.prompt, ...)` — agent runs
   f. Last assistant message read from DB → stored in `job_runs`
   g. `finish_run(pool, job.id, next_run_at)` — advances `next_run_at`; if `single_run=true` passes `None` to disable the job
   h. `hub.notify(...)` emits a completion/failure briefing to the home conversation
4. If run fails: error logged, job_runs row recorded with status `"failed"`, job still advanced/disabled

---

## `running_session_id` (restart recovery)

`scheduled_jobs.running_session_id` is non-null while a job is in-flight. On restart:

1. `recover_interrupted()` runs once, before the first tick
2. Queries `list_interrupted()` — all jobs where `running_session_id IS NOT NULL`
3. For each interrupted job, `run_job()` is spawned again (creates a fresh session — the old one is abandoned)

`list_due()` excludes rows with `running_session_id IS NOT NULL`, preventing double-runs.

---

## `kind` Column (cron vs immediate)

`scheduled_jobs` has a `kind` column with two values:

| `kind` | Behavior |
|--------|----------|
| `cron` | Scheduled job with a cron expression. Picked up by the tick loop when `next_run_at` is due. |
| `immediate` | Runs immediately on creation. No cron expression, no `next_run_at`. `single_run` is always true. Spawned in background via `add_job(kind="immediate")`. |

Immediate tasks are useful for fire-and-forget background work. The caller receives the task ID and can monitor completion via the `job_runs` audit trail and ChatHub notifications.

The `list_due()` query filters by `kind = 'cron'`, so immediate tasks are never picked up by the scheduler tick loop. Recovery (`list_interrupted()`) applies to both kinds.

---

## `single_run` (one-shot jobs)

If `single_run=true`, after the first execution `finish_run()` receives `next_run_at=None`, which sets `enabled=0` (disabling the job) rather than advancing the schedule. The job stays in the DB as a disabled record and is purged after 7 days by the cleanup loop.

**Auto-detection**: `add_job()` calls `next_fire_and_single()` which advances the cron iterator twice. If there is no second fire time — i.e. the expression can only ever match one point in time (e.g. `0 30 9 15 6 * 2026`) — `single_run` is forced to `true` regardless of what the caller passed. The LLM therefore does not need to set `single_run` explicitly for specific-datetime jobs.

---

## Session Handling

Each run always creates a **new ephemeral session**:

| Property | Value |
|---|---|
| `source` | `"cron"` |
| `is_interactive` | `0` |
| `is_ephemeral` | `1` |
| `agent_id` | job's `agent_id` (default `"worker"`) |

Sessions are not reused across runs. Each run gets a fresh context.

---

## Agent Interaction

Jobs run via the `worker` agent by default (see [agents.md](agents.md)). The worker agent:

- Executes the task described in the cron prompt
- Delegates complex work to sub-agents (engineer, researcher, architect)
- Calls `ask_user_clarification` when genuinely uncertain — this creates a pending entry in the `ClarificationManager` (visible in Agent Inbox) rather than blocking
- Its final assistant message is captured and sent as a completion notification via `ChatHub`

---

## Completion Notifications

After every run, `TaskManager` calls `hub.notify(briefing)`, which routes the message through `ChatHub`'s notification consumer to the home conversation. The briefing includes job title, status, and the agent's final response.

---

## `job_runs` (audit trail)

Every execution is recorded in `db::job_runs`. Schema: see [database.md](database.md).

---

## LLM Tools for Cron

| Tool | Action |
|---|---|
| `list_cron_jobs` | Returns JSON array of all tasks (id, title, cron, enabled, kind, next_run_at, single_run, last_run_at) |
| `add_cron_job` | Creates a new cron job; validates cron expression; computes next_run_at in the configured timezone; auto-detects single_run for one-fire expressions |
| `delete_cron_job` | Permanently deletes job by id |
| `toggle_cron_job` | Enables or disables a job; recalculates next_run_at when re-enabling |

The `add_cron_job` tool description tells the LLM that cron times are interpreted in the server timezone (`Europe/London` in the default config) and that `single_run` is auto-detected — the LLM should not need to set it for specific-datetime expressions.

---

## When to Update This File

- Scheduler tick interval changes
- `next_run_at` / `list_due` logic changes
- `run_job` session-handling logic changes
- New cron-related tools are added
- Recovery or cleanup loop logic changes
