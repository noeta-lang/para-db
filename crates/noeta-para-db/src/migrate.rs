//! The **database migration engine** (aether DB6) — the single implementation behind both the
//! `noeta migrate` CLI verb and the programmatic `Connection.migrate(dir)` surface. It drives any
//! [`SqlDriver`], so SQLite and Postgres migrate through the same code with no per-backend branch
//! above the driver seam.
//!
//! # Design
//!
//! **Migrations are files** in a project directory (default `migrations/`), ordered by a **sortable
//! filename prefix**. The engine sorts lexicographically over the whole filename, so any
//! zero-padded/monotonic scheme works; `migrate new` scaffolds a UTC-timestamp prefix
//! (`YYYYMMDDHHMMSS_name.sql`) because timestamps never collide across the many concurrent branches
//! this project is developed on, while still sorting chronologically.
//!
//! **Two body languages, one ordering.** A migration's extension picks how its body becomes DDL
//! ([`MigrationKind`]):
//!   * `.sql` — run **verbatim** in the target dialect's native SQL (via [`SqlDriver::execute_batch`],
//!     no `?`→`$N` rewrite). The escape hatch: anything dialect-specific is written here.
//!   * `.schema` — the **portable schema DSL** ([`crate::schema`]), lowered to the connected driver's
//!     DDL at the driver seam ([`SqlDriver::lower_schema`]) just before it is applied. One file
//!     migrates both SQLite and Postgres.
//!
//! Both kinds live in the **same** directory and interleave in one filename order, so a project
//! writes portable migrations wherever the DSL's vocabulary reaches and drops to raw SQL for the
//! steps it does not (`migrate new` still scaffolds `.sql` unless asked for `--schema`).
//!
//! # What a migration's checksum is taken over
//!
//! **Never the lowered DDL.** The DDL is a function of para/db's own code generator and of the
//! connected backend, so hashing it would give one migration two identities (SQLite's
//! `INTEGER PRIMARY KEY AUTOINCREMENT` against Postgres's `BIGSERIAL PRIMARY KEY`) and would turn any
//! later improvement to the lowering into "history was edited" for every project that already ran it.
//!
//! What is hashed is the migration's **meaning**, spelled the one canonical way:
//!   * `.sql` — the **file source**. Raw SQL *is* the DDL; there is no IR to canonicalize, and the
//!     engine deliberately does not parse SQL, so the bytes the author wrote are the identity.
//!   * `.schema` — the **canonical re-rendering of the parsed IR**
//!     ([`crate::schema::canonicalize`]): source → [`crate::schema::parse`] → `Vec<Statement>` →
//!     [`crate::schema::render`] → sha256. Whitespace, indentation, line breaks and comments are gone
//!     by the time the hash is taken, so reformatting a `.schema` file — a formatter hook, a fixed
//!     typo in a comment — does **not** read as tampered history, while every field the IR records
//!     does change it. No [`crate::schema::Dialect`] appears anywhere on that path, so the checksum is
//!     the same on every backend, and it is taken *before* lowering, so [`crate::schema::lower`] can
//!     be improved freely.
//!
//! A `.schema` body that does not parse has no IR, so it falls back to its source — it can never be
//! applied (the parse failure stops the run before its transaction opens), so it can never be
//! recorded under that checksum either.
//!
//! **A tracking table** [`TRACKING_TABLE`] records each applied migration's filename, a **sha256
//! checksum** of its contents, and the time it was applied. Two integrity gates run before anything is
//! applied: a checksum mismatch on an already-applied file is a hard error (someone edited history),
//! and an applied row whose file has vanished is a hard error (a migration was deleted).
//!
//! **Transactionality.** Each migration applies inside its own transaction (`BEGIN` … body … record …
//! `COMMIT`); the first failure rolls that migration back and stops, naming the file. Postgres has
//! fully transactional DDL and SQLite is transactional for the ordinary DDL migrations use, so a
//! half-applied migration never lands.
//!
//! **Forward-only.** There are no down/rollback files: a down migration is routinely wrong against
//! production data, and `--reset` (drop the schema, re-apply from zero) covers the development loop.
//!
//! **Seeds.** Plain `.sql` files under a project `seeds/` directory, discovered and ordered by the
//! very same [`load_dir`] loader migrations use, run **after** migrations by [`seed`]. Seeds are
//! re-runnable development data, **never** recorded in the tracking table: each runs in its own
//! transaction every time it is invoked, so idempotency is the seed author's concern (the
//! `INSERT OR IGNORE` / `ON CONFLICT DO NOTHING` idiom). [`seed_only`] refuses to run when migrations
//! are pending — seeding a stale schema is a footgun.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::driver::{SqlDriver, SqlValue};

/// The migration tracking table — one row per applied migration. Portable DDL: `CURRENT_TIMESTAMP`
/// and this column set work identically on SQLite and Postgres, so the runner needs no per-dialect
/// tracking schema.
pub const TRACKING_TABLE: &str = "_noeta_migrations";

/// The extension of a raw-SQL migration — the body is run verbatim.
pub const SQL_EXTENSION: &str = "sql";

/// The extension of a portable schema-DSL migration — the body is lowered per driver.
pub const SCHEMA_EXTENSION: &str = "schema";

/// How a migration's body becomes DDL. Decided by the file's **extension**, so the body language is
/// visible in the directory listing and needs no in-file marker (which a checksum would then have to
/// treat as history).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationKind {
    /// A `.sql` body: native SQL for the connected backend, run verbatim.
    Sql,
    /// A `.schema` body: the portable schema DSL, lowered by the driver at apply time.
    Schema,
}

impl MigrationKind {
    /// The kind a filename declares — [`MigrationKind::Schema`] for `.schema`, otherwise raw SQL.
    fn of(name: &str) -> MigrationKind {
        let is_schema = std::path::Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(SCHEMA_EXTENSION));
        if is_schema {
            MigrationKind::Schema
        } else {
            MigrationKind::Sql
        }
    }
}

/// A discovered migration file: its filename (the ordering key **and** the body-language selector),
/// its verbatim source body, and the sha256 hex checksum of what that body *means* (the
/// history-integrity fingerprint).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The file name including extension, e.g. `20260719_143000_init.sql`. The lexicographic order
    /// key.
    pub name: String,
    /// The verbatim file contents: native SQL for a [`MigrationKind::Sql`] migration, schema-DSL
    /// source for a [`MigrationKind::Schema`] one.
    pub body: String,
    /// Lowercase sha256 hex of the migration's **identity text** (see [`identity_text`]): the body
    /// itself for raw SQL, the canonical rendering of the parsed IR for the schema DSL. Never the
    /// lowered DDL, so it does not depend on the backend it was applied against nor on the version of
    /// para/db that applied it.
    pub checksum: String,
    /// How [`Migration::lowered`] turns `body` into DDL.
    pub kind: MigrationKind,
}

impl Migration {
    /// Build a migration from a filename and its contents, computing the checksum and reading the
    /// body language off the extension.
    pub fn new(name: impl Into<String>, body: impl Into<String>) -> Migration {
        let name = name.into();
        let body = body.into();
        let kind = MigrationKind::of(&name);
        let checksum = sha256_hex(identity_text(kind, &body).as_bytes());
        Migration {
            name,
            body,
            checksum,
            kind,
        }
    }

    /// The DDL to execute against `driver`: the body itself for a raw-SQL migration, or the body
    /// parsed as schema DSL and lowered through [`SqlDriver::lower_schema`] for a `.schema` one. Run
    /// **before** the migration's transaction opens, so a DSL syntax error never leaves a dangling
    /// `BEGIN`.
    fn lowered(&self, driver: &dyn SqlDriver) -> Result<String, MigrateError> {
        match self.kind {
            MigrationKind::Sql => Ok(self.body.clone()),
            MigrationKind::Schema => {
                let statements =
                    crate::schema::parse(&self.body).map_err(|e| MigrateError::Schema {
                        filename: self.name.clone(),
                        message: e.to_string(),
                    })?;
                driver
                    .lower_schema(&statements)
                    .map_err(|message| MigrateError::Schema {
                        filename: self.name.clone(),
                        message,
                    })
            }
        }
    }
}

