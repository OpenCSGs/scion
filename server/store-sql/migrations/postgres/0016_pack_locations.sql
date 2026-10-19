-- Where packed objects live: the pack's content hash and the byte range of the object's stored
-- bytes in it. Global like the objects themselves; objects stored on their own have no row.
CREATE TABLE pack_locations (
    id          BYTEA  PRIMARY KEY,
    pack        BYTEA  NOT NULL,
    pack_offset BIGINT NOT NULL,
    len         BIGINT NOT NULL
);
