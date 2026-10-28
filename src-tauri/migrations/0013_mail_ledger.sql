-- One row per scanned mail that carried invoice clues, so the user can see
-- which mails produced invoices and which ones still need attention. The
-- ledger is the source of truth; the mailbox \Seen flag is only a side effect.
CREATE TABLE mail_ledger (
    account_id TEXT NOT NULL,
    mailbox TEXT NOT NULL,
    uid_validity INTEGER NOT NULL CHECK (uid_validity > 0),
    uid INTEGER NOT NULL CHECK (uid > 0),
    message_id TEXT,
    subject TEXT,
    sender TEXT,
    received_at TEXT NOT NULL,
    processed_at TEXT NOT NULL,
    candidate_count INTEGER NOT NULL CHECK (candidate_count >= 0),
    imported_count INTEGER NOT NULL CHECK (imported_count >= 0),
    existing_count INTEGER NOT NULL CHECK (existing_count >= 0),
    failed_count INTEGER NOT NULL CHECK (failed_count >= 0),
    outcome TEXT NOT NULL CHECK (
        outcome IN ('imported', 'partial', 'failed', 'ignored')
    ),
    reason TEXT,
    marked_seen INTEGER NOT NULL DEFAULT 0 CHECK (marked_seen IN (0, 1)),
    PRIMARY KEY (account_id, mailbox, uid_validity, uid)
);

CREATE INDEX idx_mail_ledger_attention
    ON mail_ledger (outcome, received_at DESC);
CREATE INDEX idx_mail_ledger_account
    ON mail_ledger (account_id, received_at DESC);