/// One row of the tracking table: a migration that has already been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRecord {
    pub filename: String,
    pub checksum: String,
    pub applied_at: String,
}

/// One migration's place in the plan: applied or pending, with the recorded time when applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    pub name: String,
    pub checksum: String,
    pub applied: bool,
    pub applied_at: Option<String>,
}

/// The result of planning: every migration's status (in order) plus the indices of the pending ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub statuses: Vec<StatusRow>,
    /// Indices into the sorted migration list that are not yet applied, in apply order.
    pub pending: Vec<usize>,
}

/// A migration-engine failure, each naming the offending file where relevant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrateError {
    /// An already-applied migration file's contents no longer match the recorded checksum — its
    /// history was edited. Naming the file (edited-history detection).
    ChecksumDrift {
        filename: String,
        recorded: String,
        found: String,
    },
    /// The tracking table records a migration whose file is gone — it was deleted after being applied.
    DeletedApplied { filename: String },
    /// A migration failed while applying; the transaction was rolled back.
    Apply { filename: String, message: String },
    /// A `.schema` migration's body is not valid portable schema DSL (or the connected driver has no
    /// dialect mapping for it). Reported **before** the migration's transaction opens, so nothing was
    /// touched.
    Schema { filename: String, message: String },
    /// A seed file failed while running; its transaction was rolled back. Distinct from
    /// [`MigrateError::Apply`] so output names it as a seed, not a migration (seeds are untracked
    /// dev data — the prior seeds stay committed, the same stop-on-first-failure shape).
    Seed { filename: String, message: String },
    /// `migrate seed` (seeds-only) found pending migrations: the schema is stale, so seeding is
    /// refused (seeding a stale schema is a footgun). Carries how many migrations are pending.
    PendingMigrations { pending: usize },
    /// A database error outside a single migration (reading the tracking table, `BEGIN`/`COMMIT`).
    Db(String),
    /// A filesystem error discovering or reading migration files.
    Io(String),
    /// A `migrate new <name>` whose name has no usable characters.
    InvalidName(String),
}

impl std::fmt::Display for MigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrateError::ChecksumDrift {
                filename,
                recorded,
                found,
            } => write!(
                f,
                "migration `{filename}` was edited after it was applied: recorded checksum \
                 {recorded:.12}… but the file now hashes to {found:.12}…. A migration's history is \
                 immutable — revert the edit, or make the change in a new migration (`migrate new`).",
            ),
            MigrateError::DeletedApplied { filename } => write!(
                f,
                "migration `{filename}` is recorded as applied but its file is gone. Restore it, or \
                 (in development) run `migrate --reset` to rebuild the schema from the current files.",
            ),
            MigrateError::Apply { filename, message } => {
                write!(
                    f,
                    "migration `{filename}` failed and was rolled back: {message}"
                )
            }
            MigrateError::Schema { filename, message } => write!(
                f,
                "migration `{filename}` is not valid portable schema DSL: {message}"
            ),
            MigrateError::Seed { filename, message } => {
                write!(f, "seed `{filename}` failed and was rolled back: {message}")
            }
            MigrateError::PendingMigrations { pending } => write!(
                f,
                "cannot seed: {pending} migration(s) are still pending, so the schema is out of \
                 date. Run `noeta migrate` first (or `noeta migrate --seed` to migrate then seed) — \
                 seeding a stale schema is a footgun.",
            ),
            MigrateError::Db(message) => write!(f, "database error: {message}"),
            MigrateError::Io(message) => write!(f, "{message}"),
            MigrateError::InvalidName(name) => {
                write!(
                    f,
                    "`{name}` is not a usable migration name (needs letters or digits)"
                )
            }
        }
    }
}

impl std::error::Error for MigrateError {}

/// The text a migration's checksum is taken over — its **identity**, not its formatting.
///
/// For raw SQL that is the body verbatim: SQL *is* the DDL, so the bytes the author wrote are the
/// only honest fingerprint. For the schema DSL it is the canonical rendering of the parsed IR
/// ([`crate::schema::canonicalize`]), which is invariant under reformatting, independent of the
/// connected dialect, and taken before any lowering — see this module's header for why each of those
/// matters.
///
/// A `.schema` body that does not parse has no IR to canonicalize, so it falls back to its source.
/// That migration cannot be applied at all ([`Migration::lowered`] fails before its transaction
/// opens), so the fallback checksum is never recorded; it exists only so discovery
/// ([`load_dir`]) and [`plan`] stay total in the face of a malformed file.
fn identity_text(kind: MigrationKind, body: &str) -> String {
    match kind {
        MigrationKind::Sql => body.to_string(),
        MigrationKind::Schema => {
            crate::schema::canonicalize(body).unwrap_or_else(|_| body.to_string())
        }
    }
}

/// Lowercase sha256 hex of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Discover every `*.sql` and `*.schema` file directly under `dir`, sorted by filename, each with its
/// checksum and its body language ([`MigrationKind`]). The shared loader for **both** migrations and
/// seeds — [`seed`] reuses it for the `seeds/` dir (a seed simply ignores the computed checksum, since
/// seeds are not tracked history).
///
/// The two extensions share **one** ordering: the sort is over the whole filename, so a raw-SQL and a
/// DSL migration interleave by their timestamp prefixes exactly as two `.sql` files would, and the
/// tracking table needs no notion of body language (the filename it already records carries it).
///
/// A missing directory is an error (the caller asked to migrate but there is nowhere to read from);
/// an *empty* existing directory yields an empty list (migrate is then a clean no-op). Sub-directories
/// and files with any other extension are ignored — leaving room for a future per-dialect
/// `dir/postgres/` overlay without disturbing what exists.
pub fn load_dir(dir: &Path) -> Result<Vec<Migration>, MigrateError> {
    if !dir.exists() {
        return Err(MigrateError::Io(format!(
            "migrations directory `{}` does not exist",
            dir.display()
        )));
    }
    let entries = std::fs::read_dir(dir).map_err(|e| {
        MigrateError::Io(format!(
            "cannot read migrations directory `{}`: {e}",
            dir.display()
        ))
    })?;

    let mut migrations = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| MigrateError::Io(format!("reading `{}`: {e}", dir.display())))?;
        let path = entry.path();
        let is_migration = path.is_file()
            && path.extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case(SQL_EXTENSION)
                    || ext.eq_ignore_ascii_case(SCHEMA_EXTENSION)
            });
        if !is_migration {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let body = std::fs::read_to_string(&path).map_err(|e| {
            MigrateError::Io(format!("cannot read migration `{}`: {e}", path.display()))
        })?;
        migrations.push(Migration::new(name, body));
    }
    migrations.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(migrations)
}

/// Plan the migration run against the current tracking state — a **pure** function over the sorted
/// migrations and the applied records, so ordering / drift / deleted-file / pending computation is
/// unit-testable with no database. Returns the two integrity errors ([`MigrateError::ChecksumDrift`],
/// [`MigrateError::DeletedApplied`]) before any migration would be touched.
pub fn plan(migrations: &[Migration], applied: &[AppliedRecord]) -> Result<Plan, MigrateError> {
    // Integrity gates: every applied record must still have a matching file with a matching checksum.
    for record in applied {
        match migrations.iter().find(|m| m.name == record.filename) {
            None => {
                return Err(MigrateError::DeletedApplied {
                    filename: record.filename.clone(),
                });
            }
            Some(m) if m.checksum != record.checksum => {
                return Err(MigrateError::ChecksumDrift {
                    filename: record.filename.clone(),
                    recorded: record.checksum.clone(),
                    found: m.checksum.clone(),
                });
            }
            Some(_) => {}
        }
    }

    let mut statuses = Vec::with_capacity(migrations.len());
    let mut pending = Vec::new();
    for (index, m) in migrations.iter().enumerate() {
        match applied.iter().find(|r| r.filename == m.name) {
            Some(record) => statuses.push(StatusRow {
                name: m.name.clone(),
                checksum: m.checksum.clone(),
                applied: true,
                applied_at: Some(record.applied_at.clone()),
            }),
            None => {
                pending.push(index);
                statuses.push(StatusRow {
                    name: m.name.clone(),
                    checksum: m.checksum.clone(),
                    applied: false,
                    applied_at: None,
                });
            }
        }
    }
    Ok(Plan { statuses, pending })
}

