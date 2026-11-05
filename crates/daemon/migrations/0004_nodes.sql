ALTER TABLE jobs ADD COLUMN requested_node TEXT;
ALTER TABLE tasks ADD COLUMN node_id TEXT;

-- Nodes that ever registered (so offline agents still show up in listings).
CREATE TABLE IF NOT EXISTS nodes (
    id          TEXT PRIMARY KEY,
    url         TEXT,
    hostname    TEXT NOT NULL,
    info        TEXT NOT NULL,   -- JSON NodeRegistration
    first_seen  TEXT NOT NULL,
    last_seen   TEXT NOT NULL
);
