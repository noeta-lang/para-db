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
//! **Noeta or SQL, one ordering.** A migration's extension picks how its body becomes DDL
//! ([`MigrationKind`]):
//!   * `.noe` — a **Noeta program** that *describes* the change: it declares `migrate(): List<Statement>`
//!     and returns the schema statements it wants, built with the [`crate::schema`] builder. It takes
//!     no connection and touches no database. The engine runs it through a [`SchemaEmitter`] to get
//!     the canonical IR back ([`resolve_programs`]), then checksums, lowers and applies that — so one
//!     file migrates both SQLite and Postgres, written in the same language as the rest of the app.
//!   * `.sql` — run **verbatim** in the target dialect's native SQL (via [`SqlDriver::execute_batch`],
//!     no `?`→`$N` rewrite). The escape hatch: anything dialect-specific is written here.
//!
//! Both live in the **same** directory and interleave in one filename order, so a project writes
//! Noeta wherever the schema vocabulary reaches and drops to raw SQL for the steps it does not.
//!
//! **`.schema` is the IR, not a third language.** The portable schema DSL
//! ([`MigrationKind::Schema`]) is what a `.noe` migration *compiles down to* — the same text
//! [`crate::schema::render`] produces from a `Vec<Statement>`. It remains readable and remains
//! accepted as a body language, because it is the one form both the builder and the parser agree on
//! byte for byte, but it is no longer something a project needs to learn or write: `migrate new`
//! scaffolds `.noe`, and `--sql` is the other choice.
//!
//! A **seed** shares all three, and gives `.noe` its other meaning — see the seeds section below.
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
//!   * `.noe` — the **canonical IR its `migrate()` returned**, which is the `.schema` case below applied to
//!     the emitted text. The Noeta source is never hashed: it is a program, and two programs that
//!     build the same statements are the same migration. Reformat it, rename a local, pull a repeated
//!     column list into a helper — same identity. Add a column and it changes, because the IR did.
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
//! **Seeds.** Files under a project `seeds/` directory, discovered and ordered by the very same
//! [`load_dir`] loader migrations use, run **after** migrations by [`seed`]. Seeds are re-runnable
//! development data, **never** recorded in the tracking table: each runs every time it is invoked,
//! so idempotency is the seed author's concern (the portable `ON CONFLICT DO NOTHING` idiom — the
//! SQLite-only `INSERT OR IGNORE` spelling of the same idea is a hard syntax error on Postgres).
//! [`seed_only`] refuses to run when migrations are pending — seeding a stale schema is a footgun.
//!
//! A seed's `.noe` is the **other** meaning of a Noeta body — a program that *performs* rather than
//! describes:
//!   * `.sql` — verbatim SQL, in its own transaction. The permanent escape hatch.
//!   * `.noe` — an ordinary program that declares `seed(conn)`, connects, and writes its rows through
//!     the `para.db.query` builder, so one seed file seeds every backend without its author writing
//!     dialect SQL. The engine owns discovery, ordering and stop-on-first-failure; *running* the
//!     program is delegated to a [`ProgramRunner`], because only the CLI can load, check and run a
//!     program on the real host (`CommandCtx::run_file`, behind `noeta migrate`).
//!   * `.schema` — the IR, lowered per driver (rarely what a seed wants — a seed is data — but the
//!     two directories deliberately understand the same files).
//!
//! **The directory is what a `.noe` file's entry point means.** Under `migrations/` a program is
//! asked for `migrate(): List<Statement>` and never sees a connection; under `seeds/` it is asked for
//! `seed(conn)` and is handed one. Both are the same synthesized-entry mechanism ([`SchemaEmitter`]
//! and [`ProgramRunner`], the two runner seams) — what differs is what the engine asks the program
//! *for*, and therefore what it is allowed to do. A migration that could open its own connection
//! could write outside the engine's transaction and leave history it never recorded; asking it only
//! to return statements is what makes that unrepresentable rather than merely discouraged.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::driver::{SqlDriver, SqlValue};

/// The migration tracking table — one row per applied migration. Portable DDL: `CURRENT_TIMESTAMP`
/// and this column set work identically on SQLite and Postgres, so the runner needs no per-dialect
/// tracking schema.
pub const TRACKING_TABLE: &str = "_noeta_migrations";

/// The extension of a raw-SQL migration — the body is run verbatim.
pub const SQL_EXTENSION: &str = "sql";

/// The extension of a schema-IR body — the body is lowered per driver. What a `.noe` migration
/// compiles down to, and accepted on disk in its own right.
pub const SCHEMA_EXTENSION: &str = "schema";

/// The extension of a **Noeta program** body — the file is run as a program, not as SQL. Under
/// `migrations/` it is asked for `migrate()` and describes; under `seeds/` it is asked for `seed(conn)`
/// and performs ([`DirKind`]).
pub const PROGRAM_EXTENSION: &str = "noe";

/// How a migration's or seed's body becomes an effect on the database. Decided by the file's
/// **extension**, so the body language is visible in the directory listing and needs no in-file
/// marker (which a checksum would then have to treat as history).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationKind {
    /// A `.sql` body: native SQL for the connected backend, run verbatim.
    Sql,
    /// A `.schema` body: the portable schema IR, lowered by the driver at apply time. Also what a
    /// resolved `.noe` *migration* becomes — [`resolve_programs`] flips the kind once `migrate()` has run
    /// and its canonical IR is in hand, which is the whole of "compiled down to the IR".
    Schema,
    /// A `.noe` body: a Noeta program, run through a runner seam rather than by the driver. Under
    /// `seeds/` it stays `Program` and is run by a [`ProgramRunner`]; under `migrations/` it is
    /// *unresolved* until a [`SchemaEmitter`] has turned it into [`MigrationKind::Schema`].
    Program,
}

