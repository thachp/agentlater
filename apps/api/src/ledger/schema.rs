//! The ledger's tables: what a new ledger is created with, and every change made since.

pub(super) const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS project (
        id TEXT PRIMARY KEY,
        created_ms INTEGER NOT NULL,
        expires_ms INTEGER NOT NULL,
        attempt_budget INTEGER NOT NULL,
        attempts_used INTEGER NOT NULL,
        signing_secret TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS tasks (
        seq INTEGER PRIMARY KEY,
        id TEXT NOT NULL UNIQUE,
        status TEXT NOT NULL,
        due_ms INTEGER NOT NULL,
        max_attempts INTEGER NOT NULL,
        run_attempts INTEGER NOT NULL,
        total_attempts INTEGER NOT NULL,
        generation INTEGER NOT NULL,
        fence INTEGER NOT NULL,
        lease_until_ms INTEGER,
        cancel_requested INTEGER NOT NULL,
        terminal_ms INTEGER,
        published_ms INTEGER,
        method TEXT NOT NULL,
        url TEXT NOT NULL,
        headers TEXT NOT NULL,
        body TEXT,
        run_at_ms INTEGER NOT NULL,
        idempotency_key TEXT UNIQUE,
        fingerprint TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS tasks_by_status_due ON tasks (status, due_ms)",
    "CREATE TABLE IF NOT EXISTS attempts (
        task_id TEXT NOT NULL,
        number INTEGER NOT NULL,
        started_ms INTEGER NOT NULL,
        finished_ms INTEGER,
        outcome TEXT,
        http_status INTEGER,
        PRIMARY KEY (task_id, number)
    )",
    "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL)",
];

