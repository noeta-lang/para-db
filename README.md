# para/db

The first-party database layer for Noeta — a native swappable driver plus a pure-Noeta query builder, repository / unit-of-work, and a typed `@sql` block tier.

## What it provides

- **`para.db`** (native) — `db.connect(dsn)` → `Connection`; the dsn scheme selects the driver:
  - `sqlite::memory:` / `:memory:` — in-memory SQLite
  - `sqlite:PATH` (or a bare path) — a SQLite file
  - `postgres://user:pass@host:5432/db` (`postgresql://` too) — PostgreSQL
- **`Connection`** — `execute(sql, params) -> int` / `query(sql, params) -> List<Map<string, dyn>>` with positional `?` bind parameters (rewritten per driver; never string-spliced, so no injection risk), plus `notify(channel)` (fire a change notification: Postgres `NOTIFY`, an in-process bus publish on SQLite), `migrate(dir)`, `seed(dir)`, `close()`.
- **`para.db.query`** (pure Noeta) — a fluent query builder: `table("users").filter("age", ">", 18).order("name", "asc").limit(20)`.
- **`para.db.schema`** (pure Noeta) — a portable schema builder, the DDL peer of the query builder: `create_table("todos").id().text("title").bool("done").default(false).timestamps()`, lowered to each driver's own DDL. The same notation is the body of a `.schema` migration.
- **`para.db.repo`** (pure Noeta) — repository + unit-of-work: stage writes during a request, flush them as one transactional batch.
- **`para.db.sql`** (pure Noeta) — the typed `@sql { … }` block tier: `${…}` holes are always bound parameters, never spliced.
- **`para.db.reactive`** (pure Noeta) — `LiveRepository` + `db.watch`: reactive queries that re-run when the data changes (SQLite update hooks / Postgres `LISTEN`/`NOTIFY`).
- **The `noeta migrate` command** — this package contributes a migration/seed engine to the CLI; a consumer opts in with `[trust] commands = ["para/db"]`.
- **`editors/sql-tier.tmLanguage.json`** — a TextMate injection grammar so `@sql` bodies highlight as SQL in editors without the official extension.

## Installation

```toml
[dependencies]
para = { version = "^0.1", package = "para/db" }

[trust]
native = ["para/db"]     # authorizes the package's native driver crate
commands = ["para/db"]   # opts in to the `noeta migrate` CLI command
```

The package is keyed `para`, so its modules address as `para.db`, `para.db.query`, `para.db.schema`, `para.db.repo`, `para.db.sql`, and `para.db.reactive`. Optionally add a `[db]` table for `noeta migrate` (see below).

## Usage

```noeta
use para.db
use para.db.sql
use para.db.sql.{query}

conn = db.connect("sqlite::memory:")
conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)", [])
conn.execute("INSERT INTO users (id, name) VALUES (?, ?)", [1, "Ada"])

min_id = 0
rows = query(conn, @sql { SELECT * FROM users WHERE id > ${min_id} })
echo rows.len()
```

A `${…}` hole in an `@sql { … }` block is **always a bound parameter**: the block evaluates to a `Sql` value — the statement `text` with `?` placeholders plus its `params` in hole order — so a statement built from untrusted input carries no injection risk by construction. `query(conn, stmt)` runs it as a query; its sibling `execute(conn, stmt)` runs a non-query (INSERT/UPDATE/DELETE/DDL), returning rows affected. A bound parameter is a scalar — `int`, `float`, `bool`, `string`, or `none` (SQL `NULL`) — and a row comes back as a `Map<string, dyn>` keyed by column name, with `NULL` as `none`. Transactions are ordinary statements: `conn.execute("BEGIN", [])` / `"COMMIT"` / `"ROLLBACK"`.

**Swapping drivers is the dsn.** Everything above the driver — this raw surface, the query builder, the repository, `@sql`, migrations — runs unchanged over SQLite or PostgreSQL: `postgres_demo.noe` is `demo.noe` with only the connection string changed. The neutral `?` placeholders are rewritten to Postgres's `$1, $2, …` by the driver.

## Query builder — compose statements fluently

