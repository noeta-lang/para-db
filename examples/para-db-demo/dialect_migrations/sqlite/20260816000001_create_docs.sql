-- SQLite's spelling of the same migration — selected because this file's name matches a migration
-- directly above it and its directory names the connected driver's dialect.
--
-- It keeps that migration's NAME, so it keeps its ordinal and its row in `_noeta_migrations`: every
-- backend runs the same migrations in the same order, and only the SQL differs. What the tracking
-- table records is this body's checksum, because this body is what ran.
CREATE TABLE docs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    body TEXT NOT NULL DEFAULT '{}'
);
