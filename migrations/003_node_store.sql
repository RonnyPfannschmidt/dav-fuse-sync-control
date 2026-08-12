-- v3: a backend-neutral node store.
--
-- Replaces the WebDAV-path-shaped directories/entries pair. Both held nothing
-- but re-fetchable cache, so they are dropped rather than migrated.
--
-- The load-bearing choice is that `nodes` is FLAT and id-keyed with a single
-- nullable parent_id, rather than a per-directory child list. A cursor-based
-- backend (Google Drive) delivers a change as a file id with no indication of
-- which cached directory it belongs to, and a *move* has to invalidate both the
-- old and the new parent -- the old one being knowable only from our own row.
-- Path-keyed rows make that a full-table scan per change.

DROP TABLE IF EXISTS entries;
DROP TABLE IF EXISTS directories;

CREATE TABLE nodes (
    remote_id          TEXT PRIMARY KEY NOT NULL,
    parent_id          TEXT REFERENCES nodes(remote_id) ON DELETE CASCADE,

    -- As the backend stores it. May contain '/', '.' or '..'.
    name               TEXT NOT NULL,
    -- Sanitised, export-extended and de-duplicated. What the kernel is shown.
    display_name       TEXT NOT NULL,

    kind               INTEGER NOT NULL,   -- 0 file 1 dir 2 symlink 3 export-only 4 other
    -- NULL means SizeHint::Unknown (a Google-native doc before its first
    -- export). Distinct from 0, which means a genuinely empty file.
    size               INTEGER,
    mtime              INTEGER,            -- unix seconds UTC
    ctime              INTEGER,
    mime               TEXT,

    -- Change detection: exactly one validator, tagged by kind.
    validator_kind     INTEGER,            -- 0 etag 1 version 2 checksum 3 size_mtime
    validator          TEXT,
    checksum_algo      TEXT,
    checksum           TEXT,

    -- Directory listing bookkeeping. NULL for files.
    children_validator      TEXT,
    children_validator_kind INTEGER,
    listed_at          INTEGER,            -- NULL = never listed
    -- 0 when the listing was a partial page: absent children must NOT be
    -- deleted, or a paginated directory loses everything past the first page.
    listing_complete   INTEGER NOT NULL DEFAULT 1,

    -- Content cache bookkeeping. NULL for directories.
    content_key        TEXT,               -- blob name; NULL = no local content
    content_len        INTEGER,
    content_validator  TEXT,               -- the validator the blob was fetched at
    pinned             INTEGER NOT NULL DEFAULT 0,
    accessed_at        INTEGER,            -- LRU

    -- Write path. The columns exist now; only state 0 is reachable this round.
    sync_state         INTEGER NOT NULL DEFAULT 0,  -- 0 clean 1 dirty 2 uploading
                                                    -- 3 conflict 4 error
    local_mtime        INTEGER,
    last_error         TEXT,

    target_id          TEXT,               -- shortcut indirection (Drive)

    fetched_at         INTEGER NOT NULL    -- when this row's metadata was refreshed
) STRICT;

-- Unique on display_name, deliberately NOT on name: Google Drive permits two
-- files with the same name in one folder, and the naming pipeline is what makes
-- them distinguishable.
CREATE UNIQUE INDEX idx_nodes_parent_display ON nodes(parent_id, display_name);
CREATE INDEX idx_nodes_parent  ON nodes(parent_id);
CREATE INDEX idx_nodes_lru     ON nodes(accessed_at) WHERE content_key IS NOT NULL;
CREATE INDEX idx_nodes_dirty   ON nodes(sync_state)  WHERE sync_state <> 0;
CREATE INDEX idx_nodes_listed  ON nodes(listed_at)   WHERE kind = 1;

-- Stable inode numbers across remounts.
--
-- Deliberately NOT foreign-keyed to nodes: an inode must outlive a metadata
-- purge, which is the entire point of persisting it. ino 1 is seeded in Rust to
-- match FUSE_ROOT_ID; AUTOINCREMENT then continues from 2.
CREATE TABLE inodes (
    ino       INTEGER PRIMARY KEY AUTOINCREMENT,
    remote_id TEXT NOT NULL UNIQUE
) STRICT;

-- Account-scoped state: the backend discriminant, the root id, and a delta
-- cursor for backends that have one.
CREATE TABLE account (
    k TEXT PRIMARY KEY NOT NULL,
    v TEXT
) STRICT;