`para.db.query` composes a `Query` — statement text with `?` placeholders plus its ordered bound parameters. `table(name)` starts a builder; `filter(col, op, value)` (ANDed with the others), `order(col, dir)` (`"asc"`/`"desc"`), and `limit(n)` chain; a terminal `select(cols)`, `insert(columns, values)`, `update(columns, values)`, or `delete()` builds the `Query`. `run(conn, q)` executes a query, returning its rows; `exec(conn, q)` executes a write, returning rows affected.

```noeta
use para.db
use para.db.query.{table, run, exec}

conn = db.connect("sqlite::memory:")
conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)", [])

ins = table("users").insert(["name", "age"], ["Ada", 36])
exec(conn, ins)

q = table("users").filter("age", ">", 30).order("age", "asc").limit(10).select("name, age")
rows = run(conn, q)        // List<Map<string, dyn>>
```

In an `update(columns, values)`, the SET bindings come first and the builder's filter bindings follow; `delete()` binds this builder's filters. Filter values become bound parameters, so a query built from untrusted input carries no injection risk.

## Schema DSL — portable DDL, lowered per driver

The query builder solves portability for *statements*: it emits neutral `?` placeholders and each driver rewrites them into its own binding syntax. `para.db.schema` is the same idea one level up, for *schema*: a table is described backend-neutrally and each driver lowers that description into its own DDL. The one description creates the table on SQLite and on PostgreSQL — so a project can develop on a SQLite file and deploy on Postgres from one set of migrations.

```noeta
use para.db
use para.db.schema.{create_table, create_index, apply}

conn = db.connect("sqlite::memory:")        // or postgres://… — nothing below changes

apply(conn, [
    create_table("todos")
        .id()                               // driver-appropriate auto-assigned primary key
        .text("title").not_null()
        .bool("done").default(false)
        .timestamps()                       // created_at + updated_at
        .render(),
    create_index("todos").column("done").render(),
])
```

`id()` lowers to `INTEGER PRIMARY KEY AUTOINCREMENT` on SQLite and `BIGSERIAL PRIMARY KEY` on PostgreSQL; `float` lowers to `REAL` or `DOUBLE PRECISION`. Everything else in the vocabulary is spelled identically on both. **The lowering lives in the driver** (`SqlDriver::lower_schema`), exactly where the `?`→`$N` rewrite lives, so nothing above the driver seam branches on the backend — and a third driver gets the whole DSL by naming its dialect.

A builder describes a **`Statement`** — the same backend-neutral IR the native side parses a `.schema` file into, with `Statement`, `CreateTable`, `Column`, `DefaultValue` and the rest declared in Noeta field for field against their `schema.rs` twins. `.statement()` hands that value over, and there are two renderings of it:

- **`.render()`** — schema-DSL **source**, laid out the way a migration file is written. `echo create_table("todos").id().render()` prints something you can paste straight into `migrations/`.
- **`.canonical()`** — the **canonical** rendering, one statement per line in one fixed shape, byte for byte what the native `schema::render` produces from the same statement. It is what a `.schema` migration is *checksummed* over, so a migration's identity is its meaning rather than its formatting.

There is one grammar and one lowering, both native, and a test renders a corpus of builder expressions both ways to hold the Noeta IR and the native IR to one canonical text — so the builder and the migration file cannot drift apart. `apply(conn, statements)` is for schema you build at runtime — a test fixture, a scratch database; for a durable change, write a `.schema` **migration** (below), which is checksummed, tracked, and applied exactly once.

### The vocabulary

The tables below name the **Noeta builder** methods. The `.schema` file notation is the same, with the one difference that a list argument is spelled as plain arguments: `primary_key(["a", "b"])` in Noeta is `primary_key("a", "b")` in a file. Every builder also has a `link(name, args)` escape hatch that applies an arbitrary DSL call to the statement under construction, so a call the Noeta surface has not wrapped is still reachable; a name outside the vocabulary is refused there rather than at apply time.

| Statement | Chain |
| --- | --- |
| `create_table(name)` | columns, column modifiers, `primary_key([…])`, `unique([…])`, `if_not_exists()` |
| `alter_table(name)` | `add_text/int/bigint/float/bool/timestamp(name)` (+ modifiers), `drop_column(name)`, `rename_column(from, to)`, `rename_to(name)` |
| `drop_table(name)` | `if_exists()` |
| `create_index(table)` | `column(name)`, `columns([…])`, `name(index)`, `unique()`, `if_not_exists()` |
| `drop_index(name)` | `if_exists()` |

