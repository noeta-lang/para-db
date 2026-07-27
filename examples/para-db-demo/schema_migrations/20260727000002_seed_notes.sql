-- Raw SQL still works, unchanged, beside a `.schema` migration in the same directory: the two
-- interleave in one filename order. Anything the portable vocabulary does not cover lives here.
INSERT INTO notes (title, body, pinned) VALUES ('First note', 'Written by a raw-SQL migration', TRUE);