/// Create the tracking table if it does not exist. Portable across SQLite and Postgres.
fn ensure_tracking_table(driver: &mut dyn SqlDriver) -> Result<(), MigrateError> {
    driver
        .execute_batch(&format!(
            "CREATE TABLE IF NOT EXISTS {TRACKING_TABLE} (\
                 filename TEXT PRIMARY KEY, \
                 checksum TEXT NOT NULL, \
                 applied_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP\
             )"
        ))
        .map_err(MigrateError::Db)
}

/// Read every applied-migration record from the tracking table, ordered by filename.
fn read_applied(driver: &mut dyn SqlDriver) -> Result<Vec<AppliedRecord>, MigrateError> {
    // `applied_at` is cast in SQL rather than read as a timestamp: the row surface is textual
    // (`SqlValue` has no temporal variant) and the Postgres driver receives a `timestamp` in
    // binary, so reading the column raw fails outright — which broke every migrate *after* the
    // first, the moment the tracking table had a row to read back. `CAST(… AS TEXT)` is portable
    // across both drivers and makes the column text on the wire, which is all this needs.
    let rows = driver
        .query(
            &format!(
                "SELECT filename, checksum, CAST(applied_at AS TEXT) AS applied_at \
                 FROM {TRACKING_TABLE} ORDER BY filename"
            ),
            &[],
        )
        .map_err(MigrateError::Db)?;
    Ok(rows
        .into_iter()
        .map(|row| AppliedRecord {
            filename: column_text(&row, "filename"),
            checksum: column_text(&row, "checksum"),
            applied_at: column_text(&row, "applied_at"),
        })
        .collect())
}

/// Read a text column from a row, tolerating whatever scalar the driver returns (`applied_at` is
/// cast to text by the query above on both drivers; the other columns are text already).
fn column_text(row: &crate::driver::Row, name: &str) -> String {
    row.iter()
        .find(|(col, _)| col == name)
        .map(|(_, value)| match value {
            SqlValue::Text(s) => s.clone(),
            SqlValue::Int(n) => n.to_string(),
            SqlValue::Float(f) => f.to_string(),
            SqlValue::Bool(b) => b.to_string(),
            SqlValue::Null => String::new(),
        })
        .unwrap_or_default()
}

/// Apply one migration inside its own transaction: lower the body to this driver's DDL, then `BEGIN`,
/// run it, record the row, `COMMIT`. Any failure rolls back (best-effort) and reports the file.
/// Lowering happens *outside* the transaction so a malformed `.schema` body never opens one.
fn apply_one(driver: &mut dyn SqlDriver, migration: &Migration) -> Result<(), MigrateError> {
    let ddl = migration.lowered(driver)?;
    driver.execute_batch("BEGIN").map_err(MigrateError::Db)?;

    let body = (|| -> Result<(), String> {
        driver.execute_batch(&ddl)?;
        driver.execute(
            &format!("INSERT INTO {TRACKING_TABLE} (filename, checksum) VALUES (?, ?)"),
            &[
                SqlValue::Text(migration.name.clone()),
                SqlValue::Text(migration.checksum.clone()),
            ],
        )?;
        Ok(())
    })();

    match body {
        Ok(()) => driver.execute_batch("COMMIT").map_err(MigrateError::Db),
        Err(message) => {
            // Best-effort rollback: report the original failure regardless of whether the rollback
            // itself errored (a driver that lost the connection can't roll back either).
            let _ = driver.execute_batch("ROLLBACK");
            Err(MigrateError::Apply {
                filename: migration.name.clone(),
                message,
            })
        }
    }
}

/// Apply every pending migration in order, each in its own transaction. Returns the names applied.
/// Runs the integrity gates first (via [`plan`]); stops at the first failure with the prior
/// migrations left committed.
pub fn apply(
    driver: &mut dyn SqlDriver,
    migrations: &[Migration],
) -> Result<Vec<String>, MigrateError> {
    ensure_tracking_table(driver)?;
    let applied = read_applied(driver)?;
    let plan = plan(migrations, &applied)?;

    let mut done = Vec::with_capacity(plan.pending.len());
    for &index in &plan.pending {
        let migration = &migrations[index];
        apply_one(driver, migration)?;
        done.push(migration.name.clone());
    }
    Ok(done)
}

/// The full status of every migration (applied/pending + recorded time), running the integrity gates.
pub fn status(
    driver: &mut dyn SqlDriver,
    migrations: &[Migration],
) -> Result<Vec<StatusRow>, MigrateError> {
    ensure_tracking_table(driver)?;
    let applied = read_applied(driver)?;
    Ok(plan(migrations, &applied)?.statuses)
}

/// The names of the pending migrations, without applying anything (backs `--dry-run`).
pub fn pending(
    driver: &mut dyn SqlDriver,
    migrations: &[Migration],
) -> Result<Vec<String>, MigrateError> {
    ensure_tracking_table(driver)?;
    let applied = read_applied(driver)?;
    let plan = plan(migrations, &applied)?;
    Ok(plan
        .pending
        .into_iter()
        .map(|i| migrations[i].name.clone())
        .collect())
}

/// **Destructive.** Drop the whole schema (via [`SqlDriver::reset`]) and re-apply every migration from
/// zero. Returns the names re-applied. The caller is responsible for confirming intent.
pub fn reset(
    driver: &mut dyn SqlDriver,
    migrations: &[Migration],
) -> Result<Vec<String>, MigrateError> {
    driver.reset().map_err(MigrateError::Db)?;
    apply(driver, migrations)
}

/// Run every **seed** file in `seeds` in filename order, each in its own transaction, returning the
/// names run. Seeds are re-runnable development data, **never tracked** in [`TRACKING_TABLE`]: this
/// applies every file every time it is called, so idempotency is the seed author's concern (use
/// `INSERT OR IGNORE` on SQLite / `ON CONFLICT DO NOTHING` on Postgres to make a re-run a no-op). A
/// seed uses the same discovery/ordering as a migration — [`load_dir`] loads both — but no checksum
/// is recorded (a seed is not history). The first failure rolls that seed back and stops, naming the
/// file, with the prior seeds left committed — the same stop-on-first-failure shape as [`apply`].
pub fn seed(driver: &mut dyn SqlDriver, seeds: &[Migration]) -> Result<Vec<String>, MigrateError> {
    let mut done = Vec::with_capacity(seeds.len());
    for file in seeds {
        seed_one(driver, file)?;
        done.push(file.name.clone());
    }
    Ok(done)
}

/// Run one seed file inside its own transaction: `BEGIN`, the verbatim body, `COMMIT` — no tracking
/// insert (seeds are not history). Any failure rolls back (best-effort) and reports the file.
fn seed_one(driver: &mut dyn SqlDriver, file: &Migration) -> Result<(), MigrateError> {
    // Seeds share the loader, so they share the body languages too: a `.sql` seed runs verbatim and a
    // `.schema` one lowers through the driver first (rarely what a seed wants — seeds are data — but
    // there is no reason for the two directories to understand different files).
    let body = file.lowered(driver)?;
    driver.execute_batch("BEGIN").map_err(MigrateError::Db)?;
    match driver.execute_batch(&body) {
        Ok(()) => driver.execute_batch("COMMIT").map_err(MigrateError::Db),
        Err(message) => {
            let _ = driver.execute_batch("ROLLBACK");
            Err(MigrateError::Seed {
                filename: file.name.clone(),
                message,
            })
        }
    }
}

