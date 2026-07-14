ALTER TABLE items ADD COLUMN source_uid_validity INTEGER;

UPDATE items
SET source_uid_validity = 0
WHERE source_type = 'email';

DROP INDEX idx_items_email_part;

CREATE UNIQUE INDEX idx_items_email_part
    ON items (
        source_account_id,
        source_mailbox,
        source_uid_validity,
        source_uid,
        source_part_id
    )
    WHERE source_type = 'email';