/// Changes to ledgers created by an earlier [`SCHEMA`]. Each entry runs once per ledger, in
/// order; how many have run is kept in `meta`. Append only.
pub(super) const MIGRATIONS: &[&[&str]] = &[
    // 1: callbacks.
    &[
        "ALTER TABLE tasks ADD COLUMN callback_url TEXT",
        "ALTER TABLE tasks ADD COLUMN callback_task_id TEXT",
        "ALTER TABLE tasks ADD COLUMN callback_for TEXT",
    ],
    // 2: lanes.
    &[
        "ALTER TABLE tasks ADD COLUMN lane TEXT",
        // Covers LANE_CLEAR's lookup, so finding a lane's head stays a seek.
        "CREATE INDEX IF NOT EXISTS tasks_by_lane ON tasks (lane, status, due_ms, seq)",
    ],
    // 3: crons.
    &[
        "CREATE TABLE IF NOT EXISTS crons (
            name TEXT PRIMARY KEY,
            schedule TEXT NOT NULL,
            method TEXT NOT NULL,
            url TEXT NOT NULL,
            headers TEXT NOT NULL,
            body TEXT,
            max_attempts INTEGER NOT NULL,
            alert_url TEXT,
            next_run_ms INTEGER,
            health TEXT NOT NULL,
            health_reason TEXT,
            last_run_task_id TEXT,
            last_run_ms INTEGER,
            last_success_ms INTEGER,
            failing_since_ms INTEGER,
            health_run_at_ms INTEGER,
            created_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL
        )",
        "ALTER TABLE tasks ADD COLUMN cron TEXT",
        "ALTER TABLE tasks ADD COLUMN alert_for TEXT",
    ],
    // 4: watches.
    &[
        "ALTER TABLE tasks ADD COLUMN watch TEXT",
        "ALTER TABLE tasks ADD COLUMN watch_errors INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN watch_state TEXT",
    ],
    // 5: bought attempts. One row per payment, so a repeated webhook adds nothing.
    &["CREATE TABLE IF NOT EXISTS payments (
            id TEXT PRIMARY KEY,
            attempts INTEGER NOT NULL,
            created_ms INTEGER NOT NULL
        )"],
    // 6: response capture and the inbox. The project row holds the inbox's counters: the last
    // seq handed out (seqs are never reused, even after events are removed), the read cursor,
    // and how many unread events were removed since the cursor last moved.
    &[
        "ALTER TABLE tasks ADD COLUMN capture INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN response TEXT",
        "ALTER TABLE tasks ADD COLUMN inbox_seq INTEGER",
        "ALTER TABLE project ADD COLUMN inbox_last_seq INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE project ADD COLUMN inbox_acked_seq INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE project ADD COLUMN inbox_dropped INTEGER NOT NULL DEFAULT 0",
        // response_until_ms: when the captured response body inside the event must go,
        // in step with the task's own copy.
        "CREATE TABLE IF NOT EXISTS inbox (
            seq INTEGER PRIMARY KEY,
            at_ms INTEGER NOT NULL,
            event TEXT NOT NULL,
            response_until_ms INTEGER
        )",
    ],
    // 7: wake. At most one row: where to send a wake, and how it is going. covered_seq is the
    // newest inbox event the last wake told the agent about.
    &[
        "CREATE TABLE IF NOT EXISTS wake (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            url TEXT NOT NULL,
            headers TEXT NOT NULL,
            debounce_ms INTEGER NOT NULL,
            state TEXT NOT NULL,
            disabled_reason TEXT,
            consecutive_failures INTEGER NOT NULL,
            last_wake_ms INTEGER,
            last_wake_task_id TEXT,
            covered_seq INTEGER NOT NULL,
            created_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL
        )",
        "ALTER TABLE tasks ADD COLUMN wake_call INTEGER NOT NULL DEFAULT 0",
        // Numbers wake tasks. On the project, so deleting and setting the wake never reuses one.
        "ALTER TABLE project ADD COLUMN wakes_queued INTEGER NOT NULL DEFAULT 0",
    ],
    // 8: crons that report. Each run inherits the cron's callback_url and capture;
    // report_failures_only holds back a run's report unless it failed.
    &[
        "ALTER TABLE crons ADD COLUMN callback_url TEXT",
        "ALTER TABLE crons ADD COLUMN capture INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE crons ADD COLUMN report TEXT NOT NULL DEFAULT 'always'",
        "ALTER TABLE tasks ADD COLUMN report_failures_only INTEGER NOT NULL DEFAULT 0",
    ],
    // 9: secrets. `sealed` is ciphertext from the API's sealing key; the ledger never sees a
    // value. `failure` says why a task failed without an attempt (a secret it needed).
    &[
        "CREATE TABLE IF NOT EXISTS secrets (
            name TEXT PRIMARY KEY,
            sealed TEXT NOT NULL,
            value_bytes INTEGER NOT NULL,
            allowed_hosts TEXT NOT NULL,
            created_ms INTEGER NOT NULL,
            updated_ms INTEGER NOT NULL
        )",
        "ALTER TABLE tasks ADD COLUMN failure TEXT",
    ],
    // 10: watches that act. `on_match` is the request a watch sends when it matches;
    // `action_reserve` the rows and attempts admission set aside for it (the action, and its
    // callback); `triggered_by` names the watch on the task it sent.
    &[
        "ALTER TABLE tasks ADD COLUMN on_match TEXT",
        "ALTER TABLE tasks ADD COLUMN action_reserve INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE tasks ADD COLUMN triggered_by TEXT",
    ],
    // 11: inbound hooks. A hook is found by the SHA-256 of its token; `sealed_secret` is its
    // verify secret, sealed by the API. `hook_deliveries` holds senders' delivery ids for a day,
    // to drop retries; `hook_day`/`hook_day_count` cap the events a project gets per UTC day.
    &[
        "CREATE TABLE IF NOT EXISTS hooks (
            name TEXT PRIMARY KEY,
            token_hash TEXT NOT NULL UNIQUE,
            verify TEXT,
            sealed_secret TEXT,
            keep_headers TEXT NOT NULL,
            created_ms INTEGER NOT NULL,
            last_received_ms INTEGER,
            received INTEGER NOT NULL DEFAULT 0,
            refused TEXT NOT NULL DEFAULT '{}',
            rate_tat_ms INTEGER NOT NULL DEFAULT 0,
            day INTEGER NOT NULL DEFAULT 0,
            day_count INTEGER NOT NULL DEFAULT 0
        )",
        "CREATE TABLE IF NOT EXISTS hook_deliveries (
            hook TEXT NOT NULL,
            delivery_id TEXT NOT NULL,
            expires_ms INTEGER NOT NULL,
            PRIMARY KEY (hook, delivery_id)
        )",
        "ALTER TABLE project ADD COLUMN hook_day INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE project ADD COLUMN hook_day_count INTEGER NOT NULL DEFAULT 0",
    ],
    // 12: hook filters. `accept` holds the rules a request must match to be kept; `filtered`
    // counts the requests that matched none, answered and dropped.
    &[
        "ALTER TABLE hooks ADD COLUMN accept TEXT",
        "ALTER TABLE hooks ADD COLUMN filtered INTEGER NOT NULL DEFAULT 0",
    ],
    // 13: rotating the signing secret. The secret it replaced, and when that one stops signing;
    // nothing clears them afterwards, the time alone decides.
    &[
        "ALTER TABLE project ADD COLUMN signing_secret_previous TEXT",
        "ALTER TABLE project ADD COLUMN signing_secret_previous_until_ms INTEGER",
    ],
    // 14: A2A. `via` is 'a2a' on a task whose request came through A2A, the only tasks A2A
    // reads; null on every other task, and on all made before this.
    &["ALTER TABLE tasks ADD COLUMN via TEXT"],
];
