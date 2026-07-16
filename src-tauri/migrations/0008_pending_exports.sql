CREATE TABLE pending_exports (
    operation_id TEXT PRIMARY KEY NOT NULL,
    batch_id TEXT NOT NULL,
    staging_component TEXT NOT NULL UNIQUE CHECK (
        length(trim(staging_component)) > 0
    ),
    final_component TEXT NOT NULL UNIQUE CHECK (
        length(trim(final_component)) > 0
    ),
    exported_at TEXT NOT NULL,
    state TEXT NOT NULL CHECK (
        state IN ('generating', 'published', 'committed')
    ),
    interrupted INTEGER NOT NULL DEFAULT 0 CHECK (interrupted IN (0, 1)),
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_pending_exports_batch
    ON pending_exports (batch_id);
