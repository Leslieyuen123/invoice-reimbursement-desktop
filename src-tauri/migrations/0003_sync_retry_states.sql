CREATE TABLE sync_retry_states (
    account_id TEXT PRIMARY KEY NOT NULL,
    failures INTEGER NOT NULL CHECK (failures >= 0),
    next_retry_at TEXT,
    suspended INTEGER NOT NULL CHECK (suspended IN (0, 1)),
    updated_at TEXT NOT NULL,
    FOREIGN KEY (account_id) REFERENCES mailbox_accounts(id) ON DELETE CASCADE,
    CHECK (
        (suspended = 1 AND next_retry_at IS NULL) OR
        (suspended = 0 AND next_retry_at IS NOT NULL)
    )
);
