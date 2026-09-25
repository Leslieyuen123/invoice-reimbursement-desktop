-- The per-mail ledger only started recording in 0013, so every mail imported
-- before that has no row: the ledger page looked empty for exactly the history
-- a user wants to check when asking "did any invoice get missed?". Fill it in
-- once from the items we do have, one row per source mail.
--
-- The rows are marked as a backfill so they are never mistaken for a fresh
-- scan, and UIDVALIDITY uses the sentinel 1 because items never stored it; a
-- real scan that revisits the same UID overwrites the row (the recorder upserts
-- on the same primary key).
INSERT INTO mail_ledger (
    account_id, mailbox, uid_validity, uid, message_id, subject, sender,
    received_at, processed_at, candidate_count, imported_count, existing_count,
    failed_count, outcome, reason, marked_seen
)
SELECT
    source_account_id,
    COALESCE(source_mailbox, 'INBOX'),
    1,
    source_uid,
    source_message_id,
    NULL,
    NULL,
    fetched_at,
    fetched_at,
    COUNT(*),
    COUNT(*),
    0,
    0,
    'imported',
    '历史回填：该邮件在 0.2.5 之前导入，没有逐封处理记录',
    0
FROM items
WHERE source_type = 'email'
  AND source_account_id IS NOT NULL
  AND source_uid IS NOT NULL
GROUP BY source_account_id, COALESCE(source_mailbox, 'INBOX'), source_uid
ON CONFLICT DO NOTHING;
