-- A portable migration, with no override: it reads the same on every backend, so it is written once
-- and every backend runs this file. Overrides are for the steps that genuinely diverge; most
-- migrations are this one.
--
-- The identity column it leaves to the database is exactly what the two spellings of migration 0001
-- have in common, so this row proves both of them assign a key.
INSERT INTO docs (body) VALUES ('{"kind":"portable"}');
