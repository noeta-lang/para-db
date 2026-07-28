-- A seed for the SELF-SEEDING path (`conn.seed(dir)` at boot, see ../seed_demo.noe), so this
-- directory is deliberately SQL-only: a program that seeds itself has no CLI to load and run a
-- `.noe` seed with. The seeds directory the `noeta migrate` command drives (../seeds) has both.
--
-- `ON CONFLICT DO NOTHING` is the portable idempotent insert — SQLite and PostgreSQL both take it
-- verbatim, so a re-run is a no-op on either backend.
INSERT INTO users (id, name) VALUES (100, 'Grace') ON CONFLICT DO NOTHING;
INSERT INTO users (id, name) VALUES (101, 'Alan') ON CONFLICT DO NOTHING;
