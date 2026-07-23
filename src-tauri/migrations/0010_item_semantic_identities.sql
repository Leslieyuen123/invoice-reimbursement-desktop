CREATE TABLE item_semantic_identities (
    item_id TEXT PRIMARY KEY NOT NULL,
    fingerprint TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY (item_id) REFERENCES items(id) ON DELETE CASCADE
);

CREATE INDEX idx_item_semantic_identities_fingerprint
    ON item_semantic_identities(fingerprint, created_at, item_id);
