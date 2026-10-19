-- Where packed objects live: the pack's content hash and the byte range of the object's stored
-- bytes in it. Global like the objects themselves; objects stored on their own have no row.
CREATE TABLE pack_locations (
    id          BLOB    PRIMARY KEY,
    pack        BLOB    NOT NULL,
    pack_offset INTEGER NOT NULL,
    len         INTEGER NOT NULL
) WITHOUT ROWID;
