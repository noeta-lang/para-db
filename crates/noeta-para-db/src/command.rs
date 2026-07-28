//! `noeta migrate` — the para/db package's **extension-contributed CLI command** (para-extraction):
//! apply a project's plain-SQL database migrations. A thin command over the one migration engine in
//! this crate: it resolves the connection string and migrations directory, opens the driver the dsn
//! scheme selects, and drives `migrate::{apply, status, pending, reset}`. `migrate new` needs no
//! database — it only scaffolds a file.
//!
//! Registered through [`crate::ParaDbExtension`]'s `commands()` (higher-order-abi H6), so the verb
//! travels with the package: a consumer whose manifest depends on `para/db` and trusts its commands
//! (`[trust] commands = ["para/db"]`) gets `noeta migrate` from the composed toolchain — nothing
//! db-specific lives in the core CLI. Configuration reaches the command through the narrow
//! [`CommandCtx::manifest_str`] seam (`[db] url/migrations/seeds` in the nearest `noeta.toml`).
//!
//! Exit codes follow the CLI convention: `0` success; `2` for a usage/config problem (no dsn
//! configured, a missing migrations directory, `--reset` without confirmation); `1` for a failure
//! that ran but did not complete (connect failure, a SQL error, checksum drift, a deleted migration).

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use noeta_ext_abi::{ArgKind, ArgSpec, CommandCtx, EntryArg, EntryCall, ExtCommand, ParsedArgs};

use crate::conn::open_driver;
use crate::migrate::{
    self, DirKind, MigrateError, PROGRAM_EXTENSION, ProgramFailure, ProgramRunner,
    SCAFFOLD_TEMPLATE, SCHEMA_EXTENSION, SCHEMA_SCAFFOLD_TEMPLATE, SEED_PROGRAM_SCAFFOLD_TEMPLATE,
    SQL_EXTENSION,
};
use crate::program::{SEED_ENTRY_FUNC, SEED_ENTRY_IDENT, SEED_ENTRY_MODULE};

/// The default migrations directory when none is configured or passed.
const DEFAULT_DIR: &str = "migrations";

/// The default seeds directory when none is configured or passed.
const DEFAULT_SEEDS_DIR: &str = "seeds";

/// The environment variable consulted for the connection string (after `--db`, before `[db] url`).
const DATABASE_URL_ENV: &str = "DATABASE_URL";

/// The `noeta migrate` subcommand, declared over the extension-command ABI. The optional
/// positional words carry the sub-actions the core clap enum used to model as subcommands:
/// `noeta migrate new <name>` and `noeta migrate seed`.
pub const MIGRATE_COMMAND: ExtCommand = ExtCommand {
    name: "migrate",
    about: "Apply the project's SQL migrations (para/db); `new <name>` scaffolds, `seed` seeds only",
    args: &[
        ArgSpec {
            name: "action",
            help: "`new` — scaffold the next migration (or seed, with --seed) file; `seed` — run \
                   the seed files only. When omitted, the flags select apply / status / dry-run / \
                   reset (optionally with --seed)",
            kind: ArgKind::Word,
        },
        ArgSpec {
            name: "name",
            help: "For `new`: a short description, slugified into the filename (e.g. \"add users \
                   table\")",
            kind: ArgKind::Word,
        },
        ArgSpec {
            name: "db",
            help: "The database connection string (overrides `DATABASE_URL` and `[db] url`)",
            kind: ArgKind::OptStr,
        },
        ArgSpec {
            name: "dir",
            help: "The migrations directory (overrides `[db] migrations`; default `migrations`)",
            kind: ArgKind::OptPath,
        },
        ArgSpec {
            name: "seeds-dir",
            help: "The seeds directory (overrides `[db] seeds`; default `seeds`)",
            kind: ArgKind::OptPath,
        },
        ArgSpec {
            name: "status",
            help: "Show which migrations are applied and which are pending, then exit",
            kind: ArgKind::Bool,
        },
        ArgSpec {
            name: "dry-run",
            help: "List the migrations that would be applied without touching the database",
            kind: ArgKind::Bool,
        },
        ArgSpec {
            name: "reset",
            help: "DESTRUCTIVE: drop the whole schema and re-apply every migration from zero. \
                   Requires `--yes` (or an interactive confirmation)",
            kind: ArgKind::Bool,
        },
        ArgSpec {
            name: "seed",
            help: "After applying migrations (or after `--reset`), run the project's seed files — \
                   re-runnable development data under the seeds directory",
            kind: ArgKind::Bool,
        },
        ArgSpec {
            name: "schema",
            help: "For `new`: scaffold a portable schema-DSL migration (`<name>.schema`, lowered \
                   per driver) instead of a raw `<name>.sql` one",
            kind: ArgKind::Bool,
        },
        ArgSpec {
            name: "program",
            help: "For `new --seed`: scaffold a Noeta program seed (`<name>.noe`, run through the \
                   portable query builder) instead of a raw `<name>.sql` one",
            kind: ArgKind::Bool,
        },
        ArgSpec {
            name: "yes",
            help: "Skip the interactive confirmation for `--reset` (for scripts/CI)",
            kind: ArgKind::Bool,
        },
    ],
    run: migrate_run,
};

/// The command body: parse the declared args into an [`Invocation`], then [`execute`] against the
/// real process streams, environment, and terminal.
fn migrate_run(ctx: &mut dyn CommandCtx, args: &ParsedArgs) -> u8 {
    let mut out = io::stdout();
    let mut err = io::stderr();
    let inv = match Invocation::from_parsed(args) {
        Ok(inv) => inv,
        Err(message) => return usage_error(&mut err, &message),
    };
    let env_dsn = std::env::var(DATABASE_URL_ENV).ok();
    execute(&inv, ctx, env_dsn, &mut out, &mut err, &mut TtyPrompt)
}

/// A `migrate new` scaffold request: a name, an optional target directory, whether it is a seed, and
/// which body language it is written in (raw SQL by default; the portable schema DSL for a migration
/// with `--schema`, a Noeta program for a seed with `--program`).
struct NewArgs {
    name: String,
    dir: Option<PathBuf>,
    seed: bool,
    schema: bool,
    program: bool,
}

/// The parsed `noeta migrate` invocation.
struct Invocation {
    /// `Some(..)` for `migrate new <name>`; `None` for the apply/status/reset flags.
    new: Option<NewArgs>,
    /// `migrate seed` — run seeds only (against an up-to-date schema).
    seed_only: bool,
    db: Option<String>,
    dir: Option<PathBuf>,
    seeds_dir: Option<PathBuf>,
    status: bool,
    dry_run: bool,
    reset: bool,
    /// `--seed`: after applying migrations (or `--reset`), also run the seed files.
    seed: bool,
    yes: bool,
}

