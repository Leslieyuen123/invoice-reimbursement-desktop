CREATE INDEX idx_items_created_id
    ON items (created_at DESC, id DESC);

CREATE INDEX idx_batches_updated_id
    ON batches (updated_at DESC, id DESC);