| Column | SQLite | PostgreSQL |
| --- | --- | --- |
| `id()` | `INTEGER PRIMARY KEY AUTOINCREMENT` | `BIGSERIAL PRIMARY KEY` |
| `text(n)` | `TEXT` | `TEXT` |
| `int(n)` | `INTEGER` | `INTEGER` |
| `bigint(n)` | `BIGINT` | `BIGINT` |
| `float(n)` | `REAL` | `DOUBLE PRECISION` |
| `bool(n)` | `BOOLEAN` | `BOOLEAN` |
| `timestamp(n)` | `TIMESTAMP` | `TIMESTAMP` |
| `timestamps()` | `created_at` + `updated_at`, both `TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP` | same |

Column modifiers: `not_null()`, `default(literal)` (string / int / float / bool), `default_now()` (`CURRENT_TIMESTAMP`), `unique()`, `primary_key()`, and `references(table, column)` with `on_delete(action)` / `on_update(action)` (`"cascade"`, `"restrict"`, `"set_null"`, `"set_default"`, `"no_action"`). At table level, `primary_key(["a", "b"])` and `unique(["a", "b"])` are composite constraints. A column is **nullable unless you say otherwise**, exactly as in SQL — the DSL never invents a default SQL does not have. Use `bigint` for a foreign key onto an `id()` key: that is the width `id()` produces on both backends.

A statement ends where its chain does — the next top-level call starts a new statement, so no separator is needed. Line comments are `//` or `--`, whichever reads better in a given file.

Identifiers are **validated, never quoted**: letters, digits, and underscores, starting with a letter or underscore. That is one rule doing two jobs — a name needing quotes would behave differently under Postgres's case folding than under SQLite's case preservation, and an unquotable name cannot be spliced into DDL by an attacker.

### What is deliberately not portable

The DSL covers what lowers to genuinely equivalent DDL on both backends. Everything else is left to a raw `.sql` migration rather than approximated — a `jsonb` column silently becoming SQLite `TEXT` would compile and then behave differently, which is worse than not offering it. Out of scope, by design:

- **Types with no honest counterpart** — `uuid`, `json`/`jsonb`, `bytea`/`blob`, and exact `decimal`. (SQLite has no exact-decimal type at all; `NUMERIC(10,2)` there is an affinity hint over a float.) The neutral value surface is `int` / `float` / `bool` / `string` / `null`, and the DSL offers only column types that round-trip through it.
- **Everything that is not a table or an index** — views, triggers, functions, sequences, extensions, schemas.
- **Check constraints, partial and expression indexes, generated columns, `DROP … CASCADE`.**
- **Arbitrary expression defaults.** Only literals and `CURRENT_TIMESTAMP`.
- **Length-bounded text** (`VARCHAR(n)`). Both backends accept the syntax, but SQLite does not enforce the bound — a constraint on one backend and a comment on the other is exactly the kind of approximation this DSL refuses. Use `text`.

Two portability facts the DSL surfaces rather than hides:

- **`ALTER TABLE ADD COLUMN` is much narrower on SQLite than on Postgres.** Adding a `not_null` column without a `default(…)`, a `unique()` or `primary_key()` column, a column whose default is `default_now()`, or an identity column are all **rejected at parse time**, with a message naming the portable alternative (for uniqueness: add the column, then `create_index(…).unique()`). Emitting DDL that works on Postgres and fails on SQLite would make a "portable" migration backend-dependent.
- **A `bool` column reads back differently.** SQLite has no boolean storage class, so it comes back as `0`/`1`; Postgres returns `true`/`false`. That is the existing driver value mapping, not something the schema layer can paper over. Likewise a `TIMESTAMP` column must be selected as `CAST(col AS TEXT)` on Postgres to cross the neutral row surface — the migration engine's own tracking-table read does exactly that.

Foreign keys are emitted on both backends, but **SQLite only enforces them when `PRAGMA foreign_keys = ON`** is set on the connection (it is off by default). The clause is still worth writing: it documents the relationship and is enforced by Postgres.

## Repository & unit of work — typed models, batched writes

