-- Create directory cache table
CREATE TABLE IF NOT EXISTS directory_cache (
    path TEXT PRIMARY KEY NOT NULL,
    entries_json TEXT NOT NULL,
    etag TEXT,
    cached_at INTEGER NOT NULL
);

-- Index for faster lookups
CREATE INDEX IF NOT EXISTS idx_cached_at ON directory_cache(cached_at);
