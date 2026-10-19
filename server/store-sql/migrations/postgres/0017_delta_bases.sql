-- The object a packed object is stored as a delta against (NULL when stored whole). Deleting a
-- base (redaction purge) first rewrites the objects that depend on it.
ALTER TABLE pack_locations ADD COLUMN base BYTEA;