impl Invocation {
    /// Interpret the flat declared args as the migrate grammar: the optional `action` word selects
    /// `new`/`seed`, and the `name` word belongs to `new` alone. A combination the grammar does not
    /// admit is a usage error (exit 2), like clap's own subcommand validation used to be.
    fn from_parsed(args: &ParsedArgs) -> Result<Invocation, String> {
        let action = args.get_str("action");
        let name = args.get_str("name");
        let dir = args.get_path("dir").map(Path::to_path_buf);
        let seed = args.get_bool("seed").unwrap_or(false);
        let schema = args.get_bool("schema").unwrap_or(false);
        let program = args.get_bool("program").unwrap_or(false);
        let (new, seed_only) = match action {
            Some("new") => {
                let name = name.ok_or_else(|| {
                    "`migrate new` needs a name: `noeta migrate new <name>`".to_string()
                })?;
                if schema && seed {
                    // A seed is data, not schema — the DSL has no vocabulary for rows.
                    return Err(
                        "`--schema` and `--seed` are mutually exclusive: a seed is re-runnable \
                         data, which the schema DSL does not describe"
                            .to_string(),
                    );
                }
                if program && schema {
                    return Err(
                        "`--program` and `--schema` are mutually exclusive: they are two different \
                         body languages"
                            .to_string(),
                    );
                }
                if program && !seed {
                    // The scope fence, at the scaffold: `.noe` is a seed body language, so there is
                    // no such thing as a program migration to scaffold.
                    return Err("`--program` scaffolds a SEED body, so it needs `--seed`: \
                         `noeta migrate new --seed --program <name>`. A migration is `.sql` or \
                         `.schema` — a program is not a migration body language"
                        .to_string());
                }
                (
                    Some(NewArgs {
                        name: name.to_string(),
                        dir: dir.clone(),
                        seed,
                        schema,
                        program,
                    }),
                    false,
                )
            }
            Some("seed") => {
                if let Some(name) = name {
                    return Err(format!("unexpected argument `{name}` after `migrate seed`"));
                }
                (None, true)
            }
            Some(other) => {
                return Err(format!(
                    "unknown action `{other}` (expected `new <name>` or `seed`)"
                ));
            }
            None => (None, false),
        };
        Ok(Invocation {
            new,
            seed_only,
            db: args.get_str("db").map(str::to_string),
            dir,
            seeds_dir: args.get_path("seeds-dir").map(Path::to_path_buf),
            status: args.get_bool("status").unwrap_or(false),
            dry_run: args.get_bool("dry-run").unwrap_or(false),
            reset: args.get_bool("reset").unwrap_or(false),
            seed,
            yes: args.get_bool("yes").unwrap_or(false),
        })
    }
}

/// The interactive `--reset` confirmation seam: the real prompt asks on the process terminal;
/// tests substitute a scripted one. Split from [`execute`] so the destructive-path logic is
/// testable without a TTY.
trait ResetPrompt {
    /// Whether an interactive terminal is attached (no terminal + no `--yes` = refuse).
    fn is_terminal(&self) -> bool;
    /// Ask the user to confirm dropping `dsn`'s data; returns whether they typed `yes`.
    fn confirm(&mut self, dsn: &str) -> bool;
}

/// The real prompt: `stdin`'s terminal, answer read from it.
struct TtyPrompt;

impl ResetPrompt for TtyPrompt {
    fn is_terminal(&self) -> bool {
        io::stdin().is_terminal()
    }
    fn confirm(&mut self, dsn: &str) -> bool {
        print!(
            "This will DROP ALL DATA in `{dsn}` and re-apply from zero. Type 'yes' to continue: "
        );
        let _ = io::stdout().flush();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() {
            return false;
        }
        line.trim().eq_ignore_ascii_case("yes")
    }
}

/// Run a parsed `noeta migrate` invocation. All effects are injected — the driving [`CommandCtx`]
/// (manifest access), the `DATABASE_URL` value, the output/error streams, and the reset prompt —
/// so the crate's tests exercise the complete command against an in-memory project without a
/// process boundary. Returns the process exit code.
fn execute(
    inv: &Invocation,
    ctx: &mut dyn CommandCtx,
    env_dsn: Option<String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    prompt: &mut dyn ResetPrompt,
) -> u8 {
    // `migrate new` is database-free: scaffold a file and return.
    if let Some(new) = &inv.new {
        return match scaffold_new(
            &*ctx,
            &new.name,
            new.dir.as_deref(),
            new.seed,
            new.schema,
            new.program,
        ) {
            Ok(path) => {
                let _ = writeln!(out, "Created {}", path.display());
                0
            }
            Err(message) => usage_error(err, &message),
        };
    }

    let dir = resolve_dir(&*ctx, inv.dir.as_deref());
    let dsn = match resolve_dsn(&*ctx, inv.db.as_deref(), env_dsn) {
        Ok(dsn) => dsn,
        Err(message) => return usage_error(err, &message),
    };

    // Discover + checksum the migration files (a missing directory is a usage error).
    let migrations = match migrate::load_dir(&dir, DirKind::Migrations) {
        Ok(migrations) => migrations,
        Err(e) => return usage_error(err, &e.to_string()),
    };

    let mut driver = match open_driver(&dsn) {
        Ok(driver) => driver,
        Err(e) => return run_error(err, &format!("cannot open database: {e}")),
    };
    let driver = driver.as_mut();

    // `migrate seed` — run seeds only, refusing if any migration is still pending.
    if inv.seed_only {
        let seeds = match load_seeds(&*ctx, inv.seeds_dir.as_deref(), err) {
            Ok(seeds) => seeds,
            Err(exit) => return exit,
        };
        let mut programs = CtxPrograms { ctx, dsn: &dsn };
        return match migrate::seed_only(driver, &migrations, &seeds, &mut programs) {
            Ok(ran) => report_seeds(out, &ran),
            Err(e) => run_error(err, &e.to_string()),
        };
    }

    if inv.status {
        return match migrate::status(driver, &migrations) {
            Ok(rows) => {
                print_status(out, &dir, &rows);
                0
            }
            Err(e) => run_error(err, &e.to_string()),
        };
    }

    if inv.dry_run {
        return match migrate::pending(driver, &migrations) {
            Ok(names) => {
                print_pending(out, &names);
                0
            }
            Err(e) => run_error(err, &e.to_string()),
        };
    }

    if inv.reset {
        if !inv.yes {
            // Either there is no terminal to ask on (a usage error telling them to pass `--yes`)
            // or the user answers at the prompt — declining is a clean cancel.
            if !prompt.is_terminal() {
                return usage_error(
                    err,
                    "`--reset` needs confirmation: pass `--yes` (no interactive terminal)",
                );
            }
            if !prompt.confirm(&dsn) {
                let _ = writeln!(out, "Aborted; database unchanged.");
                return 0;
            }
        }
        return match migrate::reset(driver, &migrations) {
            Ok(applied) => {
                let _ = writeln!(
                    out,
                    "Reset: dropped the schema and re-applied {} migration(s).",
                    applied.len()
                );
                for name in &applied {
                    let _ = writeln!(out, "  applied {name}");
                }
                // `--reset --seed`: the full dev loop — reseed the freshly rebuilt schema.
                if inv.seed {
                    run_seeds(ctx, driver, &dsn, inv.seeds_dir.as_deref(), out, err)
                } else {
                    0
                }
            }
            Err(e) => run_error(err, &e.to_string()),
        };
    }

    // Default: apply every pending migration, then seed if `--seed`.
    match migrate::apply(driver, &migrations) {
        Ok(applied) if applied.is_empty() => {
            let _ = writeln!(
                out,
                "Already up to date ({} migration(s)).",
                migrations.len()
            );
            if inv.seed {
                run_seeds(ctx, driver, &dsn, inv.seeds_dir.as_deref(), out, err)
            } else {
                0
            }
        }
        Ok(applied) => {
            let _ = writeln!(
                out,
                "Applied {} migration(s) from {}:",
                applied.len(),
                dir.display()
            );
            for name in &applied {
                let _ = writeln!(out, "  applied {name}");
            }
            if inv.seed {
                run_seeds(ctx, driver, &dsn, inv.seeds_dir.as_deref(), out, err)
            } else {
                0
            }
        }
        Err(e) => run_error(err, &e.to_string()),
    }
}

