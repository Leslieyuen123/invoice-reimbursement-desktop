PRAGMA foreign_keys = ON;

CREATE TABLE batches (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    start_date TEXT NOT NULL CHECK (
        start_date GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
    ),
    end_date TEXT NOT NULL CHECK (
        end_date GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
    ),
    status TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'exported')),
    note TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_exported_at TEXT,
    CHECK (start_date <= end_date)
);

CREATE TABLE mailbox_accounts (
    id TEXT PRIMARY KEY NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('gmail', 'qq')),
    email TEXT NOT NULL UNIQUE CHECK (length(trim(email)) > 0),
    imap_host TEXT NOT NULL CHECK (length(trim(imap_host)) > 0),
    imap_port INTEGER NOT NULL CHECK (imap_port BETWEEN 1 AND 65535),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    sync_interval_minutes INTEGER NOT NULL CHECK (sync_interval_minutes BETWEEN 5 AND 1440),
    last_synced_at TEXT,
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE items (
    id TEXT PRIMARY KEY NOT NULL,
    original_name TEXT NOT NULL,
    original_path TEXT NOT NULL UNIQUE,
    normalized_pdf_path TEXT,
    sha256 TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    source_type TEXT NOT NULL CHECK (source_type IN ('email', 'manual_upload')),
    source_account_id TEXT,
    source_mailbox TEXT,
    source_uid INTEGER,
    source_message_id TEXT,
    source_part_id TEXT,
    fetched_at TEXT NOT NULL,
    invoice_date TEXT,
    suggested_period TEXT,
    batch_id TEXT,
    suggested_category TEXT CHECK (
        suggested_category IS NULL OR suggested_category IN (
            'transport', 'dining', 'accommodation', 'hospitality'
        )
    ),
    final_category TEXT CHECK (
        final_category IS NULL OR final_category IN (
            'transport', 'dining', 'accommodation', 'hospitality'
        )
    ),
    amount_cents INTEGER CHECK (amount_cents IS NULL OR amount_cents >= 0),
    currency TEXT NOT NULL DEFAULT 'CNY',
    city TEXT,
    company TEXT,
    recognition_status TEXT NOT NULL DEFAULT 'pending' CHECK (
        recognition_status IN ('pending', 'succeeded', 'failed')
    ),
    confirmation_status TEXT NOT NULL DEFAULT 'pending' CHECK (
        confirmation_status IN ('pending', 'confirmed')
    ),
    dedupe_status TEXT NOT NULL DEFAULT 'unique' CHECK (
        dedupe_status IN ('unique', 'suspected_duplicate', 'resolved')
    ),
    duplicate_of_id TEXT,
    note TEXT,
    event_tag TEXT,
    project_tag TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (batch_id) REFERENCES batches(id) ON DELETE SET NULL,
    FOREIGN KEY (duplicate_of_id) REFERENCES items(id) ON DELETE SET NULL,
    CHECK (
        source_type != 'email' OR (
            source_account_id IS NOT NULL AND length(trim(source_account_id)) > 0 AND
            source_mailbox IS NOT NULL AND length(trim(source_mailbox)) > 0 AND
            source_uid IS NOT NULL AND source_uid > 0 AND
            source_part_id IS NOT NULL AND length(trim(source_part_id)) > 0
        )
    )
);

CREATE INDEX idx_items_work_queue
    ON items (dedupe_status, recognition_status, confirmation_status);
CREATE INDEX idx_items_period ON items (suggested_period);
CREATE INDEX idx_items_batch ON items (batch_id);
CREATE INDEX idx_items_hash ON items (sha256);
CREATE UNIQUE INDEX idx_items_email_part
    ON items (source_account_id, source_mailbox, source_uid, source_part_id)
    WHERE source_type = 'email';

CREATE TABLE sync_cursors (
    account_id TEXT NOT NULL,
    mailbox TEXT NOT NULL,
    uid_validity INTEGER NOT NULL,
    last_uid INTEGER NOT NULL,
    PRIMARY KEY (account_id, mailbox),
    FOREIGN KEY (account_id) REFERENCES mailbox_accounts(id) ON DELETE CASCADE
);

CREATE TABLE sync_runs (
    id TEXT PRIMARY KEY NOT NULL,
    account_id TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed')),
    imported_count INTEGER NOT NULL DEFAULT 0 CHECK (imported_count >= 0),
    error_message TEXT,
    FOREIGN KEY (account_id) REFERENCES mailbox_accounts(id) ON DELETE CASCADE
);

CREATE TABLE settings (
    key TEXT PRIMARY KEY NOT NULL,
    value_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
