-- One-time (or N-time) registration links. Only the SHA-256 of the code is stored.
CREATE TABLE IF NOT EXISTS invites (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    hash        TEXT NOT NULL UNIQUE,
    role        TEXT NOT NULL DEFAULT 'user',
    -- If set, the invite can only register this exact user name.
    user_name   TEXT,
    note        TEXT,
    max_uses    INTEGER NOT NULL DEFAULT 1,
    uses        INTEGER NOT NULL DEFAULT 0,
    expires_at  TEXT NOT NULL,
    created_by  INTEGER REFERENCES users(id) ON DELETE SET NULL,
    created_at  TEXT NOT NULL
);

-- Which invite a user registered with (NULL for users created by an admin).
ALTER TABLE users ADD COLUMN invite_id INTEGER REFERENCES invites(id) ON DELETE SET NULL;