/// The command's [`ProgramRunner`]: a `.noe` seed is loaded, checked and run **on the real host**
/// through the driving [`CommandCtx`], with `db.run_seed("<dsn>", seed)` appended as its trailing
/// statement — the same synthesized-entry mechanism `noeta serve` uses for `http.serve(port, fetch)`.
///
/// The dsn is the one the command already resolved (`--db` → `DATABASE_URL` → `[db] url`), passed as
/// a literal argument: the seed program never names a connection string of its own, and nothing is
/// smuggled to it through the environment.
struct CtxPrograms<'a> {
    ctx: &'a mut dyn CommandCtx,
    dsn: &'a str,
}

impl ProgramRunner for CtxPrograms<'_> {
    fn run_program(&mut self, path: &Path) -> Result<(), ProgramFailure> {
        if is_private_memory_dsn(self.dsn) {
            return Err(ProgramFailure::Unsupported(format!(
                "a `.noe` seed opens its own connection, but `{}` names a private in-memory \
                 database that exists only inside the connection that created it — the seed would \
                 fill a second, empty one while this database stayed untouched. Point `--db` / \
                 `DATABASE_URL` / `[db] url` at a SQLite file (`sqlite:app.db`) or at a server.",
                self.dsn
            )));
        }
        let entry = EntryCall {
            module: SEED_ENTRY_MODULE,
            func: SEED_ENTRY_FUNC,
            args: vec![
                EntryArg::Str(self.dsn.to_string()),
                EntryArg::Ident(SEED_ENTRY_IDENT),
            ],
        };
        // The program's own diagnostics reach stderr before this returns; the exit code only says
        // which kind of failure it was — 2 for a file the driver could not read, 1 for a program
        // that failed to check or to run.
        match self.ctx.run_file(path, Some(&entry), None) {
            0 => Ok(()),
            2 => Err(ProgramFailure::Failed(
                "the seed program could not be read".to_string(),
            )),
            _ => Err(ProgramFailure::Failed(
                "the seed program reported an error (see above)".to_string(),
            )),
        }
    }
}

/// Whether `dsn` names a **private in-memory** SQLite database — one that lives inside the single
/// connection that opened it. A seed program connects for itself, so against such a dsn it would
/// populate a second, empty database; the runner refuses it with an explanation rather than
/// appearing to succeed.
fn is_private_memory_dsn(dsn: &str) -> bool {
    matches!(dsn, ":memory:" | "sqlite::memory:" | "sqlite:")
}

/// Load the seed files for `--seed` / `--reset --seed`, then run them, mapping the outcome to an
/// exit code. A missing seeds directory when `--seed` was explicitly requested is a usage error
/// (nothing to seed from); an empty one is a clean no-op.
fn run_seeds(
    ctx: &mut dyn CommandCtx,
    driver: &mut dyn crate::driver::SqlDriver,
    dsn: &str,
    flag: Option<&Path>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let seeds = match load_seeds(&*ctx, flag, err) {
        Ok(seeds) => seeds,
        Err(exit) => return exit,
    };
    let mut programs = CtxPrograms { ctx, dsn };
    match migrate::seed(driver, &seeds, &mut programs) {
        Ok(ran) => report_seeds(out, &ran),
        Err(e) => run_error(err, &e.to_string()),
    }
}

/// Discover the seed files under the resolved seeds directory. Returns the reported exit code (a
/// usage error naming the missing directory) on failure, so both `run_seeds` and the seeds-only
/// path share it.
fn load_seeds(
    ctx: &dyn CommandCtx,
    flag: Option<&Path>,
    err: &mut dyn Write,
) -> Result<Vec<migrate::Migration>, u8> {
    let dir = resolve_seeds_dir(ctx, flag);
    migrate::load_dir(&dir, DirKind::Seeds).map_err(|e| usage_error(err, &e.to_string()))
}

/// Print the seed-run summary (an empty run is an explicit no-op line).
fn report_seeds(out: &mut dyn Write, ran: &[String]) -> u8 {
    if ran.is_empty() {
        let _ = writeln!(out, "No seed files to run.");
    } else {
        let _ = writeln!(out, "Ran {} seed file(s):", ran.len());
        for name in ran {
            let _ = writeln!(out, "  seeded {name}");
        }
    }
    0
}

/// Scaffold a new migration or seed file (creating the directory), returning the new path. A
/// migration goes under the migrations directory (raw SQL by default, the portable schema DSL with
/// `--schema`); a seed goes under the seeds directory with the idempotent-idiom template. All use the
/// same UTC-timestamp-prefixed, slugified filename — only the extension and the starter body differ,
/// so the ordering model is identical whichever body language is chosen.
///
/// **Raw SQL stays the default.** A `.schema` migration cannot express everything a `.sql` one can,
/// and every existing project's muscle memory is `migrate new <name>` → a SQL file; making the DSL
/// opt-in keeps that unchanged and keeps the scaffold honest about which one is the general tool.
fn scaffold_new(
    ctx: &dyn CommandCtx,
    name: &str,
    dir: Option<&Path>,
    seed: bool,
    schema: bool,
    program: bool,
) -> Result<PathBuf, String> {
    let (dir, template, label, extension) = if seed && program {
        (
            resolve_seeds_dir(ctx, dir),
            SEED_PROGRAM_SCAFFOLD_TEMPLATE,
            "seeds",
            PROGRAM_EXTENSION,
        )
    } else if seed {
        (
            resolve_seeds_dir(ctx, dir),
            migrate::SEED_SCAFFOLD_TEMPLATE,
            "seeds",
            SQL_EXTENSION,
        )
    } else if schema {
        (
            resolve_dir(ctx, dir),
            SCHEMA_SCAFFOLD_TEMPLATE,
            "migrations",
            SCHEMA_EXTENSION,
        )
    } else {
        (
            resolve_dir(ctx, dir),
            SCAFFOLD_TEMPLATE,
            "migrations",
            SQL_EXTENSION,
        )
    };
    let filename = migrate::scaffold_filename(&utc_timestamp(), name, extension)
        .map_err(|e: MigrateError| e.to_string())?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create {label} directory `{}`: {e}", dir.display()))?;
    let path = dir.join(filename);
    if path.exists() {
        return Err(format!("`{}` already exists", path.display()));
    }
    std::fs::write(&path, template)
        .map_err(|e| format!("cannot write `{}`: {e}", path.display()))?;
    Ok(path)
}