/// Run seeds **only**, requiring the schema to be up to date first: if any migration under
/// `migrations` is still pending, returns [`MigrateError::PendingMigrations`] and runs no seed
/// (seeding a stale schema is a footgun). Backs `noeta migrate seed`. Runs the migration integrity
/// gates (via [`plan`]) as part of the pending check, so a drifted/deleted migration still errors.
pub fn seed_only(
    driver: &mut dyn SqlDriver,
    migrations: &[Migration],
    seeds: &[Migration],
) -> Result<Vec<String>, MigrateError> {
    ensure_tracking_table(driver)?;
    let applied = read_applied(driver)?;
    let plan = plan(migrations, &applied)?;
    if !plan.pending.is_empty() {
        return Err(MigrateError::PendingMigrations {
            pending: plan.pending.len(),
        });
    }
    seed(driver, seeds)
}

/// Build the filename for a new migration: `{prefix}_{slug}.{extension}`, where `slug` is `name`
/// lowercased with every run of non-alphanumeric characters collapsed to a single `_`, and
/// `extension` is [`SQL_EXTENSION`] or [`SCHEMA_EXTENSION`] (the body language). Pure (the caller
/// supplies the timestamp `prefix`), so it is testable without a clock.
pub fn scaffold_filename(
    prefix: &str,
    name: &str,
    extension: &str,
) -> Result<String, MigrateError> {
    let mut slug = String::with_capacity(name.len());
    let mut prev_underscore = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_underscore = false;
        } else if !prev_underscore {
            slug.push('_');
            prev_underscore = true;
        }
    }
    let slug = slug.trim_matches('_');
    if slug.is_empty() {
        return Err(MigrateError::InvalidName(name.to_string()));
    }
    Ok(format!("{prefix}_{slug}.{extension}"))
}

/// The starter body written into a freshly scaffolded raw-SQL migration file.
pub const SCAFFOLD_TEMPLATE: &str = "-- Migration: write forward-only SQL below. This file's contents are checksummed once applied,\n\
     -- so edit it only before it runs; make later changes in a new migration.\n\
     --\n\
     -- This body runs VERBATIM in the connected database's own SQL dialect. For a migration that\n\
     -- works on every backend, scaffold the portable schema DSL instead: `migrate new <name> --schema`.\n";

/// The starter body written into a freshly scaffolded **portable schema-DSL** migration file — it
/// shows the fluent shape and names the escape hatch, since the DSL deliberately covers only what
/// lowers identically onto both backends.
pub const SCHEMA_SCAFFOLD_TEMPLATE: &str = "// Migration (portable schema DSL): lowered to the connected driver's own DDL — SQLite gets\n\
     // `INTEGER PRIMARY KEY AUTOINCREMENT`, Postgres `BIGSERIAL PRIMARY KEY`, and so on. Once applied,\n\
     // what is checksummed is the SCHEMA THIS FILE DESCRIBES, not its text: reformatting it or editing\n\
     // a comment is safe, changing what it does is edited history. Make later changes in a new one.\n\
     //\n\
     // create_table(\"todos\")\n\
     //     .id()\n\
     //     .text(\"title\").not_null()\n\
     //     .bool(\"done\").default(false)\n\
     //     .timestamps()\n\
     //\n\
     // create_index(\"todos\").column(\"done\")\n\
     //\n\
     // Statements: create_table, alter_table, drop_table, create_index, drop_index.\n\
     // Anything dialect-specific (views, triggers, json/uuid/decimal columns, partial indexes)\n\
     // belongs in a raw `.sql` migration beside this one — the two interleave in filename order.\n";

/// The starter body written into a freshly scaffolded seed file — re-runnable dev data, so it
/// documents the idempotent-insert idiom inline.
pub const SEED_SCAFFOLD_TEMPLATE: &str = "-- Seed: re-runnable development data. This file runs on every `noeta migrate seed` / `--seed`,\n\
     -- each in its own transaction, and is NOT tracked. Make inserts idempotent so a re-run is a\n\
     -- no-op: `INSERT OR IGNORE` on SQLite, `... ON CONFLICT DO NOTHING` on Postgres.\n";

#[cfg(test)]
mod tests {
    use super::*;

    fn migration(name: &str, sql: &str) -> Migration {
        Migration::new(name, sql)
    }

    fn applied(m: &Migration, at: &str) -> AppliedRecord {
        AppliedRecord {
            filename: m.name.clone(),
            checksum: m.checksum.clone(),
            applied_at: at.to_string(),
        }
    }

    #[test]
    fn checksum_is_stable_lowercase_hex_of_contents() {
        // Known SHA-256 of the empty input and of "abc".
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // Migration::new hashes its body.
        assert_eq!(migration("0001_x.sql", "abc").checksum, sha256_hex(b"abc"));
    }

    #[test]
    fn plan_orders_and_computes_pending() {
        // Two applied, one new → only the new one is pending, statuses are in filename order.
        let a = migration("0001_a.sql", "CREATE TABLE a(id INT);");
        let b = migration("0002_b.sql", "CREATE TABLE b(id INT);");
        let c = migration("0003_c.sql", "CREATE TABLE c(id INT);");
        let migrations = vec![a.clone(), b.clone(), c.clone()];
        let records = vec![applied(&a, "t1"), applied(&b, "t2")];

        let plan = plan(&migrations, &records).unwrap();
        assert_eq!(plan.pending, vec![2]);
        assert_eq!(
            plan.statuses
                .iter()
                .map(|s| (s.name.as_str(), s.applied))
                .collect::<Vec<_>>(),
            vec![
                ("0001_a.sql", true),
                ("0002_b.sql", true),
                ("0003_c.sql", false)
            ]
        );
        assert_eq!(plan.statuses[0].applied_at.as_deref(), Some("t1"));
        assert_eq!(plan.statuses[2].applied_at, None);
    }

    #[test]
    fn plan_detects_checksum_drift_on_an_applied_file() {
        let a = migration("0001_a.sql", "CREATE TABLE a(id INT);");
        // The tracking table recorded a *different* checksum → the file was edited after applying.
        let record = AppliedRecord {
            filename: "0001_a.sql".into(),
            checksum: "deadbeef".into(),
            applied_at: "t1".into(),
        };
        let err = plan(&[a], &[record]).unwrap_err();
        match err {
            MigrateError::ChecksumDrift { filename, .. } => assert_eq!(filename, "0001_a.sql"),
            other => panic!("expected drift, got {other:?}"),
        }
    }

    #[test]
    fn plan_detects_a_deleted_applied_migration() {
        // A record with no corresponding file.
        let record = AppliedRecord {
            filename: "0001_gone.sql".into(),
            checksum: "abc".into(),
            applied_at: "t1".into(),
        };
        let err = plan(&[], &[record]).unwrap_err();
        assert_eq!(
            err,
            MigrateError::DeletedApplied {
                filename: "0001_gone.sql".into()
            }
        );
    }

    #[test]
    fn plan_with_nothing_applied_makes_everything_pending() {
        let a = migration("0001_a.sql", "SELECT 1;");
        let b = migration("0002_b.sql", "SELECT 2;");
        let plan = plan(&[a, b], &[]).unwrap();
        assert_eq!(plan.pending, vec![0, 1]);
    }

    #[test]
    fn seed_scaffold_template_documents_the_idempotent_idiom() {
        // The seed starter body names both drivers' idempotent-insert idioms, so a re-run is a no-op.
        assert!(SEED_SCAFFOLD_TEMPLATE.contains("INSERT OR IGNORE"));
        assert!(SEED_SCAFFOLD_TEMPLATE.contains("ON CONFLICT DO NOTHING"));
        assert!(SEED_SCAFFOLD_TEMPLATE.contains("NOT tracked"));
    }

