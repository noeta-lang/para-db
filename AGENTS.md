# AGENTS.md

Guidance for coding agents working in this repo — the standalone repo of the **para/db** Noeta package (the first-party database layer: native swappable driver + pure-Noeta query/repo/@sql/reactive layers), extracted from the noeta monorepo. Toolchain issues (the language, the `noeta` binary, `std.*`, the extension ABI) belong in the monorepo at github.com/noeta-lang/noeta, not here.

## Repo layout

- `noeta.toml` — the package manifest (`name = "para/db"`, `native = "native"` declares the Rust extension entry crate).
- `query.noe` / `schema.noe` / `repo.noe` / `sql.noe` / `reactive.noe` — the pure-Noeta layers over the native driver. `schema.noe` builds the portable schema DSL's neutral `Statement` IR — the Noeta twin of `schema.rs`'s, field for field — and renders it two ways: `.render()` lays it out as migration-file source, `.canonical()` reproduces the native canonical text byte for byte. The grammar, the validation and the lowering are native, so the builder and a `.schema` migration file share one implementation, and `schema.rs`'s `the_noeta_builder_and_the_ir_render_one_canonical_text` is what holds the two IRs to that one text. **That test needs a `noeta` binary**: without one it skips itself (so the `rust` CI job stays self-contained), and CI's `examples` job runs it with `NOETA_CROSS_CHECK=1`, which turns a missing binary into a failure. Changing `schema.noe` recomposes the toolchain the first time it runs, so expect one slow run after every edit.
- `crates/noeta-para-db/` — the impl crate: the `db` module + `Connection` extern type, the `SqlDriver` seam (SQLite behind default-on `ring-sqlite`, PostgreSQL behind opt-in `ring-postgres`), the portable schema DSL (`schema.rs`: parser, neutral IR, per-`Dialect` lowering — wired in at `SqlDriver::lower_schema`, the same seam as the `?`→`$N` rewrite), the migration/seed engine, and the `noeta migrate` contributed command. `program.rs` holds the `.noe` seed entry convention (`db.run_seed(dsn, seed)`); the engine reaches it through the injected `migrate::ProgramRunner`, since only the CLI can run a program. `migrate::DirKind` is the gate that keeps `.noe` a **seeds-only** body language — a `.noe` file in the migrations directory is a hard error, never a silent skip.
- `native/` — the thin entry crate the manifest's `native` key points at; re-exports `NOETA_EXTENSIONS`.
- `editors/` — the `@sql` TextMate injection grammar.
- `examples/para-db-demo/` — one standalone package, many entry demos, with its committed `noeta.lock`.
- `.github/workflows/` — CI (`ci.yml`) and the tag-triggered registry publish (`release.yml`).

## Build & test

- `cargo test` inside `crates/noeta-para-db` works standalone — the toolchain crates are git dependencies on `https://github.com/noeta-lang/noeta` (rev-pinned; flips to tag pins once a toolchain release tag exists). `native/` builds the same way. Postgres-path code compiles under `--features ring-postgres`.
- Running the examples needs the `noeta` binary and **composes a toolchain** (the native crate is compiled in). Set:
  - nothing, in the common case: the compose `[patch]` key defaults to the binary's baked repository URL (`https://github.com/noeta-lang/noeta`), which now equals the URL the crates' Cargo.toml declares. When overriding to a fork or local clone, `NOETA_TOOLCHAIN_REPO` MUST equal the declared URL, or the composed build links two copies of the extension ABI and every impl fails with a two-`Extension`-traits E0308;
  - optionally `NOETA_TOOLCHAIN_SRC=<path to a noeta checkout>` to skip the git fetch.
- Then `noeta check` / `noeta test` each demo in `examples/para-db-demo/`. The `postgres_demo` / `live_repo_demo` / `watch_demo` entries need a reachable PostgreSQL; the rest are SQLite/in-memory and hermetic.

## Conventions

- `noeta.lock` files under `examples/` **are committed** — leave resolved locks in place.
- Rust: default `rustfmt` style (no `rustfmt.toml`), `cargo clippy --all-targets -- -D warnings` clean, zero compiler warnings; the CI toolchain is pinned at 1.97.0 — lint against it locally (a floating `@stable` surfaces lints CI doesn't have yet, and vice versa). `clippy::needless_update` is allowed in `crates/noeta-para-db/Cargo.toml`: the ABI's documented `..ExtFn::DEFAULTS` additive-evolution convention trips it whenever `ExtFn` happens to have no optional fields.
- Rust naming: `snake_case` files/functions, `PascalCase` types, `SCREAMING_SNAKE_CASE` constants; prefer enums and constants over magic strings.
- Markdown never hard-wraps lines.
- **American English** throughout — code, comments, and docs (`behavior`, not `behaviour`).
- **Conventional commits** for all commit titles. Commit each green slice as it completes, but **never `git push` without explicit authorization**. Never move a published `v*` tag — a release is a new tag.
- Implement in full — no stubs or TODOs; new functionality lands with tests.
- Keep `README.md` and this file up to date when layout or behavior changes.

## CI

`ci.yml` gates the Rust crates (fmt/clippy/test) and the examples (pinned released `noeta`); `release.yml` re-runs the crate gate then publishes the tag to the hosted registry (`noeta publish`, keyless Sigstore provenance via GitHub OIDC). Both go green only once the toolchain repo is published under github.com/noeta-lang/noeta and the `file:///` deps are flipped.
