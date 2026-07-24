# AGENTS.md

Guidance for coding agents working in this repo — the standalone repo of the **para/db** Noeta package (the first-party database layer: native swappable driver + pure-Noeta query/repo/@sql/reactive layers), extracted from the noeta monorepo. Toolchain issues (the language, the `noeta` binary, `std.*`, the extension ABI) belong in the monorepo at github.com/noeta-lang/noeta, not here.

## Repo layout

- `noeta.toml` — the package manifest (`name = "para/db"`, `native = "native"` declares the Rust extension entry crate).
- `query.noe` / `repo.noe` / `sql.noe` / `reactive.noe` — the pure-Noeta layers over the native driver.
- `crates/noeta-para-db/` — the impl crate: the `db` module + `Connection` extern type, the `SqlDriver` seam (SQLite behind default-on `ring-sqlite`, PostgreSQL behind opt-in `ring-postgres`), the migration/seed engine, and the `noeta migrate` contributed command.
- `native/` — the thin entry crate the manifest's `native` key points at; re-exports `NOETA_EXTENSIONS`.
- `editors/` — the `@sql` TextMate injection grammar.
- `examples/para-db-demo/` — one standalone package, many entry demos, with its committed `noeta.lock`.
- `.github/workflows/` — CI (`ci.yml`) and the tag-triggered registry publish (`release.yml`).

## Build & test

- `cargo test` inside `crates/noeta-para-db` works standalone — the toolchain crates are git dependencies (currently the pre-publish `file:///home/niklas/Code/lang` form; flips to `https://github.com/noeta-lang/noeta` at publish). `native/` builds the same way. Postgres-path code compiles under `--features ring-postgres`.
- Running the examples needs the `noeta` binary and **composes a toolchain** (the native crate is compiled in). Set:
  - `NOETA_TOOLCHAIN_REPO=file:///home/niklas/Code/lang` — MUST equal the URL the crates' Cargo.toml declares, or the composed build links two copies of the extension ABI and every impl fails with a two-`Extension`-traits E0308;
  - optionally `NOETA_TOOLCHAIN_SRC=<path to a noeta checkout>` to skip the git fetch.
- Then `noeta check` / `noeta test` each demo in `examples/para-db-demo/`. The `postgres_demo` / `live_repo_demo` / `watch_demo` entries need a reachable PostgreSQL; the rest are SQLite/in-memory and hermetic.

## Conventions

- Rust code is `cargo fmt` and `cargo clippy --all-targets -- -D warnings` clean (toolchain pinned at 1.97.0 in CI).
- `noeta.lock` files under `examples/` **are committed** — leave resolved locks in place.
- Markdown never hard-wraps lines; American English throughout.
- Conventional commits. Never move a published `v*` tag — a release is a new tag.

## CI

`ci.yml` gates the Rust crates (fmt/clippy/test) and the examples (pinned released `noeta`); `release.yml` re-runs the crate gate then publishes the tag to the hosted registry (`noeta publish`, keyless Sigstore provenance via GitHub OIDC). Both go green only once the toolchain repo is published under github.com/noeta-lang/noeta and the `file:///` deps are flipped.