`para.db.repo` maps rows to and from a typed model struct by reflection over JSON: the model derives both `Serialize<Json>` (model → columns) and `Deserialize<Json>` (row → model), and the repository is constructed with the model's runtime type **name** — `type_name::<User>()` — its table, and its primary-key column. The name is a string because generics are erased: a `Repository<T>` cannot recover `T` at run time, and the decode registry rows map through is keyed by name. `type_name::<T>()` supplies it as the **qualified** identity that registry holds, resolved at compile time — so a model under a `namespace` needs no hand-written `"app.storage.User"`, and renaming or moving the model cannot silently desynchronize it. Reads go straight to the connection — `find(conn, id)` (a `?dyn`; `none` when absent), `all(conn)`, and `where(conn, col, op, value)`, each mapped to the model; narrow a result with `.as<User>()`. Writes are the **unit of work**: `add(entity)` / `save(entity)` / `remove(id)` *stage* an insert / a by-primary-key update / a delete, and `flush(conn)` commits everything staged as one batch inside a single transaction (`BEGIN` … `COMMIT`), returning the statement count — a failure before `COMMIT` leaves the batch to the transaction to undo. `discard()` drops the staged changes without touching the database.

```noeta
use para.db
use para.db.repo.Repository

@derive(Serialize<Json>, Deserialize<Json>)
struct User {
    id: int
    name: string
    age: int
}

conn = db.connect("sqlite::memory:")
conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)", [])
users = Repository.new("User", "users", "id")

users.add(User { id: 1, name: "Ada", age: 36 })
users.add(User { id: 2, name: "Bob", age: 41 })
n = users.flush(conn)                       // one transaction, 2 statements

users.save(User { id: 2, name: "Bob", age: 42 })    // stage an UPDATE by primary key
users.remove(1)                                     // stage a DELETE
users.flush(conn)

adults = users.where(conn, "age", ">", 18)
```

## Migrations — evolve the schema over time

`para/db` ships a migration engine, surfaced as the `noeta migrate` command **this package contributes to the CLI** (trust it with `[trust] commands = ["para/db"]`) and the programmatic `conn.migrate(dir)` method. There is one engine (in `noeta-para-db`); the command and the Noeta method are thin callers, so both drivers migrate through the same code.

**Migrations are files** in a project `migrations/` directory, one statement or many per file, applied in the order their filenames sort. `noeta migrate new <name>` scaffolds the next file with a **UTC-timestamp prefix** — `YYYYMMDDHHMMSS_<name>.sql`. Timestamps are the default because they never collide when two branches each add a migration (a sequential `0007_…` would); the engine sorts lexicographically over the whole filename, so any monotonic scheme (including zero-padded sequence numbers) also works.

**A migration's extension picks its body language**, and the two kinds live in the same directory under one ordering:

