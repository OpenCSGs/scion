-- path_changes: one index instead of two. The primary key leads with (repo_id, path, generation),
-- which path history reads; cs_id keeps rows unique (a commit has exactly one generation).
CREATE TABLE path_changes_new (
    repo_id    INTEGER NOT NULL REFERENCES repos (repo_id),
    path       TEXT    NOT NULL,
    cs_id      BLOB    NOT NULL,
    generation INTEGER NOT NULL,
    deleted    INTEGER NOT NULL,
    PRIMARY KEY (repo_id, path, generation, cs_id)
) WITHOUT ROWID;
INSERT INTO path_changes_new (repo_id, path, cs_id, generation, deleted)
    SELECT repo_id, path, cs_id, generation, deleted FROM path_changes;
DROP TABLE path_changes;
ALTER TABLE path_changes_new RENAME TO path_changes;

-- Pack names are stored once; locations refer to them by number.
CREATE TABLE packs (
    pack_id INTEGER PRIMARY KEY,
    name    BLOB    NOT NULL UNIQUE
);
INSERT INTO packs (name) SELECT DISTINCT pack FROM pack_locations;
CREATE TABLE pack_objects (
    id          BLOB    PRIMARY KEY,
    pack_id     INTEGER NOT NULL,
    pack_offset INTEGER NOT NULL,
    len         INTEGER NOT NULL,
    base        BLOB
) WITHOUT ROWID;
INSERT INTO pack_objects (id, pack_id, pack_offset, len, base)
    SELECT l.id, p.pack_id, l.pack_offset, l.len, l.base
    FROM pack_locations l JOIN packs p ON p.name = l.pack;
DROP TABLE pack_locations;
ALTER TABLE pack_objects RENAME TO pack_locations;
