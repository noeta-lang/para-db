-- The base body: PostgreSQL's spelling of "a document table with a generated key".
--
-- `BIGSERIAL` and the `::jsonb` cast are Postgres SQL, and SQLite cannot even parse the second of
-- them — which is the point of the file beside this one. A migration body runs VERBATIM in the
-- connected dialect, so a step neither the schema DSL nor portable SQL can express is written once
-- per backend: here, and in `sqlite/` under the same filename.
CREATE TABLE docs (
    id BIGSERIAL PRIMARY KEY,
    body JSONB NOT NULL DEFAULT '{}'::jsonb
);