    #[test]
    fn scaffold_filename_slugifies_and_prefixes() {
        assert_eq!(
            scaffold_filename("20260719143000", "Add Users Table", SQL_EXTENSION).unwrap(),
            "20260719143000_add_users_table.sql"
        );
        assert_eq!(
            scaffold_filename("0004", "create--posts!!", SQL_EXTENSION).unwrap(),
            "0004_create_posts.sql"
        );
        // The body language is the extension, and nothing else about the name changes.
        assert_eq!(
            scaffold_filename("20260719143000", "Add Users Table", SCHEMA_EXTENSION).unwrap(),
            "20260719143000_add_users_table.schema"
        );
    }

    #[test]
    fn scaffold_filename_rejects_an_empty_slug() {
        assert!(matches!(
            scaffold_filename("0001", "!!!", SQL_EXTENSION).unwrap_err(),
            MigrateError::InvalidName(_)
        ));
    }

    #[test]
    fn the_extension_selects_the_body_language() {
        assert_eq!(
            migration("0001_a.sql", "SELECT 1;").kind,
            MigrationKind::Sql
        );
        assert_eq!(
            migration("0001_a.schema", "create_table(\"t\").int(\"x\")").kind,
            MigrationKind::Schema
        );
        // Case-insensitive, like the loader's own extension match.
        assert_eq!(
            migration("0001_a.SCHEMA", "create_table(\"t\").int(\"x\")").kind,
            MigrationKind::Schema
        );
    }

    #[test]
    fn a_raw_sql_migration_is_still_hashed_over_its_source() {
        // Raw SQL *is* the DDL: there is no IR to canonicalize, so the bytes the author wrote are
        // the identity — unchanged by this design, formatting and all.
        let src = "CREATE   TABLE a (id INT);  -- a comment\n";
        assert_eq!(
            migration("0001_a.sql", src).checksum,
            sha256_hex(src.as_bytes())
        );
    }

    #[test]
    fn a_dsl_migrations_checksum_is_over_the_canonical_ir_not_the_source_or_the_ddl() {
        // The identity of a `.schema` migration is the schema it describes, rendered canonically —
        // so it is the same whether it was applied against SQLite or Postgres, a change to the
        // lowering can never read as edited history, and the file's text is not the fingerprint.
        let src = "create_table(\"t\").id()";
        let m = migration("0001_t.schema", src);
        assert_eq!(
            m.checksum,
            sha256_hex(crate::schema::canonicalize(src).unwrap().as_bytes())
        );
        assert_ne!(m.checksum, sha256_hex(src.as_bytes()));
        // The body is still kept verbatim — only the checksum goes through the IR.
        assert_eq!(m.body, src);
    }

    #[test]
    fn reformatting_a_dsl_migration_does_not_change_its_checksum() {
        // The whole point: a formatter hook, a re-indent, a rewritten comment, an added blank line,
        // and either comment syntax all leave the migration's identity alone.
        let terse = "create_table(\"t\").id().text(\"title\").not_null()";
        let sprawling = "\n\
             // The table this migration creates.\n\
             create_table(\"t\")\n\
             \t.id()\n\
             \n\
             \t.text(\"title\")   -- the headline\n\
             \t\t.not_null()\n\
             \n";
        assert_eq!(
            migration("0001_t.schema", terse).checksum,
            migration("0001_t.schema", sprawling).checksum
        );
        // …and the two files really are different bytes, so the equality above says something.
        assert_ne!(terse, sprawling);
    }

    #[test]
    fn changing_what_a_dsl_migration_does_always_changes_its_checksum() {
        // Every kind of meaning change — a renamed column, a changed type, a changed default, an
        // added or dropped constraint, a reordered statement, an extra statement — is a distinct
        // identity. (Pairwise distinct, so none of them collides with any other either.)
        let variants = [
            "create_table(\"t\").id().text(\"title\").not_null().bool(\"done\").default(false)",
            // renamed column
            "create_table(\"t\").id().text(\"name\").not_null().bool(\"done\").default(false)",
            // changed type
            "create_table(\"t\").id().int(\"title\").not_null().bool(\"done\").default(false)",
            // changed default
            "create_table(\"t\").id().text(\"title\").not_null().bool(\"done\").default(true)",
            // dropped constraint
            "create_table(\"t\").id().text(\"title\").bool(\"done\").default(false)",
            // added constraint
            "create_table(\"t\").id().text(\"title\").not_null().unique().bool(\"done\").default(false)",
            // reordered columns
            "create_table(\"t\").id().bool(\"done\").default(false).text(\"title\").not_null()",
            // an extra statement
            "create_table(\"t\").id().text(\"title\").not_null().bool(\"done\").default(false)\n\
             create_index(\"t\").column(\"done\")",
            // reordered statements
            "create_index(\"t\").column(\"done\")\n\
             create_table(\"t\").id().text(\"title\").not_null().bool(\"done\").default(false)",
        ];
        let checksums: Vec<String> = variants
            .iter()
            .map(|src| migration("0001_t.schema", src).checksum)
            .collect();
        for (i, a) in checksums.iter().enumerate() {
            for (j, b) in checksums.iter().enumerate().skip(i + 1) {
                assert_ne!(a, b, "variants {i} and {j} share a checksum");
            }
        }
    }

