ALTER TABLE items ADD COLUMN source_received_date TEXT CHECK (
    source_received_date IS NULL OR
    source_received_date GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
);

UPDATE items
SET source_received_date = substr(fetched_at, 1, 10)
WHERE source_type = 'email';

CREATE INDEX idx_items_source_received_date
    ON items (source_type, source_received_date);
