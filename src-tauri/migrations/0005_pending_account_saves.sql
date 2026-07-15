CREATE TABLE pending_account_saves (
    operation_id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL UNIQUE,
    phase TEXT NOT NULL CHECK (
        phase IN ('prepared', 'credential_staged', 'committed')
    ),
    is_update INTEGER NOT NULL CHECK (is_update IN (0, 1)),
    provider TEXT NOT NULL CHECK (provider IN ('gmail', 'qq')),
    email TEXT NOT NULL UNIQUE CHECK (length(trim(email)) > 0),
    imap_host TEXT NOT NULL CHECK (length(trim(imap_host)) > 0),
    imap_port INTEGER NOT NULL CHECK (imap_port BETWEEN 1 AND 65535),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    sync_interval_minutes INTEGER NOT NULL CHECK (
        sync_interval_minutes BETWEEN 5 AND 1440
    ),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_pending_account_saves_phase
    ON pending_account_saves (phase);