/// Resolve the migrations directory: the `--dir` flag, else `[db] migrations`, else `migrations/`.
fn resolve_dir(ctx: &dyn CommandCtx, flag: Option<&Path>) -> PathBuf {
    flag.map(Path::to_path_buf)
        .or_else(|| ctx.manifest_str("db", "migrations").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR))
}

/// Resolve the seeds directory: the `--seeds-dir` flag, else `[db] seeds`, else `seeds/`.
fn resolve_seeds_dir(ctx: &dyn CommandCtx, flag: Option<&Path>) -> PathBuf {
    flag.map(Path::to_path_buf)
        .or_else(|| ctx.manifest_str("db", "seeds").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SEEDS_DIR))
}

/// Resolve the connection string, highest priority first: the `--db` flag, `DATABASE_URL`
/// (threaded in as `env_dsn`), then the `[db] url` in the nearest `noeta.toml` (via the driver's
/// [`CommandCtx::manifest_str`]). Absent everywhere is a usage error.
fn resolve_dsn(
    ctx: &dyn CommandCtx,
    flag: Option<&str>,
    env_dsn: Option<String>,
) -> Result<String, String> {
    if let Some(dsn) = flag {
        return Ok(dsn.to_string());
    }
    if let Some(dsn) = env_dsn
        && !dsn.is_empty()
    {
        return Ok(dsn);
    }
    if let Some(url) = ctx.manifest_str("db", "url") {
        return Ok(url);
    }
    Err(format!(
        "no database configured: pass `--db <dsn>`, set `{DATABASE_URL_ENV}`, or add a `[db]` \
         table with `url = \"…\"` to noeta.toml"
    ))
}

