CREATE TABLE IF NOT EXISTS usage_events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    ts            TEXT    NOT NULL,
    tenant        TEXT    NOT NULL,
    workspace     TEXT,
    user_id       TEXT,
    route         TEXT    NOT NULL,
    provider      TEXT    NOT NULL,
    model         TEXT    NOT NULL,
    lane          TEXT    NOT NULL,
    input_tokens  INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cost_usd      REAL    NOT NULL,
    request_id    TEXT    NOT NULL,
    status        TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_tenant ON usage_events (tenant, workspace);
