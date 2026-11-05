CREATE TABLE IF NOT EXISTS users (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    role        TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tokens (
    id            INTEGER PRIMARY KEY,
    user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    hash          TEXT NOT NULL UNIQUE,
    name          TEXT,
    created_at    TEXT NOT NULL,
    last_used_at  TEXT
);

CREATE TABLE IF NOT EXISTS quotas (
    user_id           INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    max_running_jobs  INTEGER,
    max_running_gpus  INTEGER,
    max_queued_jobs   INTEGER
);

CREATE TABLE IF NOT EXISTS jobs (
    id               INTEGER PRIMARY KEY,
    user_id          INTEGER NOT NULL REFERENCES users(id),
    node_id          TEXT,
    name             TEXT NOT NULL,
    image            TEXT NOT NULL,
    command          TEXT NOT NULL,   -- JSON array
    env              TEXT NOT NULL,   -- JSON object
    cpus             REAL NOT NULL,
    mem_bytes        INTEGER NOT NULL,
    gpus             INTEGER NOT NULL,
    state            TEXT NOT NULL,
    priority         INTEGER NOT NULL DEFAULT 0,
    depends_on       TEXT NOT NULL,   -- JSON array
    array_index      INTEGER,
    time_limit_secs  INTEGER,
    submitted_at     TEXT NOT NULL,
    started_at       TEXT,
    finished_at      TEXT,
    exit_code        INTEGER,
    gpu_ids          TEXT NOT NULL,   -- JSON array
    container_id     TEXT,
    workspace_path   TEXT,
    reason           TEXT,
    archived         INTEGER NOT NULL DEFAULT 0,
    workspace_deleted INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state);
CREATE INDEX IF NOT EXISTS jobs_user ON jobs(user_id);
CREATE INDEX IF NOT EXISTS jobs_finished ON jobs(finished_at);