/// Format a UTC `YYYYMMDDHHMMSS` timestamp for a new migration's filename prefix.
fn utc_timestamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}{:02}{:02}{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// Print the applied/pending status table.
fn print_status(out: &mut dyn Write, dir: &Path, rows: &[migrate::StatusRow]) {
    if rows.is_empty() {
        let _ = writeln!(out, "No migrations under {}.", dir.display());
        return;
    }
    let _ = writeln!(out, "Migrations under {}:", dir.display());
    for row in rows {
        if row.applied {
            let at = row.applied_at.as_deref().unwrap_or("");
            let _ = writeln!(out, "  [applied] {}  ({at})", row.name);
        } else {
            let _ = writeln!(out, "  [pending] {}", row.name);
        }
    }
    let pending = rows.iter().filter(|r| !r.applied).count();
    let _ = writeln!(out, "{} applied, {pending} pending.", rows.len() - pending);
}

/// Print the dry-run pending list.
fn print_pending(out: &mut dyn Write, names: &[String]) {
    if names.is_empty() {
        let _ = writeln!(out, "No pending migrations.");
        return;
    }
    let _ = writeln!(out, "Would apply {} migration(s):", names.len());
    for name in names {
        let _ = writeln!(out, "  {name}");
    }
}

/// Report a usage/config problem (exit code 2), the CLI convention for bad input.
fn usage_error(err: &mut dyn Write, message: &str) -> u8 {
    let _ = writeln!(err, "noeta migrate: {message}");
    2
}

/// Report a run that started but failed (exit code 1): a connection failure or a migration error.
fn run_error(err: &mut dyn Write, message: &str) -> u8 {
    let _ = writeln!(err, "noeta migrate: {message}");
    1
}

/// The old CLI-level `noeta migrate` e2e coverage, ported to the crate the command now lives in:
/// the same grammar/output/exit-code assertions, driven through the command's own parse + execute
/// seams against a real SQLite file (hence the `ring-sqlite` gate, default-on — the same gate the
/// engine's driver-backed tests ride). The compose-dispatch path itself is proven once by the CLI's
/// `fx-info` end-to-end test; per-package CI covers migrate's own e2e.
#[cfg(all(test, feature = "ring-sqlite"))]
mod tests {
    use super::*;

    /// A bare driving ctx whose manifest is an in-memory `[db]` table — `run_file` is unreachable
    /// because `migrate` never runs a program.
    struct TestCtx {
        manifest: Vec<(&'static str, String)>,
        /// Every program `run_file` was asked to run, with the entry call the command synthesized
        /// for it — the whole `.noe` seed mechanism, observable without a CLI.
        programs: Vec<ProgramRun>,
        /// The exit code this fake driver reports for a program (0 = it ran and succeeded).
        program_exit: u8,
    }

    /// One recorded `run_file`: which file, and the trailing entry call rendered as source.
    #[derive(Debug, PartialEq, Eq)]
    struct ProgramRun {
        path: PathBuf,
        entry: String,
    }

    impl TestCtx {
        fn bare() -> TestCtx {
            TestCtx {
                manifest: Vec::new(),
                programs: Vec::new(),
                program_exit: 0,
            }
        }

        fn with_manifest(manifest: Vec<(&'static str, String)>) -> TestCtx {
            TestCtx {
                manifest,
                ..TestCtx::bare()
            }
        }
    }

    impl CommandCtx for TestCtx {
        fn run_file(
            &mut self,
            file: &Path,
            entry: Option<&noeta_ext_abi::EntryCall>,
            _banner: Option<&str>,
        ) -> u8 {
            let entry = entry
                .map(|call| {
                    let args: Vec<String> = call
                        .args
                        .iter()
                        .map(|arg| match arg {
                            noeta_ext_abi::EntryArg::Str(value) => format!("\"{value}\""),
                            noeta_ext_abi::EntryArg::Int(value) => value.to_string(),
                            noeta_ext_abi::EntryArg::Ident(name) => (*name).to_string(),
                        })
                        .collect();
                    format!("{}.{}({})", call.module, call.func, args.join(", "))
                })
                .unwrap_or_default();
            self.programs.push(ProgramRun {
                path: file.to_path_buf(),
                entry,
            });
            self.program_exit
        }
        fn manifest_str(&self, table: &str, key: &str) -> Option<String> {
            if table != "db" {
                return None;
            }
            self.manifest
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.clone())
        }
    }

    /// A scripted reset prompt (a non-TTY by default, like the test processes the old CLI e2e
    /// tests spawned).
    struct ScriptedPrompt {
        terminal: bool,
        answer: bool,
    }

    impl ResetPrompt for ScriptedPrompt {
        fn is_terminal(&self) -> bool {
            self.terminal
        }
        fn confirm(&mut self, _dsn: &str) -> bool {
            self.answer
        }
    }

    /// A fresh private project directory seeded with `(relative path, contents)` files.
    fn temp_dir(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("noeta_para_db_command_tests")
            .join(format!("{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (rel, contents) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, contents).unwrap();
        }
        dir
    }

    /// A project directory with the given migrations under `migrations/`.
    fn project(name: &str, migrations: &[(&str, &str)]) -> PathBuf {
        let files: Vec<(String, &str)> = migrations
            .iter()
            .map(|(f, sql)| (format!("migrations/{f}"), *sql))
            .collect();
        let refs: Vec<(&str, &str)> = files.iter().map(|(f, sql)| (f.as_str(), *sql)).collect();
        temp_dir(name, &refs)
    }

    /// A project with migrations under `migrations/` and seeds under `seeds/`.
    fn project_with_seeds(
        name: &str,
        migrations: &[(&str, &str)],
        seeds: &[(&str, &str)],
    ) -> PathBuf {
        let mut files: Vec<(String, &str)> = migrations
            .iter()
            .map(|(f, sql)| (format!("migrations/{f}"), *sql))
            .collect();
        files.extend(seeds.iter().map(|(f, sql)| (format!("seeds/{f}"), *sql)));
        let refs: Vec<(&str, &str)> = files.iter().map(|(f, sql)| (f.as_str(), *sql)).collect();
        temp_dir(name, &refs)
    }

    /// The outcome of one driven invocation: exit code, stdout text, stderr text.
    struct Outcome {
        code: u8,
        out: String,
        err: String,
    }

    /// Drive the command exactly as the CLI would — through [`Invocation::from_parsed`] over a
    /// [`ParsedArgs`] built from an argv-shaped word list — against captured streams and a
    /// scripted (non-TTY) prompt. `dir` is the project directory the migrations/seeds paths are
    /// anchored to (the CLI anchors them to the cwd; the test anchors them explicitly so parallel
    /// tests never chdir).
    fn run_in(dir: &Path, words: &[&str], ctx: &mut TestCtx, env_dsn: Option<&str>) -> Outcome {
        run_full(
            dir,
            words,
            ctx,
            env_dsn,
            &mut ScriptedPrompt {
                terminal: false,
                answer: false,
            },
        )
    }

    fn run_full(
        dir: &Path,
        words: &[&str],
        ctx: &mut TestCtx,
        env_dsn: Option<&str>,
        prompt: &mut dyn ResetPrompt,
    ) -> Outcome {
        // Build ParsedArgs the way the CLI's dispatch does from clap matches: positionals in
        // declaration order, then flags.
        let mut parsed = ParsedArgs::default();
        let mut positionals = 0usize;
        let mut iter = words.iter().peekable();
        let mut anchored_dir = false;
        let mut anchored_seeds = false;
        while let Some(word) = iter.next() {
            match *word {
                "--db" => parsed.push_str("db", iter.next().unwrap().to_string()),
                "--dir" => {
                    parsed.push_path("dir", dir.join(iter.next().unwrap()));
                    anchored_dir = true;
                }
                "--seeds-dir" => {
                    parsed.push_path("seeds-dir", dir.join(iter.next().unwrap()));
                    anchored_seeds = true;
                }
                "--status" => parsed.push_bool("status", true),
                "--dry-run" => parsed.push_bool("dry-run", true),
                "--reset" => parsed.push_bool("reset", true),
                "--seed" => parsed.push_bool("seed", true),
                "--schema" => parsed.push_bool("schema", true),
                "--program" => parsed.push_bool("program", true),
                "--yes" => parsed.push_bool("yes", true),
                positional => {
                    match positionals {
                        0 => parsed.push_str("action", positional.to_string()),
                        1 => parsed.push_str("name", positional.to_string()),
                        _ => panic!("too many positional words in the test argv"),
                    }
                    positionals += 1;
                }
            }
        }
        // Anchor the default directories to the project dir (the CLI resolves them against the
        // cwd) unless the invocation already passed explicit ones. `migrate new` takes its target
        // from `--dir` alone (a seed scaffold's `--dir` names the SEEDS directory), so the `new`
        // tests pass it explicitly instead of riding this anchor.
        let is_new = words.first() == Some(&"new");
        if !is_new {
            if !anchored_dir && ctx.manifest_str("db", "migrations").is_none() {
                parsed.push_path("dir", dir.join(DEFAULT_DIR));
            }
            if !anchored_seeds && ctx.manifest_str("db", "seeds").is_none() {
                parsed.push_path("seeds-dir", dir.join(DEFAULT_SEEDS_DIR));
            }
        }
        let inv = match Invocation::from_parsed(&parsed) {
            Ok(inv) => inv,
            Err(message) => {
                let mut err = Vec::new();
                let code = usage_error(&mut err, &message);
                return Outcome {
                    code,
                    out: String::new(),
                    err: String::from_utf8(err).unwrap(),
                };
            }
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = execute(
            &inv,
            ctx,
            env_dsn.map(str::to_string),
            &mut out,
            &mut err,
            prompt,
        );
        Outcome {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    /// The dsn for a SQLite file inside the project dir (absolute, so no test ever chdirs).
    fn dsn(dir: &Path) -> String {
        format!("sqlite:{}", dir.join("app.db").display())
    }

    const M1: (&str, &str) = (
        "0001_users.sql",
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);",
    );
    const M2: (&str, &str) = (
        "0002_seed.sql",
        "INSERT INTO users (id, name) VALUES (1, 'Ada');\n\
         INSERT INTO users (id, name) VALUES (2, 'Bob');",
    );

    /// A seed inserting Ada, written idempotently so a re-run is a no-op (the documented idiom).
    const SEED_IDEMPOTENT: (&str, &str) = (
        "0001_users.sql",
        "INSERT OR IGNORE INTO users (id, name) VALUES (10, 'Ada');",
    );

    #[test]
    fn apply_then_rerun_is_idempotent() {
        let dir = project("apply", &[M1, M2]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);

        let first = run_in(&dir, &["--db", &db], &mut ctx, None);
        assert_eq!(first.code, 0, "{}", first.err);
        assert!(
            first.out.contains("Applied 2 migration(s)"),
            "{}",
            first.out
        );
        assert!(
            first.out.contains("applied 0001_users.sql"),
            "{}",
            first.out
        );

        // Re-running applies nothing.
        let second = run_in(&dir, &["--db", &db], &mut ctx, None);
        assert_eq!(second.code, 0, "{}", second.err);
        assert!(second.out.contains("Already up to date"), "{}", second.out);
    }

    #[test]
    fn status_reports_applied_and_pending() {
        let dir = project("status", &[M1, M2]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);

        // Before applying: both pending.
        let before = run_in(&dir, &["--db", &db, "--status"], &mut ctx, None);
        assert_eq!(before.code, 0, "{}", before.err);
        assert!(
            before.out.contains("0 applied, 2 pending"),
            "{}",
            before.out
        );

        assert_eq!(run_in(&dir, &["--db", &db], &mut ctx, None).code, 0);

        let after = run_in(&dir, &["--db", &db, "--status"], &mut ctx, None);
        assert_eq!(after.code, 0, "{}", after.err);
        assert!(after.out.contains("2 applied, 0 pending"), "{}", after.out);
    }

    #[test]
    fn dry_run_lists_without_applying() {
        let dir = project("dryrun", &[M1]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);

        let dry = run_in(&dir, &["--db", &db, "--dry-run"], &mut ctx, None);
        assert_eq!(dry.code, 0, "{}", dry.err);
        assert!(
            dry.out.contains("Would apply 1 migration(s)"),
            "{}",
            dry.out
        );

        // The dry-run did not apply anything: a real status still shows it pending.
        let status = run_in(&dir, &["--db", &db, "--status"], &mut ctx, None);
        assert!(
            status.out.contains("0 applied, 1 pending"),
            "{}",
            status.out
        );
    }

    #[test]
    fn new_scaffolds_a_timestamped_file() {
        let dir = temp_dir("new", &[]);
        let mut ctx = TestCtx::bare();

        let outcome = run_in(
            &dir,
            &["new", "add posts table", "--dir", "migrations"],
            &mut ctx,
            None,
        );
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(outcome.out.contains("Created"), "{}", outcome.out);
        assert!(
            outcome.out.contains("_add_posts_table.sql"),
            "{}",
            outcome.out
        );

        // Exactly one .sql file landed under migrations/.
        let created: Vec<_> = std::fs::read_dir(dir.join("migrations"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
            .collect();
        assert_eq!(created.len(), 1);
    }

    #[test]
    fn new_schema_scaffolds_a_portable_dsl_migration() {
        let dir = temp_dir("new_schema", &[]);
        let mut ctx = TestCtx::bare();

        let outcome = run_in(
            &dir,
            &["new", "create todos", "--schema", "--dir", "migrations"],
            &mut ctx,
            None,
        );
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(
            outcome.out.contains("_create_todos.schema"),
            "{}",
            outcome.out
        );

        // Exactly one .schema file landed, carrying the DSL starter body.
        let created: Vec<_> = std::fs::read_dir(dir.join("migrations"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "schema"))
            .collect();
        assert_eq!(created.len(), 1);
        let body = std::fs::read_to_string(created[0].path()).unwrap();
        assert!(body.contains("create_table(\"todos\")"), "{body}");
        assert!(body.contains("raw `.sql` migration"), "{body}");
    }

    #[test]
    fn new_schema_and_seed_together_are_a_usage_error() {
        let dir = temp_dir("new_schema_seed", &[]);
        let outcome = run_in(
            &dir,
            &["new", "demo", "--schema", "--seed"],
            &mut TestCtx::bare(),
            None,
        );
        assert_eq!(outcome.code, 2);
        assert!(
            outcome.err.contains("mutually exclusive"),
            "{}",
            outcome.err
        );
    }

    /// The end-to-end portability claim through the CLI: one `.schema` migration and one raw `.sql`
    /// migration in one directory, applied in filename order against a real SQLite file, then
    /// re-run as a no-op — the DSL is tracked, checksummed, and ordered exactly like raw SQL.
    #[test]
    fn a_schema_migration_applies_beside_raw_sql_through_the_cli() {
        let dir = project(
            "mixed",
            &[
                (
                    "0001_todos.schema",
                    "create_table(\"todos\")\n    .id()\n    .text(\"title\").not_null()\n    \
                     .bool(\"done\").default(false)\n",
                ),
                (
                    "0002_first.sql",
                    "INSERT INTO todos (title) VALUES ('write a migration');",
                ),
            ],
        );
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);

        let first = run_in(&dir, &["--db", &db], &mut ctx, None);
        assert_eq!(first.code, 0, "{}", first.err);
        assert!(
            first.out.contains("applied 0001_todos.schema"),
            "{}",
            first.out
        );
        assert!(
            first.out.contains("applied 0002_first.sql"),
            "{}",
            first.out
        );

        let again = run_in(&dir, &["--db", &db], &mut ctx, None);
        assert!(again.out.contains("Already up to date"), "{}", again.out);
    }

    #[test]
    fn a_malformed_schema_migration_is_reported_with_its_line() {
        let dir = project(
            "bad_schema",
            &[(
                "0001_bad.schema",
                "create_table(\"t\")\n    .frobnicate()\n",
            )],
        );
        let db = dsn(&dir);
        let outcome = run_in(&dir, &["--db", &db], &mut TestCtx::bare(), None);
        assert_eq!(outcome.code, 1, "{}", outcome.err);
        assert!(
            outcome.err.contains("not valid portable schema DSL"),
            "{}",
            outcome.err
        );
        assert!(outcome.err.contains("line 2"), "{}", outcome.err);
    }

    #[test]
    fn new_without_a_name_is_a_usage_error() {
        let dir = temp_dir("new_no_name", &[]);
        let outcome = run_in(&dir, &["new"], &mut TestCtx::bare(), None);
        assert_eq!(outcome.code, 2);
        assert!(outcome.err.contains("needs a name"), "{}", outcome.err);
    }

    #[test]
    fn an_unknown_action_word_is_a_usage_error() {
        let dir = temp_dir("bad_action", &[]);
        let outcome = run_in(&dir, &["frobnicate"], &mut TestCtx::bare(), None);
        assert_eq!(outcome.code, 2);
        assert!(outcome.err.contains("unknown action"), "{}", outcome.err);
    }

    #[test]
    fn reset_reapplies_with_yes() {
        let dir = project("reset", &[M1, M2]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);
        assert_eq!(run_in(&dir, &["--db", &db], &mut ctx, None).code, 0);

        let outcome = run_in(&dir, &["--db", &db, "--reset", "--yes"], &mut ctx, None);
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(
            outcome.out.contains("dropped the schema and re-applied 2"),
            "{}",
            outcome.out
        );
    }

    #[test]
    fn reset_without_yes_is_refused_in_a_non_tty() {
        let dir = project("reset_refuse", &[M1]);
        let db = dsn(&dir);
        let outcome = run_in(&dir, &["--db", &db, "--reset"], &mut TestCtx::bare(), None);
        assert_eq!(outcome.code, 2);
        assert!(
            outcome.err.contains("needs confirmation"),
            "{}",
            outcome.err
        );
    }

    #[test]
    fn reset_declined_at_the_prompt_is_a_clean_cancel() {
        let dir = project("reset_decline", &[M1]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);
        assert_eq!(run_in(&dir, &["--db", &db], &mut ctx, None).code, 0);

        let outcome = run_full(
            &dir,
            &["--db", &db, "--reset"],
            &mut ctx,
            None,
            &mut ScriptedPrompt {
                terminal: true,
                answer: false,
            },
        );
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(
            outcome.out.contains("Aborted; database unchanged."),
            "{}",
            outcome.out
        );
    }

    #[test]
    fn a_failing_migration_stops_and_keeps_the_prior() {
        let dir = project(
            "fail",
            &[
                ("0001_ok.sql", "CREATE TABLE ok (id INTEGER);"),
                (
                    "0002_bad.sql",
                    "CREATE TABLE bad (id INTEGER); NONSENSE SQL;",
                ),
            ],
        );
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);

        let outcome = run_in(&dir, &["--db", &db], &mut ctx, None);
        assert_eq!(outcome.code, 1, "{}", outcome.err);
        assert!(outcome.err.contains("0002_bad.sql"), "{}", outcome.err);
        assert!(outcome.err.contains("rolled back"), "{}", outcome.err);

        // The first migration committed; the failed one is still pending.
        let status = run_in(&dir, &["--db", &db, "--status"], &mut ctx, None);
        assert!(
            status.out.contains("1 applied, 1 pending"),
            "{}",
            status.out
        );
    }

    #[test]
    fn editing_an_applied_migration_is_rejected() {
        let dir = project("drift", &[("0001_a.sql", "CREATE TABLE a (id INTEGER);")]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);
        assert_eq!(run_in(&dir, &["--db", &db], &mut ctx, None).code, 0);

        // Edit the already-applied file, then re-run.
        std::fs::write(
            dir.join("migrations/0001_a.sql"),
            "CREATE TABLE a (id INTEGER, extra TEXT);",
        )
        .unwrap();
        let outcome = run_in(&dir, &["--db", &db], &mut ctx, None);
        assert_eq!(outcome.code, 1);
        assert!(
            outcome.err.contains("was edited after it was applied"),
            "{}",
            outcome.err
        );
    }

    #[test]
    fn no_dsn_configured_is_a_usage_error() {
        let dir = project("no_dsn", &[M1]);
        let outcome = run_in(&dir, &["--status"], &mut TestCtx::bare(), None);
        assert_eq!(outcome.code, 2);
        assert!(
            outcome.err.contains("no database configured"),
            "{}",
            outcome.err
        );
    }

    #[test]
    fn dsn_is_read_from_the_database_url_env() {
        let dir = project("env_dsn", &[M1]);
        let db = format!("sqlite:{}", dir.join("env.db").display());
        // The env layer is threaded in exactly where `migrate_run` reads `DATABASE_URL`.
        let outcome = run_in(&dir, &[], &mut TestCtx::bare(), Some(&db));
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(
            outcome.out.contains("Applied 1 migration(s)"),
            "{}",
            outcome.out
        );
    }

    #[test]
    fn an_empty_database_url_env_is_ignored() {
        let dir = project("empty_env_dsn", &[M1]);
        let outcome = run_in(&dir, &["--status"], &mut TestCtx::bare(), Some(""));
        assert_eq!(outcome.code, 2);
        assert!(
            outcome.err.contains("no database configured"),
            "{}",
            outcome.err
        );
    }

    #[test]
    fn db_flag_wins_over_env_and_manifest() {
        let ctx = TestCtx::with_manifest(vec![("url", "sqlite:manifest.db".to_string())]);
        assert_eq!(
            resolve_dsn(&ctx, Some("sqlite::memory:"), Some("sqlite:env.db".into())).unwrap(),
            "sqlite::memory:"
        );
        // And env wins over the manifest.
        assert_eq!(
            resolve_dsn(&ctx, None, Some("sqlite:env.db".into())).unwrap(),
            "sqlite:env.db"
        );
    }

    #[test]
    fn migrate_seed_flag_applies_then_seeds() {
        let dir = project_with_seeds("seed_flag", &[M1], &[SEED_IDEMPOTENT]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);

        let outcome = run_in(&dir, &["--db", &db, "--seed"], &mut ctx, None);
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(
            outcome.out.contains("Applied 1 migration(s)"),
            "{}",
            outcome.out
        );
        assert!(
            outcome.out.contains("Ran 1 seed file(s)"),
            "{}",
            outcome.out
        );
        assert!(
            outcome.out.contains("seeded 0001_users.sql"),
            "{}",
            outcome.out
        );
    }

    #[test]
    fn migrate_seed_subcommand_errors_when_a_migration_is_pending() {
        let dir = project_with_seeds("seed_pending", &[M1], &[SEED_IDEMPOTENT]);
        let db = dsn(&dir);

        // Nothing migrated yet: seeding a stale schema is refused with guidance.
        let outcome = run_in(&dir, &["--db", &db, "seed"], &mut TestCtx::bare(), None);
        assert_eq!(outcome.code, 1, "{}", outcome.err);
        assert!(
            outcome.err.contains("migration(s) are still pending"),
            "{}",
            outcome.err
        );
        assert!(outcome.err.contains("--seed"), "{}", outcome.err);
    }

    #[test]
    fn migrate_seed_subcommand_runs_when_current_and_is_rerunnable() {
        let dir = project_with_seeds("seed_current", &[M1], &[SEED_IDEMPOTENT]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);
        assert_eq!(run_in(&dir, &["--db", &db], &mut ctx, None).code, 0);

        // Seeds-only against the up-to-date schema, twice — the idempotent idiom keeps it a no-op.
        for _ in 0..2 {
            let outcome = run_in(&dir, &["--db", &db, "seed"], &mut ctx, None);
            assert_eq!(outcome.code, 0, "{}", outcome.err);
            assert!(
                outcome.out.contains("Ran 1 seed file(s)"),
                "{}",
                outcome.out
            );
        }
    }

    #[test]
    fn reset_seed_is_the_full_dev_loop() {
        let dir = project_with_seeds("reset_seed", &[M1], &[SEED_IDEMPOTENT]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);
        assert_eq!(run_in(&dir, &["--db", &db], &mut ctx, None).code, 0);

        let outcome = run_in(
            &dir,
            &["--db", &db, "--reset", "--seed", "--yes"],
            &mut ctx,
            None,
        );
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(
            outcome.out.contains("dropped the schema and re-applied 1"),
            "{}",
            outcome.out
        );
        assert!(
            outcome.out.contains("Ran 1 seed file(s)"),
            "{}",
            outcome.out
        );
    }

    #[test]
    fn new_seed_scaffolds_under_the_seeds_directory() {
        let dir = temp_dir("new_seed", &[]);
        let mut ctx = TestCtx::bare();

        // For a seed scaffold, `--dir` names the SEEDS directory (same as the old subcommand).
        let outcome = run_in(
            &dir,
            &["new", "demo users", "--seed", "--dir", "seeds"],
            &mut ctx,
            None,
        );
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(outcome.out.contains("Created"), "{}", outcome.out);
        assert!(outcome.out.contains("_demo_users.sql"), "{}", outcome.out);

        // The file landed under seeds/, not migrations/, with the idempotent-idiom template.
        let created: Vec<_> = std::fs::read_dir(dir.join("seeds"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
            .collect();
        assert_eq!(created.len(), 1);
        let body = std::fs::read_to_string(created[0].path()).unwrap();
        // The idiom it teaches is the portable one, and it says plainly that the SQLite-only
        // spelling is not.
        assert!(body.contains("ON CONFLICT DO NOTHING"), "{body}");
        assert!(body.contains("SQLite-only"), "{body}");
        assert!(!dir.join("migrations").exists());
    }

    #[test]
    fn seeds_dir_override_flag_is_honored() {
        let dir = temp_dir(
            "seeds_dir_flag",
            &[
                ("migrations/0001_users.sql", M1.1),
                ("data/0001_users.sql", SEED_IDEMPOTENT.1),
            ],
        );
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);

        let outcome = run_in(
            &dir,
            &["--db", &db, "--seed", "--seeds-dir", "data"],
            &mut ctx,
            None,
        );
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(
            outcome.out.contains("Ran 1 seed file(s)"),
            "{}",
            outcome.out
        );
    }

    #[test]
    fn seeds_dir_is_read_from_the_manifest_db_table() {
        let dir = temp_dir(
            "seeds_manifest",
            &[
                ("migrations/0001_users.sql", M1.1),
                ("fixtures/0001_users.sql", SEED_IDEMPOTENT.1),
            ],
        );
        // The manifest's `[db]` table names the dirs and the url (the test ctx plays the role the
        // CLI's `manifest_str` driver does over the nearest noeta.toml).
        let mut ctx = TestCtx::with_manifest(vec![
            ("url", dsn(&dir)),
            ("migrations", dir.join("migrations").display().to_string()),
            ("seeds", dir.join("fixtures").display().to_string()),
        ]);

        let outcome = run_in(&dir, &["--seed"], &mut ctx, None);
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(
            outcome.out.contains("Ran 1 seed file(s)"),
            "{}",
            outcome.out
        );
    }

    #[test]
    fn dsn_is_read_from_the_manifest_db_table() {
        let dir = temp_dir(
            "manifest_dsn",
            &[("migrations/0001_a.sql", "CREATE TABLE a (id INTEGER);")],
        );
        let mut ctx = TestCtx::with_manifest(vec![
            (
                "url",
                format!("sqlite:{}", dir.join("manifest.db").display()),
            ),
            ("migrations", dir.join("migrations").display().to_string()),
        ]);
        let outcome = run_in(&dir, &[], &mut ctx, None);
        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(
            outcome.out.contains("Applied 1 migration(s)"),
            "{}",
            outcome.out
        );
    }

    #[test]
    fn utc_timestamp_is_fourteen_digits() {
        let ts = utc_timestamp();
        assert_eq!(ts.len(), 14, "{ts}");
        assert!(ts.chars().all(|c| c.is_ascii_digit()), "{ts}");
    }

    #[test]
    fn default_directories_resolve_without_flags_or_manifest() {
        let ctx = TestCtx::bare();
        assert_eq!(resolve_dir(&ctx, None), PathBuf::from("migrations"));
        assert_eq!(resolve_seeds_dir(&ctx, None), PathBuf::from("seeds"));
    }

    // --- `.noe` seed bodies ---------------------------------------------------------------------

    /// A minimal program seed. Its body is never parsed here — the fake driver stands in for the
    /// CLI's loader — so what these tests pin down is the *mechanism*: which file is run, and with
    /// which entry call.
    const SEED_PROGRAM: (&str, &str) = (
        "0002_programmatic.noe",
        "use para.db\nfn seed(conn: db.Connection): void {}\n",
    );

    #[test]
    fn a_program_seed_is_run_through_the_driver_with_the_resolved_dsn_as_its_entry_call() {
        let dir = project_with_seeds("seed_program", &[M1], &[SEED_IDEMPOTENT, SEED_PROGRAM]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);

        let outcome = run_in(&dir, &["--db", &db, "--seed"], &mut ctx, None);

        assert_eq!(outcome.code, 0, "{}", outcome.err);
        // Both body languages report through one summary, in filename order.
        assert!(
            outcome.out.contains("Ran 2 seed file(s)"),
            "{}",
            outcome.out
        );
        assert!(
            outcome.out.contains("seeded 0002_programmatic.noe"),
            "{}",
            outcome.out
        );
        // The `.sql` seed went to the database; only the `.noe` one was run as a program.
        assert_eq!(
            ctx.programs,
            vec![ProgramRun {
                path: dir.join("seeds").join(SEED_PROGRAM.0),
                // The dsn the command resolved, passed as a literal — nothing is smuggled through
                // the environment — plus the program's own `seed` function by name.
                entry: format!("db.run_seed(\"{db}\", seed)"),
            }]
        );
    }

    #[test]
    fn migrate_seed_only_runs_program_seeds_too() {
        let dir = project_with_seeds("seed_program_only", &[M1], &[SEED_PROGRAM]);
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);
        assert_eq!(run_in(&dir, &["--db", &db], &mut ctx, None).code, 0);

        let outcome = run_in(&dir, &["--db", &db, "seed"], &mut ctx, None);

        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(
            outcome.out.contains("Ran 1 seed file(s)"),
            "{}",
            outcome.out
        );
        assert_eq!(ctx.programs.len(), 1);
    }

    #[test]
    fn a_program_seed_that_fails_stops_the_run_and_names_the_file() {
        let dir = project_with_seeds("seed_program_fail", &[M1], &[SEED_PROGRAM]);
        let mut ctx = TestCtx::bare();
        ctx.program_exit = 1; // the program loaded and reported an error
        let db = dsn(&dir);

        let outcome = run_in(&dir, &["--db", &db, "--seed"], &mut ctx, None);

        assert_eq!(outcome.code, 1, "{}", outcome.out);
        assert!(
            outcome.err.contains("seed program `0002_programmatic.noe`"),
            "{}",
            outcome.err
        );
    }

    #[test]
    fn a_program_seed_against_a_private_in_memory_dsn_is_refused_with_the_reason() {
        // Seeding an in-memory SQLite database from a separate program is impossible in principle:
        // the program's own connection creates a second, empty database. Saying so beats "Ran 1
        // seed file(s)" over a database that never changed.
        let dir = project_with_seeds("seed_program_memory", &[M1], &[SEED_PROGRAM]);
        let mut ctx = TestCtx::bare();

        let outcome = run_in(&dir, &["--db", "sqlite::memory:", "--seed"], &mut ctx, None);

        assert_eq!(outcome.code, 1, "{}", outcome.out);
        assert!(outcome.err.contains("in-memory"), "{}", outcome.err);
        assert!(outcome.err.contains("sqlite:app.db"), "{}", outcome.err);
        assert!(ctx.programs.is_empty(), "{:?}", ctx.programs);
    }

    #[test]
    fn a_program_body_in_the_migrations_directory_is_a_usage_error() {
        // The scope fence, from the command's side: `noeta migrate` refuses to run at all rather
        // than applying the `.sql` migrations and quietly ignoring the `.noe` file beside them.
        let dir = project(
            "noe_migration",
            &[M1, ("0002_b.noe", "fn seed(conn: db.Connection): void {}")],
        );
        let mut ctx = TestCtx::bare();
        let db = dsn(&dir);

        let outcome = run_in(&dir, &["--db", &db], &mut ctx, None);

        assert_eq!(outcome.code, 2, "{}", outcome.out);
        assert!(outcome.err.contains("0002_b.noe"), "{}", outcome.err);
        assert!(outcome.err.contains("seeds directory"), "{}", outcome.err);
        assert!(ctx.programs.is_empty());
    }

    #[test]
    fn new_seed_program_scaffolds_a_noe_seed_under_the_seeds_directory() {
        let dir = temp_dir("new_seed_program", &[]);
        let mut ctx = TestCtx::bare();

        let outcome = run_in(
            &dir,
            &["new", "demo users", "--seed", "--program", "--dir", "seeds"],
            &mut ctx,
            None,
        );

        assert_eq!(outcome.code, 0, "{}", outcome.err);
        assert!(outcome.out.contains("_demo_users.noe"), "{}", outcome.out);
        let created: Vec<_> = std::fs::read_dir(dir.join("seeds"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "noe"))
            .collect();
        assert_eq!(created.len(), 1);
        let body = std::fs::read_to_string(created[0].path()).unwrap();
        assert!(body.contains("fn seed(conn: db.Connection)"), "{body}");
        assert!(body.contains("insert_or_ignore"), "{body}");
    }

    #[test]
    fn new_program_without_seed_is_a_usage_error() {
        // There is no such thing as a program migration to scaffold — the fence again, at the one
        // place a user would first try to cross it.
        let dir = temp_dir("new_program_migration", &[]);
        let mut ctx = TestCtx::bare();

        let outcome = run_in(
            &dir,
            &["new", "add posts", "--program", "--dir", "migrations"],
            &mut ctx,
            None,
        );

        assert_eq!(outcome.code, 2, "{}", outcome.out);
        assert!(outcome.err.contains("--seed"), "{}", outcome.err);
    }
}
