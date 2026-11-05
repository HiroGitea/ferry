-- Per-user notification channels.
CREATE TABLE IF NOT EXISTS notify_settings (
    user_id          INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    webhook_url      TEXT,
    webhook_format   TEXT NOT NULL DEFAULT 'json',
    telegram_chat_id TEXT,
    on_success       INTEGER NOT NULL DEFAULT 1,
    on_failure       INTEGER NOT NULL DEFAULT 1,
    task_steps       INTEGER NOT NULL DEFAULT 0,
    updated_at       TEXT NOT NULL
);