| File | Body | Applied as |
| --- | --- | --- |
| `<name>.sql` | native SQL for the connected database | run **verbatim** — no `?`→`$N` rewrite, no translation |
| `<name>.schema` | the [portable schema DSL](#schema-dsl--portable-ddl-lowered-per-driver) | **lowered** to the connected driver's DDL just before it applies |

A project can therefore write portable migrations wherever the vocabulary reaches and drop to raw SQL for the steps it does not. `noeta migrate new <name> --schema` scaffolds a `.schema` migration; plain `noeta migrate new <name>` still scaffolds `.sql`. Raw SQL stays the **default** deliberately: the DSL cannot express everything SQL can, so the scaffold should not imply it does, and every existing project's `migrate new` keeps behaving exactly as before.

```
migrations/
  20260727000001_create_notes.schema     # portable: create_table("notes").id().text("title")…
  20260727000002_backfill.sql            # raw SQL: whatever the DSL does not cover
  20260727000003_add_archived.schema     # portable: alter_table("notes").add_bool("archived")…
```

**A tracking table `_noeta_migrations`** records, for each applied migration, its `filename`, a **sha256 `checksum`**, and `applied_at`. The checksum is taken over what the migration *means*, never over the lowered DDL:

- A **`.sql`** migration is hashed over its **file source**. Raw SQL *is* the DDL — the engine does not parse it, so the bytes the author wrote are the identity.
- A **`.schema`** migration is hashed over the **canonical rendering of its parsed statements**: source → parse → the neutral IR → canonical re-render → sha256. Formatting is gone by the time the hash is taken, so re-indenting a file, wrapping a chain differently, or fixing a typo in a comment does **not** read as tampered history — while a renamed column, a changed default, an added constraint, or a reordered statement always does.

Hashing the generated DDL instead would be worse on both counts: it is backend-dependent (`INTEGER PRIMARY KEY AUTOINCREMENT` here, `BIGSERIAL PRIMARY KEY` there), so one migration would have two identities — and it would turn any future improvement to the lowering into "history was edited" for every project that had already run it. The checksum is taken **before** lowering and never sees a dialect, so one `.schema` file has one identity across SQLite and PostgreSQL and the code generator stays free to improve.

Two integrity checks run before anything is applied, both hard errors that name the file:

- **Checksum drift** — an already-applied migration's file was edited. History is immutable; revert the edit or make the change in a new migration. (For a `.schema` file, only a change to what it *does* counts — reformat it freely.)
- **Deleted applied migration** — a file recorded as applied is gone. Restore it, or `--reset` in development.

A `.schema` body is parsed and lowered **before** its transaction opens, so a DSL syntax error stops the run with the file and line named and nothing touched.

**Transactionality.** Each migration runs inside its own transaction — `BEGIN`, the file body, the tracking-row insert, `COMMIT`. The first failure rolls that migration back and stops, reporting the exact file. Postgres has fully transactional DDL; SQLite is transactional for the ordinary DDL migrations use — so a migration is all-or-nothing, and a failed run leaves every prior migration applied. (Do not put `BEGIN`/`COMMIT` in a migration file — the runner owns the transaction.)

**Forward-only.** There are deliberately no down/rollback files: a down migration is routinely wrong against real production data — the production answer is to roll *forward* with a new migration. Development uses `--reset` (drop the schema and re-apply from zero), and **seeds** (below) refill the rebuilt schema with dev data — so `noeta migrate --reset --seed` is the whole development loop. `--reset` is destructive and driver-specific: on SQLite it drops every user table/view/trigger; on PostgreSQL it runs `DROP SCHEMA public CASCADE; CREATE SCHEMA public` (the `public` schema only).

## Seeds — re-runnable development data

Where a migration is immutable schema *history*, a **seed** is throwaway development *data*: sample rows to develop and demo against. Seeds are plain `.sql` files in a project `seeds/` directory, discovered and ordered by the very same filename-sort convention migrations use, and applied **after** migrations.

The rerun semantics are deliberately honest and different from migrations. **Seeds run in filename order, each in its own transaction, every time they are invoked — and are never recorded in the tracking table.** They are not history, so there is no checksum and no "already applied" skip: running seeds twice runs every file twice. Idempotency is therefore the seed author's job — write inserts that a re-run turns into a no-op:

```sql
-- SQLite
INSERT OR IGNORE INTO users (id, name) VALUES (1, 'Ada');
-- PostgreSQL
INSERT INTO users (id, name) VALUES (1, 'Ada') ON CONFLICT DO NOTHING;
```

A mid-seed failure stops at that file (naming it) with the prior seeds committed — the same stop-on-first-failure shape as migrations, minus the tracking.

### CLI

```
noeta migrate                      # apply every pending migration, printing each applied file
noeta migrate --status             # table of applied / pending migrations
noeta migrate --dry-run            # list what would be applied, without touching the database
noeta migrate new <name>           # scaffold migrations/<timestamp>_<name>.sql  (raw SQL)
noeta migrate new <name> --schema  # scaffold migrations/<timestamp>_<name>.schema  (portable DSL)
noeta migrate new --seed <name>    # scaffold seeds/<timestamp>_<name>.sql
noeta migrate --seed               # apply pending migrations, then run the seed files
noeta migrate seed                 # run the seed files ONLY (errors if any migration is pending)
noeta migrate --reset --yes        # DESTRUCTIVE: drop the schema and re-apply from zero
noeta migrate --reset --seed --yes # the full dev loop: reset -> migrate -> seed
```

`noeta migrate seed` refuses to run when any migration is still pending — seeding a stale schema is a footgun; run `noeta migrate` first, or `noeta migrate --seed` to migrate then seed in one step. The connection string is resolved, highest priority first, from the `--db <dsn>` flag, the `DATABASE_URL` environment variable, then a `[db]` table in `noeta.toml`:

```toml
[db]
url = "sqlite:app.db"        # or postgres://…  — the same dsn schemes db.connect accepts
migrations = "migrations"    # optional; the directory (default "migrations"), overridable with --dir
seeds = "seeds"              # optional; the directory (default "seeds"), overridable with --seeds-dir
```

`--reset` refuses to run without either `--yes` or an interactive `yes` typed at the prompt.

### At boot (self-migrating apps)

An aether server (or any program) can migrate itself at startup off the same engine, and optionally seed after — seeding is never implicit, an app opts in explicitly and controls the order (migrate, then seed):

```noeta
conn = db.connect(env.get("DATABASE_URL") ?? "sqlite:app.db")
applied = conn.migrate("migrations")   // returns the count applied; a no-op when up to date
seeded  = conn.seed("seeds")           // runs every seed file every time; returns how many ran
```

`conn.migrate(dir)` applies every pending migration under `dir` — `.sql` and `.schema` alike — and returns how many it applied, with the same tracking table and integrity checks as the CLI. `conn.seed(dir)` runs every seed file under `dir` (untracked, re-runnable) and returns how many it ran. `conn.apply_schema(source)` applies schema-DSL source directly, without tracking it; that is what `para.db.schema`'s `apply` calls.

## TLS (PostgreSQL)

The Postgres driver uses a pure-Rust rustls connector (the `ring` crypto provider — no OpenSSL / C build — and the bundled Mozilla root store, so no system trust store is needed). The connection URL's `sslmode` parameter selects the behavior, mirroring libpq. Two independent security properties vary: whether the connection **must** be encrypted, and whether the server's certificate is **authenticated** (verified against the trust store).

| `sslmode` | Encrypted? | Certificate verified? | Notes |
| --- | --- | --- | --- |
| `disable` | ❌ | — | Always plaintext. Use only over an already-trusted local socket. |
| `prefer` *(default)* | when offered | ✅ (when TLS negotiated) | Try TLS and verify against the bundled roots, else fall back to plaintext. The safe default. |
| `require` | ✅ | ❌ | **Encrypted but NOT authenticated** — libpq parity. See the warning below. |
| `verify-ca` | ✅ | ✅ | Mandatory TLS, certificate verified against the bundled roots. |
| `verify-full` | ✅ | ✅ (incl. hostname) | Mandatory TLS, full certificate verification. The strongest mode. |

```noeta
conn = db.connect("postgres://user:pass@host:5432/db?sslmode=require")
```

> **`sslmode=require` is encrypted, not authenticated.** It negotiates TLS (so a passive eavesdropper on the wire sees only ciphertext) but does **not** verify the server's certificate — so it does **not** defend against an active man-in-the-middle who substitutes their own certificate. This matches libpq's `sslmode=require`, and is deliberately distinct from `verify-ca` / `verify-full`. Reach for it only when the network path to the server is already trusted (e.g. a private link) but the server presents a self-signed or otherwise unverifiable certificate. When the server has a real CA-issued certificate, prefer `verify-full`; the default `prefer` already verifies whenever TLS is negotiated.

An unrecognized `sslmode` value is a clear error before any connection is attempted.

## Reactive queries — keep the UI in sync with the database

`para.db` integrates with `std.reactive`, so a query can be a **reactive value**: when the data changes, the query re-runs and every dependent — an `effect`, a `computed`, a LiveView `view.expose(...)` — updates. Reactivity is **opt-in**: the plain `Repository` stays non-reactive and zero-overhead; you choose it by using `LiveRepository`.

```noeta
use para.db
use para.db.reactive.LiveRepository
use std.reactive.effect

conn = db.connect("sqlite::memory:")           // or postgres://…
users = LiveRepository.new("User", "users", "id", conn)

live = users.all()                             // a reactive query (a computed)
effect(fn() {
    echo "UI: ${live.get().len()} user(s)"     // re-renders whenever `users` changes
})

users.add(User { id: 1, name: "Ada", age: 36 })
users.flush()                                  // commit + notify
users.pump()                                   // deliver notifications → the effect re-runs
```

`LiveRepository` wraps a plain `Repository` with three additions: `all()` returns a reactive query, `flush()` notifies after committing, and `pump()` (called from your loop, e.g. the serve loop) delivers pending change notifications and wakes the reactive graph. Under the hood it composes `db.watch(conn, channel)` — a reactive source node over a change-notification channel — with a plain repository. The source is also usable directly: a `computed` that reads `watch.get()` and re-queries the table re-runs whenever a fired notification is pumped in with `watch.pump()` (see `watch_demo.noe`).

**How far a change propagates depends on the driver:**

| a write is seen by a reactive query… | in the same process | across parallel-serve workers (isolate threads of one process) | across separate OS processes |
| --- | --- | --- | --- |
| **SQLite** (per-connection update hook + a process bus) | ✅ | ✅ (only channel-name strings cross — `Send`-safe) | ❌ — SQLite has no server to push |
| **PostgreSQL** (`LISTEN`/`NOTIFY`) | ✅ | ✅ | ✅ |

For SQLite, a write through **any** connection in the process fires its update hook and wakes every `db.watch` on that table — no trigger or explicit `NOTIFY` needed. For PostgreSQL, a write from another process wakes the UI when the database `NOTIFY`s the channel: either `conn.notify("<table>")` from each writer, or a trigger so *any* writer fires it —

```sql
CREATE FUNCTION users_notify() RETURNS trigger AS $$ BEGIN PERFORM pg_notify('users', ''); RETURN NULL; END; $$ LANGUAGE plpgsql;
CREATE TRIGGER users_changed AFTER INSERT OR UPDATE OR DELETE ON users FOR EACH STATEMENT EXECUTE FUNCTION users_notify();
```

## Editor highlighting for `@sql`

**With the official Noeta VS Code extension (v0.9.0+), `@sql` highlights automatically** — it bundles injection for well-known languages by tier name, and `@sql`'s name is its `text:` language, so nothing extra is needed.

For **other editors** (or a custom setup), `@sql { … }` bodies highlight as SQL through a **one-rule TextMate injection grammar** — the standard mechanism for a package that declares a text/expression tier (see the Noeta VS Code extension's README, "Text tiers and embedded languages"). The core language grammar stays fixed; this attaches by textual match. This package ships that grammar at [`editors/sql-tier.tmLanguage.json`](editors/sql-tier.tmLanguage.json); contribute it from a VS Code extension's `contributes.grammars`:

```jsonc
{
  "scopeName": "inline.noeta.para-db.sql-tier",
  "path": "./sql-tier.tmLanguage.json",
  "injectTo": ["source.noeta"],
  "embeddedLanguages": { "meta.embedded.block.sql": "sql" }
}
```

`${…}` holes inside an `@sql` body are scoped back to Noeta, so they highlight as code (not SQL) — the same split the compiler makes between the SQL statics and the checked Noeta hole expressions.

## Examples

[`examples/para-db-demo/`](examples/para-db-demo) — one demo per surface: `demo.noe` (connect/execute/query), `query_demo.noe` (the builder), `schema_demo.noe` (the schema DSL) + `schema_migrate_demo.noe` (`.schema` and `.sql` migrations side by side, under `schema_migrations/`), `repo_demo.noe` (repository + unit-of-work), `sql_demo.noe` (the `@sql` tier), `migrate_demo.noe` + `seed_demo.noe` (the engine), `reactive_demo.noe` (the manual-signal pattern, any driver), `live_repo_sqlite_demo.noe`, `live_repo_demo.noe` + `watch_demo.noe` (PostgreSQL, external writes), `postgres_demo.noe`.

## Requirements

Consumers compile this package's native driver crate locally: `cargo` and a Rust toolchain (1.95+) must be on `PATH`. The Noeta toolchain composes and builds it automatically on first use. SQLite is bundled (compiled from source — no system libsqlite3 needed); the Postgres driver rides the opt-in `ring-postgres` feature — the composed toolchain auto-enables it (no flags needed), and a `--native` (AOT) build requests it in the manifest with `[native] rings = ["ring-postgres"]`.

## Development

- `cargo test` in `crates/noeta-para-db` runs the driver/engine tests.
- `noeta check` / `noeta test` the programs under `examples/` (each demo is its own entry; the Postgres demos need a reachable server).

See [AGENTS.md](AGENTS.md) for the repo layout and the toolchain environment the examples need.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
