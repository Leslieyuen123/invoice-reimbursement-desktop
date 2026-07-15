CREATE TABLE pending_account_save_cleanups (
    operation_id TEXT PRIMARY KEY NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

INSERT INTO pending_account_save_cleanups (operation_id, created_at, updated_at)
SELECT operation_id, created_at, updated_at
FROM pending_account_saves
WHERE phase = 'committed';

DELETE FROM pending_account_saves
WHERE phase = 'committed';
