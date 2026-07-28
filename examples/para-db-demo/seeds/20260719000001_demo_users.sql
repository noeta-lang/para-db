-- Seed: re-runnable development data. Runs on every `noeta migrate seed` / `--seed`, each in its own
-- transaction, and is NOT tracked. `ON CONFLICT DO NOTHING` makes a second run a no-op, and both
-- SQLite and PostgreSQL accept it verbatim — so this raw-SQL seed is portable as written.
--
-- (`INSERT OR IGNORE` says the same thing, but is SQLite-only: PostgreSQL rejects it with
-- `syntax error at or near "OR"`. That is why this file does not use it.)
INSERT INTO users (id, name) VALUES (100, 'Grace') ON CONFLICT DO NOTHING;
INSERT INTO users (id, name) VALUES (101, 'Alan') ON CONFLICT DO NOTHING;
