-- Widen the item category domain to include 'other' ("其他").
--
-- The user's rule: when none of the four real categories (交通/餐饮/住宿/招待)
-- matches an invoice, classify it as "其他" instead of leaving the category
-- empty, so the invoice can still enter a reimbursement batch.
--
-- SQLite cannot alter a CHECK constraint, so `items` has to be rebuilt. The
-- app's connections enable foreign keys, so a plain rebuild is unsafe:
--   * dropping `items` while `item_semantic_identities` still references it
--     would cascade-delete every semantic fingerprint row;
--   * dropping `items` while the copied rows already reference it through
--     `duplicate_of_id` would SET NULL every duplicate link.
-- Both tables are therefore staged in TEMP tables for the duration of the
-- rebuild, which also keeps the migration valid inside sqlx's migration
-- transaction (toggling `PRAGMA foreign_keys` would be a no-op there).
--
-- The rebuilt table reproduces the schema left by 0001 + 0002 + 0011 exactly
-- (column order, defaults, CHECKs, foreign keys); only the two category CHECKs
-- gain 'other'. Every index added by 0001/0002/0007/0011 is recreated.

CREATE TEMP TABLE items_category_rebuild AS SELECT * FROM items;
CREATE TEMP TABLE item_semantic_identities_category_rebuild AS
    SELECT * FROM item_semantic_identities;

DROP TABLE item_semantic_identities;
DROP TABLE items;

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
            'transport', 'dining', 'accommodation', 'hospitality', 'other'
        )
    ),
    final_category TEXT CHECK (
        final_category IS NULL OR final_category IN (
            'transport', 'dining', 'accommodation', 'hospitality', 'other'
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
    source_uid_validity INTEGER,
    source_received_date TEXT CHECK (
        source_received_date IS NULL OR
        source_received_date GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
    ),
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

INSERT INTO items (
    id, original_name, original_path, normalized_pdf_path, sha256, mime_type,
    source_type, source_account_id, source_mailbox, source_uid,
    source_message_id, source_part_id, fetched_at, invoice_date,
    suggested_period, batch_id, suggested_category, final_category,
    amount_cents, currency, city, company, recognition_status,
    confirmation_status, dedupe_status, duplicate_of_id, note, event_tag,
    project_tag, created_at, updated_at, source_uid_validity,
    source_received_date
)
SELECT
    id, original_name, original_path, normalized_pdf_path, sha256, mime_type,
    source_type, source_account_id, source_mailbox, source_uid,
    source_message_id, source_part_id, fetched_at, invoice_date,
    suggested_period, batch_id, suggested_category, final_category,
    amount_cents, currency, city, company, recognition_status,
    confirmation_status, dedupe_status, duplicate_of_id, note, event_tag,
    project_tag, created_at, updated_at, source_uid_validity,
    source_received_date
FROM items_category_rebuild;

DROP TABLE items_category_rebuild;

CREATE INDEX idx_items_work_queue
    ON items (dedupe_status, recognition_status, confirmation_status);
CREATE INDEX idx_items_period ON items (suggested_period);
CREATE INDEX idx_items_batch ON items (batch_id);
CREATE INDEX idx_items_hash ON items (sha256);
CREATE UNIQUE INDEX idx_items_email_part
    ON items (
        source_account_id,
        source_mailbox,
        source_uid_validity,
        source_uid,
        source_part_id
    )
    WHERE source_type = 'email';
CREATE INDEX idx_items_created_id
    ON items (created_at DESC, id DESC);
CREATE INDEX idx_items_source_received_date
    ON items (source_type, source_received_date);

CREATE TABLE item_semantic_identities (
    item_id TEXT PRIMARY KEY NOT NULL,
    fingerprint TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (item_id) REFERENCES items(id) ON DELETE CASCADE
);

INSERT INTO item_semantic_identities (
    item_id, fingerprint, created_at, updated_at
)
SELECT item_id, fingerprint, created_at, updated_at
FROM item_semantic_identities_category_rebuild;

DROP TABLE item_semantic_identities_category_rebuild;

CREATE INDEX idx_item_semantic_identities_fingerprint
    ON item_semantic_identities(fingerprint, created_at, item_id);