impl MigrationKind {
    /// The kind a filename declares — `.schema` → [`MigrationKind::Schema`], `.noe` →
    /// [`MigrationKind::Program`], anything else raw SQL (the loader only ever offers names whose
    /// extension it already accepted).
    fn of(name: &str) -> MigrationKind {
        let has_extension = |wanted: &str| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case(wanted))
        };
        if has_extension(SCHEMA_EXTENSION) {
            MigrationKind::Schema
        } else if has_extension(PROGRAM_EXTENSION) {
            MigrationKind::Program
        } else {
            MigrationKind::Sql
        }
    }
}

/// Which directory a load is for — the **body-language gate**, and the noun the loader's errors use.
///
/// The loader is deliberately shared between migrations and seeds so the two directories order files
/// identically. Both accept all three bodies, but a `.noe` file means a different thing in each — see
/// [`DirKind::Migrations`] and [`DirKind::Seeds`] — so every call names its purpose and the entry
/// convention a program is held to is never inferred from the file alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirKind {
    /// The migrations directory: tracked, checksummed history. A `.noe` here **describes** (`migrate()`).
    Migrations,
    /// The seeds directory: untracked, re-runnable data. A `.noe` here **performs** (`seed(conn)`).
    Seeds,
}

impl DirKind {
    /// The noun this directory is reported as ("migration" / "seed").
    fn noun(self) -> &'static str {
        match self {
            DirKind::Migrations => "migration",
            DirKind::Seeds => "seed",
        }
    }

    /// The directory's own name in an error ("migrations directory" / "seeds directory").
    fn label(self) -> &'static str {
        match self {
            DirKind::Migrations => "migrations",
            DirKind::Seeds => "seeds",
        }
    }

    /// The entry point a `.noe` file in this directory must declare — `migrate` to describe a schema
    /// change, `seed` to write rows. What the synthesized entry call names, and what an author reads
    /// in a scaffold.
    pub fn program_entry(self) -> &'static str {
        match self {
            DirKind::Migrations => crate::program::MIGRATION_ENTRY_IDENT,
            DirKind::Seeds => crate::program::SEED_ENTRY_IDENT,
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
    /// Where the file lives — what a [`MigrationKind::Program`] seed is run from (a program is
    /// loaded from disk by the CLI, not handed over as a string). [`Migration::new`] defaults it to
    /// the bare filename; [`load_dir`] sets the real path.
    pub path: PathBuf,
}

