# AGENTS.md

Guidance for coding agents working in this repo — the standalone repo of the **para/db** Noeta package (native swappable driver + pure-Noeta query/schema/repo/`@sql`/reactive layers), extracted from the noeta monorepo. Toolchain issues (the language, the `noeta` binary, `std.*`, the extension ABI) belong in the monorepo at github.com/noeta-lang/noeta, not here.

## Versions & pins

- `noeta.toml` requires `toolchain = ">=0.6"`; the Rust crates take the two contract crates from crates.io by **range** — `noeta-ext-abi = "0.6"` and `noeta-reactive-abi = "0.6"`. A patch toolchain release is absorbed by the range, a minor is not. Don't hand-bump them: `toolchain-pin.yml` rewrites the ranges on a toolchain release and opens a PR, and deliberately leaves `toolchain = ">=X.Y"` and the package version to a human.
- CI installs the toolchain named by the **org variable** `NOETA_VERSION`, not by anything in this repo. The Rust compiler is pinned at 1.97.0 in `ci.yml`/`release.yml` — lint against that locally, since a floating `@stable` surfaces lints CI doesn't have yet and vice versa.
- Never move a published `v*` tag; a release is a new tag (`release.yml` gates with the full CI, then `noeta publish`es to the hosted registry).

## Build & test

- Each Rust crate is its own workspace root, so there is no top-level `cargo test` — run it per directory, as CI does: `for c in crates/*/ native/; do (cd "$c" && cargo test); done`. The Postgres driver is behind an opt-in ring; CI keeps it compiling with `cargo check --features ring-postgres` in `crates/noeta-para-db`.
- Running the examples needs the `noeta` binary and **composes a toolchain** (the native crate is cargo-built), so expect one slow run after a native change. Usually set nothing; `NOETA_TOOLCHAIN_SRC=<path to a noeta checkout>` skips the git fetch, and `NOETA_TOOLCHAIN_REPO` matters only when pointing composition at a fork or local clone.
- Then `noeta check` + `noeta test` each demo in `examples/para-db-demo/`. `postgres_demo` / `live_repo_demo` / `watch_demo` need a reachable PostgreSQL; the rest are SQLite/in-memory and hermetic.
- Two tests hold the Rust half and the Noeta half to one agreement by running a real `noeta` binary, and **skip themselves** when there is none (so `cargo test` stays self-contained): `schema::tests::the_noeta_builder_and_the_ir_render_one_canonical_text` and `command::tests::a_real_noe_migration_applies_through_the_real_command`. `NOETA_CROSS_CHECK=1` turns that skip into a failure. CI's examples job runs only the first by name — run the second locally after touching `migrations.noe`, `program.rs` or `command.rs`.

## Gotchas

- The schema DSL is implemented **twice** — `schema.noe`'s Noeta `Statement` IR and `schema.rs`'s Rust one, field for field, with `.canonical()` the byte-for-byte shared text (`.render()` is the migration-file source form). Change one side and you must change the other, then run the cross-check above.
- A `.noe` file is a legal body in **both** the migrations and the seeds directory and means a different thing in each: under `migrations/` it declares `migrate(): List<Statement>` (describes), under `seeds/` it declares `seed(conn)` (performs). `migrate::DirKind` is what selects the entry convention — it is never inferred from the file. `.sql`, `.schema` and `.noe` all load in both directories and interleave in one filename order.
- CI checks + tests the hermetic demos **by name**, so a new demo has to be added to the list in `ci.yml` or it is never run. `examples/para-db-namespaced/` is gated by nothing at all — check it by hand when you touch module naming.
- A demo's assertions belong in an `@test` block. CI runs `noeta check` + `noeta test` and **nothing else** — an `echo` that "shows" the behavior is not a gate, and a demo with no `@test` reports "no tests found" while passing.
- `clippy::needless_update` is allowed in `crates/noeta-para-db/Cargo.toml` on purpose: the ABI's documented `..ExtFn::DEFAULTS` additive-evolution convention trips it whenever `ExtFn` happens to have no optional fields. Keep the `..DEFAULTS`, not the lint.
- `noeta.lock` under `examples/` and every `Cargo.lock` are gitignored — an example is a demo, not a package root. This package, a library, carries no root lock either: it resolves at the consumer.

## Conventions

- Rust: default `rustfmt` style (no `rustfmt.toml`), `cargo clippy --all-targets -- -D warnings` clean, zero compiler warnings. `snake_case` files/functions, `PascalCase` types, `SCREAMING_SNAKE_CASE` constants; prefer enums and constants over magic strings.
- Markdown never hard-wraps lines. **American English** throughout — code, comments, and docs (`behavior`, not `behaviour`).
- **Conventional commits** for all commit titles. Commit each green slice as it completes, but **never `git push` without explicit authorization**.
- Implement in full — no stubs or TODOs; new functionality lands with tests. Keep `README.md` and this file up to date when layout or behavior changes.
