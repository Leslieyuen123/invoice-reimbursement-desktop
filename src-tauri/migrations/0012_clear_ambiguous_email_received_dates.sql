UPDATE items
SET source_received_date = NULL
WHERE source_type = 'email'
  AND source_received_date = substr(fetched_at, 1, 10)
  AND julianday(created_at) < (
      SELECT julianday(installed_on)
      FROM _sqlx_migrations
      WHERE version = 11
  );