impl Migration {
    /// Build a migration from a filename and its contents, computing the checksum and reading the
    /// body language off the extension. The path defaults to the filename itself — [`load_dir`]
    /// replaces it with the file's real location.
    pub fn new(name: impl Into<String>, body: impl Into<String>) -> Migration {
        let name = name.into();
        let body = body.into();
        let kind = MigrationKind::of(&name);
        let checksum = sha256_hex(identity_text(kind, &body).as_bytes());
        let path = PathBuf::from(&name);
        Migration {
            name,
            body,
            checksum,
            kind,
            path,
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
            // An *unresolved* program body has no SQL to lower — its `migrate()` has not run, so nothing
            // knows what it describes. The seed engine never reaches here (it routes a program to
            // its [`ProgramRunner`] first) and neither does any caller that ran
            // [`resolve_programs`], which is precisely the invariant this arm reports when it is
            // missed rather than papering over with a skip.
            MigrationKind::Program => Err(MigrateError::UnresolvedProgram {
                filename: self.name.clone(),
            }),
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
    /// A `.noe` migration's `migrate()` could not be run, or produced IR that does not parse back. The
    /// message carries which — a missing loader (the in-process surface), a program that failed to
    /// check or run, or a program that built something the IR grammar cannot express.
    Emit { filename: String, message: String },
    /// A `.noe` migration reached lowering **unresolved** — [`resolve_programs`] was not run for it.
    /// An engine invariant rather than a user error: it means a caller assembled its own migration
    /// list and skipped the resolution step, and it is reported instead of skipped because a
    /// silently ignored migration is a schema that differs between two machines.
    UnresolvedProgram { filename: String },
    /// A `.noe` seed could not be run because the caller has no way to run a program — the
    /// programmatic `Connection.seed(dir)` surface, which drives a database driver, not the CLI's
    /// loader. Carries the reason so the message names the one path that does work.
    ProgramUnsupported { filename: String, message: String },
    /// A `.noe` seed **ran** and failed (a check error, a panic, a non-zero exit). Distinct from
    /// [`MigrateError::Seed`] because nothing was rolled back for it: a program owns its own
    /// connection and its own transactions, so whatever it committed before failing stands.
    SeedProgram { filename: String, message: String },
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
            MigrateError::Emit { filename, message } => write!(
                f,
                "migration `{filename}` did not produce a schema: {message}"
            ),
            MigrateError::UnresolvedProgram { filename } => write!(
                f,
                "migration `{filename}` is a Noeta program whose `migrate()` has not been run, so what \
                 it describes is not known yet — `migrate::resolve_programs` has to run between \
                 loading and applying. This is an engine invariant, not something the file did \
                 wrong.",
            ),
            MigrateError::ProgramUnsupported { filename, message } => {
                write!(f, "seed `{filename}` cannot run here: {message}")
            }
            MigrateError::SeedProgram { filename, message } => write!(
                f,
                "seed program `{filename}` failed: {message}. Seeds before it stand — a `.noe` seed \
                 owns its own connection, so the engine has no transaction to roll back for it.",
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
/// A [`MigrationKind::Program`] body hashes as its source text, which is right for the only case
/// that keeps that kind past discovery: a *seed*, which is not history, so nothing reads the
/// checksum — computing it keeps discovery uniform. A `.noe` **migration** never hashes here;
/// [`resolve_programs`] replaces its body with the IR `migrate()` returned and rehashes it as a
/// [`MigrationKind::Schema`] one, so the Noeta source is not part of its identity.
fn identity_text(kind: MigrationKind, body: &str) -> String {
    match kind {
        MigrationKind::Sql | MigrationKind::Program => body.to_string(),
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

/// Discover every body file directly under `dir`, sorted by filename, each with its checksum, its
/// body language ([`MigrationKind`]) and its path. The shared loader for **both** migrations and
/// seeds — [`seed`] reuses it for the `seeds/` dir (a seed simply ignores the computed checksum,
/// since seeds are not tracked history).
///
/// `kind` says which directory this is, which is how the loader's errors read and — for a `.noe`
/// body — which entry convention the file will be held to ([`DirKind::program_entry`]). All three
/// extensions load in both directories; a `.noe` *migration* comes back unresolved
/// ([`MigrationKind::Program`]) and must go through [`resolve_programs`] before it can be planned or
/// applied.
///
/// The extensions share **one** ordering: the sort is over the whole filename, so a raw-SQL and a DSL
/// migration interleave by their timestamp prefixes exactly as two `.sql` files would, and the
/// tracking table needs no notion of body language (the filename it already records carries it).
///
/// A missing directory is an error (the caller asked to migrate but there is nowhere to read from);
/// an *empty* existing directory yields an empty list (migrate is then a clean no-op). Sub-directories
/// and files with any other extension are ignored — leaving room for a future per-dialect
/// `dir/postgres/` overlay without disturbing what exists.
pub fn load_dir(dir: &Path, kind: DirKind) -> Result<Vec<Migration>, MigrateError> {
    let label = kind.label();
    if !dir.exists() {
        return Err(MigrateError::Io(format!(
            "{label} directory `{}` does not exist",
            dir.display()
        )));
    }
    let entries = std::fs::read_dir(dir).map_err(|e| {
        MigrateError::Io(format!(
            "cannot read {label} directory `{}`: {e}",
            dir.display()
        ))
    })?;

    let mut migrations = Vec::new();
    for entry in entries {
        let entry =
            entry.map_err(|e| MigrateError::Io(format!("reading `{}`: {e}", dir.display())))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // A body file, or not one at all (a README, a `.bak`, a `.gitkeep`) — anything else is
        // ignored in both directories, as it always has been. Which of the three it *is* comes from
        // `MigrationKind::of` below, off the same filename; this only decides whether to look.
        let is_body = path.extension().is_some_and(|ext| {
            [SQL_EXTENSION, SCHEMA_EXTENSION, PROGRAM_EXTENSION]
                .iter()
                .any(|wanted| ext.eq_ignore_ascii_case(wanted))
        });
        if !is_body {
            continue;
        }
        let body = std::fs::read_to_string(&path).map_err(|e| {
            MigrateError::Io(format!(
                "cannot read {} `{}`: {e}",
                kind.noun(),
                path.display()
            ))
        })?;
        migrations.push(Migration {
            path,
            ..Migration::new(name, body)
        });
    }
    migrations.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(migrations)
}

/// Run a `.noe` **migration** and hand back the canonical schema IR its `migrate()` returned.
///
/// The migrations-directory twin of [`ProgramRunner`], and the reason the two are separate traits:
/// this one is handed a path and gets *a value* back, having opened no database at all, while a
/// [`ProgramRunner`] is handed a path and a live dsn and gets only success or failure back. A caller
/// that can run programs (the CLI) implements both; a caller that cannot implements neither, and the
/// engine's error says which of the two it needed.
pub trait SchemaEmitter {
    /// Load, check and run the program at `path`, calling its `migrate()` and returning
    /// [`crate::schema::render`]-canonical source for the statements it built.
    fn emit(&mut self, path: &Path) -> Result<String, ProgramFailure>;
}

/// The [`SchemaEmitter`] for callers with no way to run a program — the programmatic
/// `Connection.migrate(dir)` surface, which has a database but no loader. Every `.noe` migration is
/// refused by name, so an in-process migrate never silently skips one.
#[derive(Debug)]
pub struct UnsupportedEmitter;

impl SchemaEmitter for UnsupportedEmitter {
    fn emit(&mut self, path: &Path) -> Result<String, ProgramFailure> {
        Err(ProgramFailure::Unsupported(format!(
            "`{}` is a Noeta migration, and running one needs the loader that `noeta migrate` has \
             and a running program does not",
            path.display()
        )))
    }
}

/// Resolve every unresolved `.noe` migration in `migrations` **in place**: run its `migrate()` through
/// `emitter`, replace its body with the canonical IR that came back, recompute its checksum over
/// that IR, and flip its kind to [`MigrationKind::Schema`]. Anything already resolved, and every
/// `.sql` / `.schema` file, is left untouched.
///
/// Called once, right after [`load_dir`], **before** [`plan`] — because a `.noe` migration's identity
/// is the IR it describes, so nothing can be planned, drift-checked or applied until `migrate()` has run.
/// That it runs on every verb (`--status` and `--dry-run` included) is deliberate: `migrate()` takes no
/// connection and has no effects, so running it is how the engine learns what the file *means*, and a
/// status that skipped it would report on a checksum it had not computed.
pub fn resolve_programs(
    migrations: &mut [Migration],
    emitter: &mut dyn SchemaEmitter,
) -> Result<(), MigrateError> {
    for migration in migrations.iter_mut() {
        if migration.kind != MigrationKind::Program {
            continue;
        }
        let ir = emitter.emit(&migration.path).map_err(|failure| {
            let (ProgramFailure::Failed(message) | ProgramFailure::Unsupported(message)) = failure;
            MigrateError::Emit {
                filename: migration.name.clone(),
                message,
            }
        })?;
        // Parsed once here so a program that builds something the IR grammar cannot express fails
        // against *its own* filename, at resolution, rather than as a mystery `.schema` parse error
        // over text the author never wrote.
        crate::schema::parse(&ir).map_err(|e| MigrateError::Emit {
            filename: migration.name.clone(),
            message: format!("`migrate()` produced schema IR that does not parse back: {e}"),
        })?;
        migration.checksum = sha256_hex(identity_text(MigrationKind::Schema, &ir).as_bytes());
        migration.body = ir;
        migration.kind = MigrationKind::Schema;
    }
    Ok(())
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
            // The migration ledger's own columns are declared TEXT, so this cannot arise from a
            // well-formed `_noeta_migrations` table; decode it as UTF-8 rather than drop it, so a
            // hand-built ledger still reports something recognizable.
            SqlValue::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
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

/// How a [`MigrationKind::Program`] seed body is actually run — the seam between this engine, which
/// owns discovery, ordering and stop-on-first-failure, and the one caller that can load, check and
/// run a Noeta program on the real host.
///
/// Only the CLI can do that (`CommandCtx::run_file`, the same mechanism behind `noeta serve`), and
/// this crate must not depend on the CLI, so the engine takes the capability as an argument instead.
/// Callers that have no program driver pass [`UnsupportedPrograms`], which turns a `.noe` seed into a
/// clear error naming the path that does work.
pub trait ProgramRunner {
    /// Run the seed program at `path` to completion.
    fn run_program(&mut self, path: &Path) -> Result<(), ProgramFailure>;
}

/// Why a [`ProgramRunner`] did not complete a seed program — kept apart because they are different
/// problems for the person reading the output: one is a program bug, the other is "you are driving
/// seeds from a surface that cannot run programs".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramFailure {
    /// The program ran (loaded, checked, executed) and did not succeed.
    Failed(String),
    /// This runner cannot run programs at all; the message names the path that can.
    Unsupported(String),
}

/// The [`ProgramRunner`] for callers with no way to run a program — the programmatic
/// `Connection.seed(dir)` surface, which holds a database driver, not the CLI's loader. Every `.noe`
/// seed it meets becomes a [`MigrateError::ProgramUnsupported`] naming the command that can run it.
#[derive(Debug, Clone, Copy)]
pub struct UnsupportedPrograms;

impl ProgramRunner for UnsupportedPrograms {
    fn run_program(&mut self, path: &Path) -> Result<(), ProgramFailure> {
        Err(ProgramFailure::Unsupported(format!(
            "`{}` is a `.noe` seed — a Noeta program, which only the CLI can load, check and run. \
             Run the seeds with `noeta migrate --seed` (or `noeta migrate seed`); \
             `conn.seed(dir)` runs `.sql` and `.schema` seed bodies only.",
            path.display()
        )))
    }
}

/// Run every **seed** file in `seeds` in filename order, returning the names run. Seeds are
/// re-runnable development data, **never tracked** in [`TRACKING_TABLE`]: this applies every file
/// every time it is called, so idempotency is the seed author's concern (`ON CONFLICT DO NOTHING`,
/// which both backends accept, or the `para.db.query` builder's `insert_or_ignore`/`upsert` that
/// emit it). A seed uses the same discovery/ordering as a migration — [`load_dir`] loads both — but
/// no checksum is recorded (a seed is not history). The first failure stops the run, naming the
/// file, with the prior seeds left committed — the same stop-on-first-failure shape as [`apply`].
///
/// The body language decides how a file runs: a `.sql`/`.schema` seed goes to `driver` inside its own
/// transaction ([`seed_one`]), while a `.noe` seed goes to `programs` — a separate program with its
/// own connection, and therefore its own transactions (an implicit outer transaction here would
/// collide with any the program opens itself, e.g. through a repository's `flush`).
pub fn seed(
    driver: &mut dyn SqlDriver,
    seeds: &[Migration],
    programs: &mut dyn ProgramRunner,
) -> Result<Vec<String>, MigrateError> {
    let mut done = Vec::with_capacity(seeds.len());
    for file in seeds {
        match file.kind {
            MigrationKind::Program => {
                programs
                    .run_program(&file.path)
                    .map_err(|failure| match failure {
                        ProgramFailure::Failed(message) => MigrateError::SeedProgram {
                            filename: file.name.clone(),
                            message,
                        },
                        ProgramFailure::Unsupported(message) => MigrateError::ProgramUnsupported {
                            filename: file.name.clone(),
                            message,
                        },
                    })?;
            }
            MigrationKind::Sql | MigrationKind::Schema => seed_one(driver, file)?,
        }
        done.push(file.name.clone());
    }
    Ok(done)
}

/// Run one seed file inside its own transaction: `BEGIN`, the verbatim body, `COMMIT` — no tracking
/// insert (seeds are not history). Any failure rolls back (best-effort) and reports the file.
fn seed_one(driver: &mut dyn SqlDriver, file: &Migration) -> Result<(), MigrateError> {
    // Seeds share the loader, so they share the SQL body languages too: a `.sql` seed runs verbatim
    // and a `.schema` one lowers through the driver first (rarely what a seed wants — seeds are data
    // — but there is no reason for the two directories to understand different files). A `.noe` seed
    // never reaches here: [`seed`] routes it to the [`ProgramRunner`] instead.
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
    programs: &mut dyn ProgramRunner,
) -> Result<Vec<String>, MigrateError> {
    ensure_tracking_table(driver)?;
    let applied = read_applied(driver)?;
    let plan = plan(migrations, &applied)?;
    if !plan.pending.is_empty() {
        return Err(MigrateError::PendingMigrations {
            pending: plan.pending.len(),
        });
    }
    seed(driver, seeds, programs)
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
     -- This body runs VERBATIM in the connected database's own SQL dialect, which is the point of\n\
     -- it: triggers, views, and anything else one backend spells its own way belong here. For a\n\
     -- migration that works on every backend, scaffold a Noeta one instead: `migrate new <name>`.\n";

/// The starter body written into a freshly scaffolded **Noeta** migration — the default. It teaches
/// the entry convention (`migrate()` returning statements, taking no connection), because neither the
/// name nor the absence of a connection is guessable, and names the raw-SQL escape hatch.
pub const MIGRATION_PROGRAM_SCAFFOLD_TEMPLATE: &str = "// Migration (Noeta): describes a schema change and returns it. `noeta migrate` runs the `migrate`\n\
     // function below, lowers what it returns to the connected driver's own DDL — SQLite gets\n\
     // `INTEGER PRIMARY KEY AUTOINCREMENT`, Postgres `BIGSERIAL PRIMARY KEY` — and applies it in a\n\
     // transaction. One file migrates every backend.\n\
     //\n\
     // The function MUST be named `migrate` and take nothing: that is the entry convention, and taking no\n\
     // connection is deliberate. A migration says what the schema should be; applying it, recording\n\
     // it and rolling it back are the engine's job, not this file's.\n\
     //\n\
     // Once applied, what is checksummed is the SCHEMA THIS FILE DESCRIBES, not its text: reformat\n\
     // it, rename a local, pull a repeated column list into a helper — same migration. Change what\n\
     // it builds and it is edited history, so make later changes in a new migration.\n\
     use para.db.schema.{Statement, create_table, create_index}\n\
     \n\
     pub fn migrate(): List<Statement> {\n\
     \x20   return [\n\
     \x20       create_table(\"todos\")\n\
     \x20           .id()\n\
     \x20           .text(\"title\").not_null()\n\
     \x20           .bool(\"done\").default(false)\n\
     \x20           .timestamps()\n\
     \x20           .statement(),\n\
     \x20       create_index(\"todos\").column(\"done\").statement(),\n\
     \x20   ]\n\
     }\n\
     \n\
     // Statements: create_table, alter_table, drop_table, create_index, drop_index. The vocabulary is\n\
     // deliberately only what lowers to equivalent DDL on both backends; anything dialect-specific\n\
     // (views, triggers, json/uuid/decimal columns, partial indexes) belongs in a raw `.sql` migration\n\
     // beside this one — `migrate new <name> --sql`. The two interleave in filename order.\n";

/// The starter body written into a freshly scaffolded raw-SQL seed file — re-runnable dev data, so it
/// documents the idempotent-insert idiom inline, in the spelling that works on **both** backends.
pub const SEED_SCAFFOLD_TEMPLATE: &str = "-- Seed: re-runnable development data. This file runs on every `noeta migrate seed` / `--seed`,\n\
     -- each in its own transaction, and is NOT tracked. Make inserts idempotent so a re-run is a\n\
     -- no-op — `ON CONFLICT DO NOTHING` is accepted verbatim by SQLite and PostgreSQL:\n\
     --\n\
     --   INSERT INTO users (id, name) VALUES (1, 'Ada') ON CONFLICT DO NOTHING;\n\
     --\n\
     -- (`INSERT OR IGNORE` means the same thing but is SQLite-only — it is a syntax error on\n\
     -- PostgreSQL, so a seed written that way is not portable.)\n\
     --\n\
     -- This body runs VERBATIM in the connected database's own SQL dialect. For a seed that writes\n\
     -- its rows through the portable query builder instead, scaffold a Noeta one — that is the\n\
     -- default: `migrate new --seed <name>`.\n";

/// The starter body written into a freshly scaffolded **program** (`.noe`) seed — the entry
/// convention (`fn seed(conn)`) plus the portable idempotent insert, since neither is guessable.
pub const SEED_PROGRAM_SCAFFOLD_TEMPLATE: &str = "// Seed (Noeta program): re-runnable development data written in Noeta rather than SQL, so one file\n\
     // seeds every backend. `noeta migrate --seed` / `noeta migrate seed` loads and runs it, then\n\
     // calls the `seed` function below with a connection to the project's database (the `--db` flag,\n\
     // `DATABASE_URL`, or `[db] url` in noeta.toml — resolved once, by the command).\n\
     //\n\
     // The function MUST be named `seed` and take one `Connection`: that is the entry convention.\n\
     // A program seed owns its connection, so it also owns its transactions — the engine has none to\n\
     // roll back for it.\n\
     use para.db\n\
     use para.db.query.{table, exec}\n\
     \n\
     fn seed(conn: db.Connection): void {\n\
     \x20   // `insert_or_ignore` emits `... ON CONFLICT DO NOTHING`, which SQLite and PostgreSQL both\n\
     \x20   // accept — so running the seeds twice leaves the same rows.\n\
     \x20   exec(conn, table(\"users\").insert_or_ignore([\"id\", \"name\"], [1, \"Ada\"]))\n\
     }\n";

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
    fn the_migration_scaffold_teaches_the_entry_convention_and_names_the_escape_hatch() {
        // `migrate`, its return type, and the absence of a connection parameter are the convention, and
        // none of the three is guessable from the filename.
        assert!(MIGRATION_PROGRAM_SCAFFOLD_TEMPLATE.contains("pub fn migrate(): List<Statement>"));
        assert!(MIGRATION_PROGRAM_SCAFFOLD_TEMPLATE.contains("use para.db.schema"));
        assert!(MIGRATION_PROGRAM_SCAFFOLD_TEMPLATE.contains("create_table(\"todos\")"));
        assert!(MIGRATION_PROGRAM_SCAFFOLD_TEMPLATE.contains(".timestamps()"));
        // Each starting point names the other, and `--sql` is the only body flag either mentions.
        assert!(MIGRATION_PROGRAM_SCAFFOLD_TEMPLATE.contains("--sql"));
        assert!(SCAFFOLD_TEMPLATE.contains("migrate new <name>"));
    }

    #[test]
    fn load_dir_errors_when_missing_but_is_empty_when_bare() {
        let missing = std::path::Path::new("/does/not/exist/noeta-migrate-xyz");
        assert!(matches!(
            load_dir(missing, DirKind::Migrations),
            Err(MigrateError::Io(_))
        ));

        let dir = std::env::temp_dir().join(format!("noeta-migrate-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load_dir(&dir, DirKind::Migrations).unwrap(), Vec::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_dir_reads_sorts_and_ignores_non_sql() {
        let dir = std::env::temp_dir().join(format!("noeta-migrate-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0002_b.sql"), "SELECT 2;").unwrap();
        std::fs::write(dir.join("0001_a.sql"), "SELECT 1;").unwrap();
        std::fs::write(dir.join("README.md"), "not a migration").unwrap();

        let migrations = load_dir(&dir, DirKind::Migrations).unwrap();
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

        let migrations = load_dir(&dir, DirKind::Migrations).unwrap();
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

    #[test]
    fn the_seeds_directory_takes_all_three_body_languages_in_one_order() {
        let dir = std::env::temp_dir().join(format!("noeta-seeds-mixed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0003_c.schema"), "create_table(\"c\").int(\"x\")").unwrap();
        std::fs::write(dir.join("0001_a.sql"), "SELECT 1;").unwrap();
        std::fs::write(
            dir.join("0002_b.noe"),
            "fn seed(conn: db.Connection): void {}",
        )
        .unwrap();

        let seeds = load_dir(&dir, DirKind::Seeds).unwrap();
        assert_eq!(
            seeds
                .iter()
                .map(|m| (m.name.as_str(), m.kind))
                .collect::<Vec<_>>(),
            vec![
                ("0001_a.sql", MigrationKind::Sql),
                ("0002_b.noe", MigrationKind::Program),
                ("0003_c.schema", MigrationKind::Schema),
            ]
        );
        // A program is run from disk, so the loader records where it is (a `.sql` body travels as
        // text; a `.noe` body travels as a path).
        assert_eq!(seeds[1].path, dir.join("0002_b.noe"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A [`SchemaEmitter`] that returns canned IR — the resolution seam exercised with no CLI, the
    /// way [`RecordingRunner`] does for seeds.
    struct CannedEmitter {
        ir: String,
        seen: Vec<PathBuf>,
    }

    impl SchemaEmitter for CannedEmitter {
        fn emit(&mut self, path: &Path) -> Result<String, ProgramFailure> {
            self.seen.push(path.to_path_buf());
            Ok(self.ir.clone())
        }
    }

    #[test]
    fn a_noe_migration_resolves_to_the_ir_its_up_returned() {
        // The whole of "written in Noeta, compiled down to the schema IR when run": the file is
        // discovered unresolved, `migrate()` produces the IR, and from that point the engine is looking
        // at an ordinary schema migration whose identity is what it describes.
        let dir = std::env::temp_dir().join(format!("noeta-noe-migration-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("0001_a.sql"), "SELECT 1;").unwrap();
        std::fs::write(
            dir.join("0002_b.noe"),
            "pub fn migrate(): List<Statement> { }",
        )
        .unwrap();

        let mut migrations = load_dir(&dir, DirKind::Migrations).unwrap();
        assert_eq!(migrations[1].kind, MigrationKind::Program);

        let ir = "create_table(\"todos\").id().text(\"title\").not_null()\n";
        let mut emitter = CannedEmitter {
            ir: ir.to_string(),
            seen: Vec::new(),
        };
        resolve_programs(&mut migrations, &mut emitter).unwrap();

        // The program was run from its real path, and what came back is now the body.
        assert_eq!(emitter.seen, vec![dir.join("0002_b.noe")]);
        assert_eq!(migrations[1].kind, MigrationKind::Schema);
        assert_eq!(migrations[1].body, ir);
        // The identity is the IR the program produced, never the Noeta source that produced it — so
        // rewriting a migration's *program* without changing what it describes is not a history
        // edit. (`Migration::new` takes a name only to pick the body language from its extension.)
        assert_eq!(
            migrations[1].checksum,
            Migration::new("x.schema", ir).checksum
        );
        // The `.sql` file beside it is untouched by resolution.
        assert_eq!(migrations[0].kind, MigrationKind::Sql);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unresolved_noe_migration_is_reported_not_skipped() {
        // Skipping one would leave two machines with different schemas and no error to explain why,
        // so the invariant is enforced where the body would have been lowered.
        let migration = Migration::new("0001_a.noe", "pub fn migrate(): List<Statement> { }");
        assert_eq!(migration.kind, MigrationKind::Program);
        let err = migration
            .lowered(&crate::sqlite::SqliteDriver::open_in_memory().unwrap())
            .unwrap_err();
        assert_eq!(
            err,
            MigrateError::UnresolvedProgram {
                filename: "0001_a.noe".to_string()
            }
        );
    }

    #[test]
    fn the_in_process_surface_names_the_path_that_runs_a_noe_migration() {
        // `Connection.migrate(dir)` has a database but no loader. It refuses by name rather than
        // skipping, and the message says which command does work.
        let mut migrations = vec![Migration::new("0001_a.noe", "pub fn migrate() { }")];
        let err = resolve_programs(&mut migrations, &mut UnsupportedEmitter).unwrap_err();
        let rendered = err.to_string();
        assert!(rendered.contains("0001_a.noe"), "{rendered}");
        assert!(rendered.contains("noeta migrate"), "{rendered}");
    }

    #[test]
    fn the_program_seed_scaffold_teaches_the_entry_convention_and_the_portable_idiom() {
        assert!(SEED_PROGRAM_SCAFFOLD_TEMPLATE.contains("fn seed(conn: db.Connection)"));
        assert!(SEED_PROGRAM_SCAFFOLD_TEMPLATE.contains("insert_or_ignore"));
        assert!(SEED_PROGRAM_SCAFFOLD_TEMPLATE.contains("use para.db.query"));
        // The SQL seed scaffold points at the Noeta one, so either starting point names the other.
        assert!(SEED_SCAFFOLD_TEMPLATE.contains("migrate new --seed <name>"));
    }
}

/// The [`ProgramRunner`] seam, exercised without a CLI: a recording fake stands in for
/// `CommandCtx::run_file`, so ordering, delegation and the two failure shapes are unit-testable.
#[cfg(all(test, feature = "ring-sqlite"))]
mod program_seeds {
    use super::*;
    use crate::sqlite::SqliteDriver;

    /// Records the programs it was asked to run, and can be told to fail on one of them.
    struct RecordingRunner {
        ran: Vec<String>,
        fail_on: Option<&'static str>,
    }

    impl RecordingRunner {
        fn new() -> RecordingRunner {
            RecordingRunner {
                ran: Vec::new(),
                fail_on: None,
            }
        }
    }

    impl ProgramRunner for RecordingRunner {
        fn run_program(&mut self, path: &Path) -> Result<(), ProgramFailure> {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let failing = self.fail_on == Some(name.as_str());
            self.ran.push(name);
            if failing {
                return Err(ProgramFailure::Failed("boom".to_string()));
            }
            Ok(())
        }
    }

    const PROGRAM_BODY: &str = "fn seed(conn: db.Connection): void {}";

    #[test]
    fn sql_seeds_go_to_the_driver_and_program_seeds_to_the_runner_in_one_order() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        driver
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        let seeds = vec![
            Migration::new("0001_a.sql", "INSERT INTO t (id) VALUES (1);"),
            Migration::new("0002_b.noe", PROGRAM_BODY),
            Migration::new("0003_c.sql", "INSERT INTO t (id) VALUES (3);"),
        ];

        let mut runner = RecordingRunner::new();
        let ran = seed(&mut driver, &seeds, &mut runner).unwrap();

        // Every file is reported, in filename order, whatever its body language.
        assert_eq!(ran, vec!["0001_a.sql", "0002_b.noe", "0003_c.sql"]);
        // Only the program went to the runner; the SQL bodies went to the database.
        assert_eq!(runner.ran, vec!["0002_b.noe"]);
        assert_eq!(
            driver
                .query("SELECT id FROM t ORDER BY id", &[])
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn a_failing_program_seed_stops_the_run_and_names_the_file() {
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        driver
            .execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        let seeds = vec![
            Migration::new("0001_a.noe", PROGRAM_BODY),
            Migration::new("0002_b.noe", PROGRAM_BODY),
            Migration::new("0003_c.sql", "INSERT INTO t (id) VALUES (3);"),
        ];

        let mut runner = RecordingRunner::new();
        runner.fail_on = Some("0002_b.noe");
        let err = seed(&mut driver, &seeds, &mut runner).unwrap_err();

        assert_eq!(
            err,
            MigrateError::SeedProgram {
                filename: "0002_b.noe".to_string(),
                message: "boom".to_string()
            }
        );
        // Stop-on-first-failure: the seed after it never ran.
        assert_eq!(runner.ran, vec!["0001_a.noe", "0002_b.noe"]);
        assert_eq!(driver.query("SELECT id FROM t", &[]).unwrap().len(), 0);
    }

    #[test]
    fn a_driver_only_caller_reports_a_program_seed_as_unsupported() {
        // What `conn.seed(dir)` does: it holds a driver, not the CLI's loader, so it cannot run a
        // program — and says so, naming the command that can, rather than skipping the file.
        let mut driver = SqliteDriver::open_in_memory().unwrap();
        let seeds = vec![Migration::new("0001_a.noe", PROGRAM_BODY)];

        let err = seed(&mut driver, &seeds, &mut UnsupportedPrograms).unwrap_err();
        let rendered = err.to_string();
        assert!(matches!(err, MigrateError::ProgramUnsupported { .. }));
        assert!(rendered.contains("0001_a.noe"), "{rendered}");
        assert!(rendered.contains("noeta migrate --seed"), "{rendered}");
    }
}

/// The exact statements the `para.db.query` builder's `insert_or_ignore` and `upsert` terminals
/// emit — the same two strings `examples/para-db-demo/conflict_demo.noe`'s `@test` block asserts the
/// builder produces.
///
/// The builder is pure Noeta, so Rust cannot call it; these constants close the loop from the other
/// side. The Noeta test pins *what the builder emits*, and the two live tests below (one per backend)
/// pin that *both drivers accept it verbatim*, bound parameters and all — which together is the
/// portability claim. Changing the builder's output without changing these makes the Noeta test fail;
/// changing these without the builder makes them describe nothing, which is why they name it.
#[cfg(test)]
const BUILDER_INSERT_OR_IGNORE: &str =
    "INSERT INTO users (id, name) VALUES (?, ?) ON CONFLICT DO NOTHING";

/// The `upsert(["id", "name"], …, ["id"])` form. See [`BUILDER_INSERT_OR_IGNORE`].
#[cfg(test)]
const BUILDER_UPSERT: &str = "INSERT INTO users (id, name) VALUES (?, ?)      ON CONFLICT (id) DO UPDATE SET name = excluded.name";

/// Run the builder's two conflict statements against `driver` and assert they behave: the second
/// insert of an existing key is skipped (0 rows, no error), and the upsert overwrites it. Shared by
/// the SQLite and Postgres e2e modules so both backends are held to the identical script.
#[cfg(test)]
fn assert_builder_conflict_statements(driver: &mut dyn SqlDriver) {
    driver
        .execute_batch("CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL)")
        .unwrap();

    let ada = [SqlValue::Int(1), SqlValue::Text("Ada".to_string())];
    assert_eq!(driver.execute(BUILDER_INSERT_OR_IGNORE, &ada).unwrap(), 1);
    // The same key again: skipped, not an error — what makes a seed re-runnable.
    let grace = [SqlValue::Int(1), SqlValue::Text("Grace".to_string())];
    assert_eq!(driver.execute(BUILDER_INSERT_OR_IGNORE, &grace).unwrap(), 0);
    assert_eq!(
        driver
            .query("SELECT name FROM users WHERE id = 1", &[])
            .unwrap()[0][0]
            .1,
        SqlValue::Text("Ada".to_string())
    );

    // The upsert overwrites the row it collided with.
    let lovelace = [SqlValue::Int(1), SqlValue::Text("Ada Lovelace".to_string())];
    assert_eq!(driver.execute(BUILDER_UPSERT, &lovelace).unwrap(), 1);
    assert_eq!(
        driver
            .query("SELECT name FROM users WHERE id = 1", &[])
            .unwrap()[0][0]
            .1,
        SqlValue::Text("Ada Lovelace".to_string())
    );
    // And inserts one it does not.
    let radia = [SqlValue::Int(2), SqlValue::Text("Radia".to_string())];
    assert_eq!(driver.execute(BUILDER_UPSERT, &radia).unwrap(), 1);
    assert_eq!(driver.query("SELECT id FROM users", &[]).unwrap().len(), 2);
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

    #[test]
    fn the_query_builders_conflict_statements_run_on_sqlite() {
        assert_builder_conflict_statements(&mut mem());
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
        let ran = seed(&mut driver, &ordered, &mut UnsupportedPrograms).unwrap();
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
        seed(&mut driver, &plain, &mut UnsupportedPrograms).unwrap();
        seed(&mut driver, &plain, &mut UnsupportedPrograms).unwrap_err(); // second run trips the PK — proves it re-ran

        // The documented idempotent idiom makes a re-run a no-op.
        let idempotent = vec![Migration::new(
            "0001_idem.sql",
            "INSERT OR IGNORE INTO posts (id, title) VALUES (3, 'once');",
        )];
        seed(&mut driver, &idempotent, &mut UnsupportedPrograms).unwrap();
        let after_first = count(&mut driver, "posts");
        seed(&mut driver, &idempotent, &mut UnsupportedPrograms).unwrap();
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
        let err = seed(&mut driver, &seeds, &mut UnsupportedPrograms).unwrap_err();
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
        let err =
            seed_only(&mut driver, &migrations, &seeds, &mut UnsupportedPrograms).unwrap_err();
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
        // `add_bool` lowers to a `BOOLEAN` column, and the driver reads a declared boolean back as a
        // boolean — SQLite stores it as 0/1, but that is storage, not the schema's meaning.
        assert_eq!(rows[0][1].1, SqlValue::Bool(false));
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
        let ran = seed_only(&mut driver, &migrations, &seeds, &mut UnsupportedPrograms).unwrap();
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
        let ran = seed_only(&mut driver, &migrations, &seeds, &mut UnsupportedPrograms).unwrap();
        assert_eq!(ran, vec!["0001_widgets.sql"]);
        assert_eq!(
            driver
                .query("SELECT COUNT(*) AS n FROM widgets", &[])
                .unwrap()[0][0]
                .1,
            SqlValue::Int(3)
        );
        // Re-running the idempotent seed is a no-op (ON CONFLICT DO NOTHING), and it was never tracked.
        seed(&mut driver, &seeds, &mut UnsupportedPrograms).unwrap();
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

    /// The twin of `sqlite_e2e::the_query_builders_conflict_statements_run_on_sqlite`, against a
    /// live server: the statements the query builder emits for a re-runnable seed are accepted
    /// **verbatim** by PostgreSQL too, `?` placeholders rewritten to `$N` and all. This is what makes
    /// a builder-written seed portable — and what the SQLite-only `INSERT OR IGNORE` spelling fails
    /// here with (`syntax error at or near "OR"`).
    #[test]
    fn the_query_builders_conflict_statements_run_on_postgres_too() {
        let _pg = crate::pg_test_guard();
        let Ok(dsn) = std::env::var("NOETA_PG_TEST_DSN") else {
            return; // no server configured — skip
        };
        let mut driver = PostgresDriver::connect(&dsn).expect("connect to NOETA_PG_TEST_DSN");
        driver.reset().expect("reset");

        assert_builder_conflict_statements(&mut driver);

        // The unportable spelling, proven unportable rather than assumed: this is exactly why the
        // builder never emits it.
        let err = driver
            .execute(
                "INSERT OR IGNORE INTO users (id, name) VALUES (?, ?)",
                &[SqlValue::Int(3), SqlValue::Text("Grace".to_string())],
            )
            .unwrap_err();
        assert!(err.contains("syntax error"), "{err}");

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
