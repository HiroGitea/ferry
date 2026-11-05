CREATE TABLE IF NOT EXISTS tasks (
    id                INTEGER PRIMARY KEY,
    user_id           INTEGER NOT NULL REFERENCES users(id),
    name              TEXT NOT NULL,
    base_image        TEXT NOT NULL,
    cpus              REAL NOT NULL,
    mem_bytes         INTEGER NOT NULL,
    gpus              INTEGER NOT NULL,
    keep_warm_secs    INTEGER NOT NULL DEFAULT 0,
    state             TEXT NOT NULL,
    head_snapshot_id  INTEGER,
    workspace_path    TEXT,
    archive_path      TEXT,
    warm_container    TEXT,
    warm_until        TEXT,
    warm_gpus         TEXT NOT NULL DEFAULT '[]',
    warm_spec         TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL,
    closed_at         TEXT
);
CREATE INDEX IF NOT EXISTS tasks_user ON tasks(user_id);

CREATE TABLE IF NOT EXISTS snapshots (
    id          INTEGER PRIMARY KEY,
    task_id     INTEGER NOT NULL REFERENCES tasks(id),
    parent_id   INTEGER,
    job_id      INTEGER,
    image       TEXT NOT NULL,
    size_bytes  INTEGER,
    note        TEXT,
    pinned      INTEGER NOT NULL DEFAULT 0,
    deleted     INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS snapshots_task ON snapshots(task_id);

ALTER TABLE jobs ADD COLUMN task_id INTEGER;
ALTER TABLE jobs ADD COLUMN base_snapshot_id INTEGER;
ALTER TABLE jobs ADD COLUMN snapshot_id INTEGER;
CREATE INDEX IF NOT EXISTS jobs_task ON jobs(task_id);