    #[test]
    fn a_dsl_migrations_checksum_is_pinned_to_the_canonical_text() {
        // A regression pin over a representative statement set: the checksum is the sha256 of THIS
        // text, and of nothing that a `lower` change could touch. If a future canonical rendering
        // changes, this fails — which is the point, since it would silently re-identify every
        // already-applied `.schema` migration in every project.
        let src = "
            create_table(\"notes\")
                .id()
                .text(\"title\").not_null()
                .bigint(\"author_id\").references(\"users\", \"id\").on_delete(\"CASCADE\")
                .bool(\"pinned\").default(false)
                .timestamps()
                .unique(\"title\", \"author_id\")
                .if_not_exists()

            create_index(\"notes\").column(\"pinned\").unique().name(\"by_pinned\")
            alter_table(\"notes\").add_text(\"body\").rename_column(\"title\", \"headline\")
            drop_index(\"by_pinned\").if_exists()
            drop_table(\"scratch\")
        ";
        let canonical = crate::schema::canonicalize(src).unwrap();
        assert_eq!(
            canonical,
            "create_table(\"notes\")\
             .id(\"id\").not_null()\
             .text(\"title\").not_null()\
             .bigint(\"author_id\").references(\"users\", \"id\").on_delete(\"cascade\")\
             .bool(\"pinned\").default(false)\
             .timestamp(\"created_at\").not_null().default_now()\
             .timestamp(\"updated_at\").not_null().default_now()\
             .unique(\"title\", \"author_id\")\
             .if_not_exists()\n\
             create_index(\"notes\").name(\"by_pinned\").columns(\"pinned\").unique()\n\
             alter_table(\"notes\").add_text(\"body\").rename_column(\"title\", \"headline\")\n\
             drop_index(\"by_pinned\").if_exists()\n\
             drop_table(\"scratch\")\n"
        );
        assert_eq!(
            migration("0001_notes.schema", src).checksum,
            "5739ddfd8bc0649ee5c74cde071bb20da2388747426aaf2326e119dbae405ce0"
        );
    }

    #[test]
    fn a_malformed_dsl_body_falls_back_to_its_source() {
        // No IR, no canonical form. The fallback keeps discovery total; the file can never be
        // applied, so the checksum is never recorded (see `identity_text`).
        let src = "create_table(\"t\").frobnicate()";
        assert!(crate::schema::parse(src).is_err());
        assert_eq!(
            migration("0001_t.schema", src).checksum,
            sha256_hex(src.as_bytes())
        );
    }

    #[test]
    fn schema_scaffold_template_shows_the_shape_and_names_the_escape_hatch() {
        assert!(SCHEMA_SCAFFOLD_TEMPLATE.contains("create_table(\"todos\")"));
        assert!(SCHEMA_SCAFFOLD_TEMPLATE.contains(".timestamps()"));
        assert!(SCHEMA_SCAFFOLD_TEMPLATE.contains("raw `.sql` migration"));
        // The SQL scaffold points the other way, so either starting point mentions the other.
        assert!(SCAFFOLD_TEMPLATE.contains("--schema"));
    }

    #[test]
    fn load_dir_errors_when_missing_but_is_empty_when_bare() {
        let missing = std::path::Path::new("/does/not/exist/noeta-migrate-xyz");
        assert!(matches!(load_dir(missing), Err(MigrateError::Io(_))));

        let dir = std::env::temp_dir().join(format!("noeta-migrate-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_dir(&dir).unwrap(), Vec::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_reads_sorts_and_ignores_non_sql() {
        let dir = std::env::temp_dir().join(format!("noeta-migrate-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0002_b.sql"), "SELECT 2;").unwrap();
        std::fs::write(dir.join("0001_a.sql"), "SELECT 1;").unwrap();
        std::fs::write(dir.join("README.md"), "not a migration").unwrap();

        let migrations = load_dir(&dir).unwrap();
        assert_eq!(
            migrations
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["0001_a.sql", "0002_b.sql"]
        );
        assert_eq!(migrations[0].body, "SELECT 1;");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_interleaves_both_body_languages_in_one_filename_order() {
        let dir = std::env::temp_dir().join(format!("noeta-migrate-mixed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0003_c.sql"), "SELECT 3;").unwrap();
        std::fs::write(dir.join("0001_a.schema"), "create_table(\"a\").int(\"x\")").unwrap();
        std::fs::write(dir.join("0002_b.sql"), "SELECT 2;").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();

        let migrations = load_dir(&dir).unwrap();
        assert_eq!(
            migrations
                .iter()
                .map(|m| (m.name.as_str(), m.kind))
                .collect::<Vec<_>>(),
            vec![
                ("0001_a.schema", MigrationKind::Schema),
                ("0002_b.sql", MigrationKind::Sql),
                ("0003_c.sql", MigrationKind::Sql),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// End-to-end tests against a real in-memory SQLite driver (behind `ring-sqlite`, the default
/// feature). They exercise the whole engine over a live database: apply, idempotent re-run, status,
/// reset, dry-run, and a mid-migration failure leaving the prior migration committed.
#[cfg(all(test, feature = "ring-sqlite"))]
mod sqlite_e2e {
    use super::*;
    use crate::sqlite::SqliteDriver;

    fn mem() -> SqliteDriver {
        SqliteDriver::open_in_memory().unwrap()
    }

    fn table_exists(driver: &mut dyn SqlDriver, table: &str) -> bool {
        let rows = driver
            .query(
                "SELECT name FROM sqlite_master WHERE type='table' AND name = ?",
                &[SqlValue::Text(table.to_string())],
            )
            .unwrap();
        !rows.is_empty()
    }

    fn migrations() -> Vec<Migration> {
        vec![
            Migration::new(
                "0001_users.sql",
                "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
            ),
            Migration::new(
                "0002_posts.sql",
                "CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT);\n\
                 INSERT INTO posts (id, title) VALUES (1, 'hello');",
            ),
        ]
    }

    #[test]
    fn apply_then_rerun_is_a_noop() {
        let mut driver = mem();
        let migrations = migrations();

        let first = apply(&mut driver, &migrations).unwrap();
        assert_eq!(first, vec!["0001_users.sql", "0002_posts.sql"]);
        assert!(table_exists(&mut driver, "users"));
        assert!(table_exists(&mut driver, "posts"));
        // The multi-statement second migration ran fully (its INSERT landed).
        let rows = driver
            .query("SELECT COUNT(*) AS n FROM posts", &[])
            .unwrap();
        assert_eq!(rows[0][0].1, SqlValue::Int(1));

        // Re-running applies nothing.
        let second = apply(&mut driver, &migrations).unwrap();
        assert!(second.is_empty());
    }

    #[test]
    fn status_reports_applied_and_pending() {
        let mut driver = mem();
        let migrations = migrations();
        apply(&mut driver, &migrations[..1]).unwrap(); // apply only the first

        let rows = status(&mut driver, &migrations).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].applied);
        assert!(rows[0].applied_at.is_some());
        assert!(!rows[1].applied);
    }

    #[test]
    fn dry_run_lists_pending_without_applying() {
        let mut driver = mem();
        let migrations = migrations();

        let names = pending(&mut driver, &migrations).unwrap();
        assert_eq!(names, vec!["0001_users.sql", "0002_posts.sql"]);
        // Nothing was created — dry-run only read.
        assert!(!table_exists(&mut driver, "users"));
    }

    #[test]
    fn reset_drops_everything_and_reapplies() {
        let mut driver = mem();
        let migrations = migrations();
        apply(&mut driver, &migrations).unwrap();
        driver
            .execute("INSERT INTO users (id, name) VALUES (1, 'Ada')", &[])
            .unwrap();

        let reapplied = reset(&mut driver, &migrations).unwrap();
        assert_eq!(reapplied, vec!["0001_users.sql", "0002_posts.sql"]);
        // The user row is gone (schema was dropped), the schema is back.
        assert!(table_exists(&mut driver, "users"));
        let rows = driver
            .query("SELECT COUNT(*) AS n FROM users", &[])
            .unwrap();
        assert_eq!(rows[0][0].1, SqlValue::Int(0));
    }

    #[test]
    fn a_failing_migration_rolls_back_and_leaves_the_prior_one() {
        let mut driver = mem();
        let migrations = vec![
            Migration::new("0001_ok.sql", "CREATE TABLE ok (id INTEGER);"),
            Migration::new(
                "0002_bad.sql",
                "CREATE TABLE bad (id INTEGER); NONSENSE SQL HERE;",
            ),
        ];

        let err = apply(&mut driver, &migrations).unwrap_err();
        match err {
            MigrateError::Apply { filename, .. } => assert_eq!(filename, "0002_bad.sql"),
            other => panic!("expected an apply error, got {other:?}"),
        }
        // The first migration is committed; the failed one left nothing behind (its CREATE rolled back).
        assert!(table_exists(&mut driver, "ok"));
        assert!(!table_exists(&mut driver, "bad"));
        // And it is recorded as applied so a re-run resumes at the failing one.
        let rows = status(&mut driver, &migrations).unwrap();
        assert!(rows[0].applied);
        assert!(!rows[1].applied);
    }

    #[test]
    fn editing_an_applied_migration_is_detected_on_the_next_run() {
        let mut driver = mem();
        let original = vec![Migration::new("0001_a.sql", "CREATE TABLE a (id INTEGER);")];
        apply(&mut driver, &original).unwrap();

        // Same filename, different contents → checksum drift.
        let edited = vec![Migration::new(
            "0001_a.sql",
            "CREATE TABLE a (id INTEGER, extra TEXT);",
        )];
        let err = apply(&mut driver, &edited).unwrap_err();
        assert!(matches!(err, MigrateError::ChecksumDrift { .. }));
    }

    fn count(driver: &mut dyn SqlDriver, table: &str) -> i64 {
        let rows = driver
            .query(&format!("SELECT COUNT(*) AS n FROM {table}"), &[])
            .unwrap();
        match rows[0][0].1 {
            SqlValue::Int(n) => n,
            ref other => panic!("expected an int count, got {other:?}"),
        }
    }

    #[test]
    fn seeds_run_in_filename_order_and_are_never_tracked() {
        let mut driver = mem();
        apply(&mut driver, &migrations()).unwrap();

        let seeds = vec![
            Migration::new(
                "0002_b.sql",
                "INSERT INTO users (id, name) VALUES (2, 'Bob');",
            ),
            Migration::new(
                "0001_a.sql",
                "INSERT INTO users (id, name) VALUES (1, 'Ada');",
            ),
        ];
        // `load_dir` sorts; `seed` runs whatever order it is handed — so sort first, like the callers.
        let mut ordered = seeds.clone();
        ordered.sort_by(|a, b| a.name.cmp(&b.name));
        let ran = seed(&mut driver, &ordered).unwrap();
        assert_eq!(ran, vec!["0001_a.sql", "0002_b.sql"]);
        assert_eq!(count(&mut driver, "users"), 2);

        // Seeds are NOT recorded in the tracking table — they are re-runnable data, not history.
        let tracked = driver
            .query(
                &format!("SELECT filename FROM {TRACKING_TABLE} ORDER BY filename"),
                &[],
            )
            .unwrap();
        let names: Vec<_> = tracked.iter().map(|r| column_text(r, "filename")).collect();
        assert_eq!(names, vec!["0001_users.sql", "0002_posts.sql"]);
    }

    #[test]
    fn a_plain_seed_reruns_every_time_but_an_idempotent_one_is_a_noop() {
        let mut driver = mem();
        apply(&mut driver, &migrations()).unwrap();

        // A plain INSERT re-inserts on every run (idempotency is the author's concern).
        let plain = vec![Migration::new(
            "0001_plain.sql",
            "INSERT INTO posts (id, title) VALUES (2, 'again');",
        )];
        seed(&mut driver, &plain).unwrap();
        seed(&mut driver, &plain).unwrap_err(); // second run trips the PK — proves it re-ran

        // The documented idempotent idiom makes a re-run a no-op.
        let idempotent = vec![Migration::new(
            "0001_idem.sql",
            "INSERT OR IGNORE INTO posts (id, title) VALUES (3, 'once');",
        )];
        seed(&mut driver, &idempotent).unwrap();
        let after_first = count(&mut driver, "posts");
        seed(&mut driver, &idempotent).unwrap();
        assert_eq!(count(&mut driver, "posts"), after_first);
    }

    #[test]
    fn a_failing_seed_stops_and_leaves_the_prior_committed() {
        let mut driver = mem();
        apply(&mut driver, &migrations()).unwrap();

        let seeds = vec![
            Migration::new(
                "0001_ok.sql",
                "INSERT INTO users (id, name) VALUES (1, 'Ada');",
            ),
            Migration::new("0002_bad.sql", "NONSENSE SQL HERE;"),
        ];
        let err = seed(&mut driver, &seeds).unwrap_err();
        match err {
            MigrateError::Seed { filename, .. } => assert_eq!(filename, "0002_bad.sql"),
            other => panic!("expected a seed error, got {other:?}"),
        }
        // The first seed committed before the second failed.
        assert_eq!(count(&mut driver, "users"), 1);
    }

    #[test]
    fn seed_only_refuses_when_a_migration_is_pending() {
        let mut driver = mem();
        // Apply only the first migration, leaving the second pending.
        let migrations = migrations();
        apply(&mut driver, &migrations[..1]).unwrap();

        let seeds = vec![Migration::new(
            "0001_a.sql",
            "INSERT INTO users (id, name) VALUES (1, 'Ada');",
        )];
        let err = seed_only(&mut driver, &migrations, &seeds).unwrap_err();
        assert_eq!(err, MigrateError::PendingMigrations { pending: 1 });
        // No seed ran — `users` is empty (the schema exists from the first migration).
        assert_eq!(count(&mut driver, "users"), 0);
    }

    /// The regression corpus for the DSL slice: a portable `.schema` migration and a raw `.sql` one
    /// in the same list, applied through the same engine in filename order.
    fn mixed_migrations() -> Vec<Migration> {
        vec![
            Migration::new(
                "0001_todos.schema",
                "create_table(\"todos\")\n\
                 .id()\n\
                 .text(\"title\").not_null()\n\
                 .bool(\"done\").default(false)\n\
                 .timestamps()\n\
                 \n\
                 create_index(\"todos\").column(\"done\")\n",
            ),
            // Raw SQL keeps working unchanged, beside the DSL, in one directory.
            Migration::new(
                "0002_first_todo.sql",
                "INSERT INTO todos (title) VALUES ('write a migration');",
            ),
        ]
    }

    #[test]
    fn a_schema_dsl_migration_applies_beside_a_raw_sql_one() {
        let mut driver = mem();
        let migrations = mixed_migrations();

        let applied = apply(&mut driver, &migrations).unwrap();
        assert_eq!(applied, vec!["0001_todos.schema", "0002_first_todo.sql"]);
        assert!(table_exists(&mut driver, "todos"));

        // The lowered SQLite DDL really is what the DSL described: an autoincrement identity, a
        // NOT NULL text column, a defaulted boolean, and the two timestamps.
        let rows = driver
            .query(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='todos'",
                &[],
            )
            .unwrap();
        let ddl = column_text(&rows[0], "sql");
        assert!(
            ddl.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"),
            "{ddl}"
        );
        assert!(ddl.contains("title TEXT NOT NULL"), "{ddl}");
        assert!(ddl.contains("done BOOLEAN DEFAULT FALSE"), "{ddl}");
        assert!(ddl.contains("created_at TIMESTAMP NOT NULL"), "{ddl}");

        // The index landed under its derived name, and the raw-SQL insert ran against the DSL schema.
        let indexes = driver
            .query(
                "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_todos_done'",
                &[],
            )
            .unwrap();
        assert_eq!(indexes.len(), 1);
        assert_eq!(count(&mut driver, "todos"), 1);

        // Re-running is a no-op — the DSL migration is tracked exactly like a SQL one.
        assert!(apply(&mut driver, &migrations).unwrap().is_empty());
    }

    #[test]
    fn a_dsl_migration_is_tracked_by_filename_and_canonical_checksum() {
        let mut driver = mem();
        let migrations = mixed_migrations();
        apply(&mut driver, &migrations).unwrap();

        let recorded = read_applied(&mut driver).unwrap();
        assert_eq!(recorded[0].filename, "0001_todos.schema");
        assert_eq!(recorded[0].checksum, migrations[0].checksum);
        assert_eq!(
            recorded[0].checksum,
            sha256_hex(
                crate::schema::canonicalize(&migrations[0].body)
                    .unwrap()
                    .as_bytes()
            )
        );

        // Editing a `.schema` file's *meaning* after it applied is edited history, exactly as for
        // `.sql`.
        let edited = vec![
            Migration::new(
                "0001_todos.schema",
                "create_table(\"todos\").id().text(\"t\")",
            ),
            migrations[1].clone(),
        ];
        assert!(matches!(
            apply(&mut driver, &edited).unwrap_err(),
            MigrateError::ChecksumDrift { .. }
        ));
    }

    #[test]
    fn reformatting_an_applied_dsl_migration_is_not_edited_history() {
        let mut driver = mem();
        let migrations = mixed_migrations();
        apply(&mut driver, &migrations).unwrap();

        // The same schema, run through a formatter: re-indented, re-wrapped, freshly commented, the
        // modifiers moved onto their own lines. Different bytes, same migration.
        let reformatted = vec![
            Migration::new(
                "0001_todos.schema",
                "// Reformatted after it was applied — a comment rewritten, everything re-indented.\n\
                 create_table( \"todos\" )\n\
                 \t.id()\n\
                 \t.text(\"title\")\n\
                 \t\t.not_null()\n\
                 \t.bool(\"done\")\n\
                 \t\t.default(false)\n\
                 \t.timestamps()\n\
                 \n\
                 -- and a SQL-style comment, for good measure\n\
                 create_index(\"todos\")\n\
                 \t.column(\"done\")\n",
            ),
            migrations[1].clone(),
        ];
        assert_ne!(reformatted[0].body, migrations[0].body);
        // No drift, and nothing to re-apply.
        assert!(apply(&mut driver, &reformatted).unwrap().is_empty());
        assert!(status(&mut driver, &reformatted).unwrap()[0].applied);
    }

    #[test]
    fn an_alter_table_dsl_migration_evolves_an_existing_table() {
        let mut driver = mem();
        let mut migrations = mixed_migrations();
        apply(&mut driver, &migrations).unwrap();

        migrations.push(Migration::new(
            "0003_notes.schema",
            "alter_table(\"todos\")\n\
             .add_text(\"note\")\n\
             .add_bool(\"archived\").not_null().default(false)\n",
        ));
        assert_eq!(
            apply(&mut driver, &migrations).unwrap(),
            vec!["0003_notes.schema"]
        );
        // Both new columns exist and the pre-existing row got the declared default.
        let rows = driver
            .query("SELECT note, archived FROM todos", &[])
            .unwrap();
        assert_eq!(rows[0][0].1, SqlValue::Null);
        assert_eq!(rows[0][1].1, SqlValue::Int(0)); // SQLite reads a boolean back as 0/1
    }

    #[test]
    fn a_malformed_dsl_migration_fails_before_it_opens_a_transaction() {
        let mut driver = mem();
        let migrations = vec![
            Migration::new("0001_ok.sql", "CREATE TABLE ok (id INTEGER);"),
            Migration::new("0002_bad.schema", "create_table(\"bad\").frobnicate()"),
        ];
        let err = apply(&mut driver, &migrations).unwrap_err();
        match &err {
            MigrateError::Schema { filename, message } => {
                assert_eq!(filename, "0002_bad.schema");
                assert!(message.contains("frobnicate"), "{message}");
            }
            other => panic!("expected a schema error, got {other:?}"),
        }
        assert!(err.to_string().contains("not valid portable schema DSL"));
        // The prior migration is committed and no transaction was left open — a following statement
        // succeeds, which it could not inside an abandoned `BEGIN`.
        assert!(table_exists(&mut driver, "ok"));
        driver
            .execute_batch("CREATE TABLE after (id INTEGER)")
            .unwrap();
    }

    #[test]
    fn seed_only_runs_seeds_once_the_schema_is_current() {
        let mut driver = mem();
        let migrations = migrations();
        apply(&mut driver, &migrations).unwrap();

        let seeds = vec![Migration::new(
            "0001_a.sql",
            "INSERT INTO users (id, name) VALUES (1, 'Ada');",
        )];
        let ran = seed_only(&mut driver, &migrations, &seeds).unwrap();
        assert_eq!(ran, vec!["0001_a.sql"]);
        assert_eq!(count(&mut driver, "users"), 1);
    }
}

/// A full migration round-trip against a **live** PostgreSQL, run only when `NOETA_PG_TEST_DSN` is set
/// (the same env gate the driver's own live tests use), so the default suite stays hermetic. Proves
/// the engine is genuinely driver-agnostic: apply, idempotent re-run, status, and reset all work over
/// Postgres transactional DDL exactly as over SQLite.
#[cfg(all(test, feature = "ring-postgres"))]
mod postgres_e2e {
    use super::*;
    use crate::pg::PostgresDriver;

    #[test]
    fn migrate_round_trip_against_a_live_server() {
        // Serialize against the other live-server tests: they share one database and each
        // wipes it, so concurrent runs race in the system catalog. Held for the whole test.
        let _pg = crate::pg_test_guard();
        let Ok(dsn) = std::env::var("NOETA_PG_TEST_DSN") else {
            return; // no server configured — skip (the hermetic + SQLite tests still ran)
        };
        let mut driver = PostgresDriver::connect(&dsn).expect("connect to NOETA_PG_TEST_DSN");
        // Start from a clean schema so repeated CI runs are deterministic.
        driver.reset().expect("reset");

        let migrations = vec![
            Migration::new(
                "0001_widgets.sql",
                "CREATE TABLE widgets (id INT PRIMARY KEY, name TEXT NOT NULL);",
            ),
            Migration::new(
                "0002_seed.sql",
                "INSERT INTO widgets (id, name) VALUES (1, 'alpha');\n\
                 INSERT INTO widgets (id, name) VALUES (2, 'beta');",
            ),
        ];

        let applied = apply(&mut driver, &migrations).unwrap();
        assert_eq!(applied, vec!["0001_widgets.sql", "0002_seed.sql"]);
        let rows = driver
            .query("SELECT COUNT(*) AS n FROM widgets", &[])
            .unwrap();
        assert_eq!(rows[0][0].1, SqlValue::Int(2));

        // Idempotent re-run.
        assert!(apply(&mut driver, &migrations).unwrap().is_empty());

        // Status reports both applied.
        let st = status(&mut driver, &migrations).unwrap();
        assert!(st.iter().all(|s| s.applied));

        // Seeds run against the up-to-date schema, untracked and re-runnable via the Postgres idiom.
        let seeds = vec![Migration::new(
            "0001_widgets.sql",
            "INSERT INTO widgets (id, name) VALUES (3, 'gamma') ON CONFLICT DO NOTHING;",
        )];
        let ran = seed_only(&mut driver, &migrations, &seeds).unwrap();
        assert_eq!(ran, vec!["0001_widgets.sql"]);
        assert_eq!(
            driver
                .query("SELECT COUNT(*) AS n FROM widgets", &[])
                .unwrap()[0][0]
                .1,
            SqlValue::Int(3)
        );
        // Re-running the idempotent seed is a no-op (ON CONFLICT DO NOTHING), and it was never tracked.
        seed(&mut driver, &seeds).unwrap();
        assert_eq!(
            driver
                .query("SELECT COUNT(*) AS n FROM widgets", &[])
                .unwrap()[0][0]
                .1,
            SqlValue::Int(3)
        );

        // Reset wipes and re-applies.
        let reapplied = reset(&mut driver, &migrations).unwrap();
        assert_eq!(reapplied.len(), 2);

        driver.reset().expect("final cleanup");
    }

    /// The **same** `.schema` migration the SQLite e2e applies, run against a live Postgres: the one
    /// file produces a `BIGSERIAL` identity here and an `AUTOINCREMENT` rowid there, and the raw-SQL
    /// migration beside it inserts into the result either way. This is the portability claim, proven
    /// end to end. (The lowered Postgres DDL string itself is asserted hermetically in
    /// [`crate::schema`]'s tests, so this test's absence never leaves the Postgres path uncovered.)
    #[test]
    fn a_portable_schema_migration_applies_to_postgres_too() {
        // Serialize against the other live-server tests: they share one database and each
        // wipes it, so concurrent runs race in the system catalog. Held for the whole test.
        let _pg = crate::pg_test_guard();
        let Ok(dsn) = std::env::var("NOETA_PG_TEST_DSN") else {
            return; // no server configured — skip
        };
        let mut driver = PostgresDriver::connect(&dsn).expect("connect to NOETA_PG_TEST_DSN");
        driver.reset().expect("reset");

        let migrations = vec![
            Migration::new(
                "0001_todos.schema",
                "create_table(\"todos\")\n\
                 .id()\n\
                 .text(\"title\").not_null()\n\
                 .bool(\"done\").default(false)\n\
                 .timestamps()\n\
                 \n\
                 create_index(\"todos\").column(\"done\")\n",
            ),
            Migration::new(
                "0002_first_todo.sql",
                "INSERT INTO todos (title) VALUES ('write a migration');",
            ),
        ];

        let applied = apply(&mut driver, &migrations).unwrap();
        assert_eq!(applied, vec!["0001_todos.schema", "0002_first_todo.sql"]);

        // `id` was auto-assigned by the sequence BIGSERIAL created, and `done` took its default.
        let rows = driver
            .query("SELECT id, title, done FROM todos ORDER BY id", &[])
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].1, SqlValue::Int(1));
        assert_eq!(rows[0][2].1, SqlValue::Bool(false));

        // The index exists under the same derived name it gets on SQLite.
        let indexes = driver
            .query(
                "SELECT indexname FROM pg_indexes WHERE tablename = 'todos' \
                 AND indexname = 'idx_todos_done'",
                &[],
            )
            .unwrap();
        assert_eq!(indexes.len(), 1);

        // Idempotent re-run, and the checksum recorded is the canonical rendering of the DSL's IR —
        // byte-identical to the one SQLite would record for the same file, since no dialect is on
        // that path.
        assert!(apply(&mut driver, &migrations).unwrap().is_empty());
        let recorded = read_applied(&mut driver).unwrap();
        assert_eq!(recorded[0].checksum, migrations[0].checksum);

        driver.reset().expect("final cleanup");
    }
}
