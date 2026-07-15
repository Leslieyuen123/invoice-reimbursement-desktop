CREATE TABLE sync_retry_states_bounded (
    account_id TEXT PRIMARY KEY NOT NULL,
    failures INTEGER NOT NULL CHECK (failures BETWEEN 0 AND 2147483647),
    next_retry_at TEXT,
    suspended INTEGER NOT NULL CHECK (suspended IN (0, 1)),
    updated_at TEXT NOT NULL,
    FOREIGN KEY (account_id) REFERENCES mailbox_accounts(id) ON DELETE CASCADE,
    CHECK (
        (suspended = 1 AND next_retry_at IS NULL) OR
        (suspended = 0 AND next_retry_at IS NOT NULL)
    )
);

INSERT INTO sync_retry_states_bounded (
    account_id, failures, next_retry_at, suspended, updated_at
)
SELECT
    account_id,
    CASE WHEN failures > 2147483647 THEN 2147483647 ELSE failures END,
    next_retry_at,
    suspended,
    updated_at
FROM sync_retry_states;

DROP TABLE sync_retry_states;

ALTER TABLE sync_retry_states_bounded RENAME TO sync_retry_states;
