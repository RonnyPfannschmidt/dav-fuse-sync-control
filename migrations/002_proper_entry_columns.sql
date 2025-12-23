-- Drop old table with JSON data
DROP TABLE IF EXISTS directory_cache;

-- Create directories table
CREATE TABLE IF NOT EXISTS directories (
    path TEXT PRIMARY KEY NOT NULL,
    etag TEXT,
    cached_at INTEGER NOT NULL
);

-- Create entries table with proper columns
CREATE TABLE IF NOT EXISTS entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    directory_path TEXT NOT NULL,
    name TEXT NOT NULL,
    is_dir BOOLEAN NOT NULL,
    size INTEGER NOT NULL,
    modified INTEGER,
    FOREIGN KEY (directory_path) REFERENCES directories(path) ON DELETE CASCADE
);

-- Index for faster lookups
CREATE INDEX IF NOT EXISTS idx_directories_cached_at ON directories(cached_at);
CREATE INDEX IF NOT EXISTS idx_entries_directory ON entries(directory_path);
CREATE UNIQUE INDEX IF NOT EXISTS idx_entries_unique ON entries(directory_path, name);
