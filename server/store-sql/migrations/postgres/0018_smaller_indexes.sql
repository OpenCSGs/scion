-- path_changes: one index instead of two. The primary key leads with (repo_id, path, generation),
-- which path history reads; cs_id keeps rows unique (a commit has exactly one generation).
ALTER TABLE path_changes DROP CONSTRAINT path_changes_pkey;
DROP INDEX path_changes_by_generation;
ALTER TABLE path_changes ADD PRIMARY KEY (repo_id, path, generation, cs_id);

-- Pack names are stored once; locations refer to them by number, with 4-byte offsets (packs are
-- at most a few dozen MiB).
CREATE TABLE packs (
    pack_id INTEGER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name    BYTEA   NOT NULL UNIQUE
);
INSERT INTO packs (name) SELECT DISTINCT pack FROM pack_locations;
CREATE TABLE pack_objects (
    id          BYTEA   PRIMARY KEY,
    pack_id     INTEGER NOT NULL,
    pack_offset INTEGER NOT NULL,
    len         INTEGER NOT NULL,
    base        BYTEA
);
INSERT INTO pack_objects (id, pack_id, pack_offset, len, base)
    SELECT l.id, p.pack_id, l.pack_offset::INTEGER, l.len::INTEGER, l.base
    FROM pack_locations l JOIN packs p ON p.name = l.pack;
DROP TABLE pack_locations;
ALTER TABLE pack_objects RENAME TO pack_locations;
