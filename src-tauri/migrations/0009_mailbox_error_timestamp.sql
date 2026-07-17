ALTER TABLE mailbox_accounts ADD COLUMN last_error_at TEXT;

UPDATE mailbox_accounts
SET last_error_at = (
    SELECT sync_retry_states.updated_at
    FROM sync_retry_states
    WHERE sync_retry_states.account_id = mailbox_accounts.id
)
WHERE last_error IS NOT NULL
  AND EXISTS (
      SELECT 1
      FROM sync_retry_states
      WHERE sync_retry_states.account_id = mailbox_accounts.id
  );
