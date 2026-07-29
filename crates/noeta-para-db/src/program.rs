//! The **`.noe` seed entry convention** — the native half of the program body language
//! ([`crate::migrate::MigrationKind::Program`]).
//!
//! A program seed is an ordinary Noeta program that declares one function:
//!
//! ```text
//! use para.db
//! use para.db.query.{table, exec}
//!
//! fn seed(conn: db.Connection): void {
//!     exec(conn, table("users").insert_or_ignore(["id", "name"], [1, "Ada"]))
//! }
//! ```
//!
//! `noeta migrate --seed` loads and checks that file through `CommandCtx::run_file` and appends one
//! synthesized trailing statement — `db.run_seed("<dsn>", seed)` — exactly the way `noeta serve`
//! appends `http.serve(port, fetch)`. [`RUN_SEED_FN`] is that call: an ordinary registered function,
//! so the mechanism behind the command is the same one a program can call directly.
//!
//! **Why an entry call and not an ambient connection.** The dsn is resolved *once*, by the command
//! (`--db` → `DATABASE_URL` → `[db] url`), and reaches the program as a literal argument. Nothing is
//! smuggled through the environment, the seed file names no connection string of its own, and
//! `noeta migrate --db … --seed` therefore seeds the database the flag names.
//!
//! **No implicit transaction.** `run_seed` opens the connection, hands it over, and releases it. A
//! seed program may open its own transaction (`conn.execute("BEGIN", [])`, or a repository's
//! `flush`), which an outer transaction opened here would collide with — SQLite rejects a nested
//! `BEGIN` outright. Per-statement idempotency (`insert_or_ignore`/`upsert`) is what makes a seed
//! re-runnable, and that needs no transaction at all.

use std::sync::{Arc, Mutex};

use noeta_ext_abi::registry::{ExtFn, NativeOut, RetTy, SigType};
use noeta_ext_abi::{
    CtxError, CtxOut, ExternBox, NativeCtx, NativeValue, Slot, ctx_arity, no_function_error,
    type_error,
};

use crate::conn::{CONNECTION_TYPE_NAME, ConnectionBox, io_error, open_driver};

/// The module the synthesized seed entry call names (`db.run_seed(…)`), **qualified**: the driver
/// calls its last segment and binds that segment with a synthetic `use para.db`, so a `.noe` seed
/// resolves the entry call whether or not it imports the module itself. (A bare, single-segment
/// name binds nothing and leaves resolution to the seed's own imports — see `EntryCall::module`.)
pub const SEED_ENTRY_MODULE: &str = "para.db";

/// The function the synthesized seed entry call names.
pub const SEED_ENTRY_FUNC: &str = "run_seed";

/// The top-level function a `.noe` seed must declare — the argument the entry call passes by name.
/// A seed without it fails to check, with the ordinary "unknown name" diagnostic against the seed
/// file, exactly as if the author had written the call themselves.
pub const SEED_ENTRY_IDENT: &str = "seed";

/// `db.run_seed(dsn, seed)` — open `dsn`, run `seed(conn)`, release the connection. A **ctx**
/// (higher-order) function: it takes a callable and re-enters the backend to invoke it.
pub const RUN_SEED_FN: ExtFn = ExtFn {
    name: SEED_ENTRY_FUNC,
    params: &[
        SigType::String,
        SigType::Fn(&[SigType::Named(CONNECTION_TYPE_NAME)], &SigType::Unit),
    ],
    ret: RetTy::Concrete(SigType::Unit),
    ..ExtFn::DEFAULTS
};

/// The `db.run_seed` ctx dispatch (paired with [`RUN_SEED_FN`]).
pub fn run_seed_ctx_dispatch(
    func: &str,
    ctx: &mut dyn NativeCtx,
    args: &[Slot],
) -> Result<CtxOut, CtxError> {
    if func != SEED_ENTRY_FUNC {
        return Err(no_function_error(SEED_ENTRY_MODULE, func).into());
    }
    ctx_arity(func, args, 2)?;
    let dsn = match ctx.view(args[0])? {
        NativeValue::Str(dsn) => dsn,
        _ => return Err(type_error(func, "string").into()),
    };
    let driver = open_driver(&dsn).map_err(io_error)?;
    let conn = ConnectionBox(Arc::new(Mutex::new(driver)));
    let slot = ctx.intern(NativeOut::Extern(ExternBox::new(conn)))?;
    // Run the seed body, then release the handle whether it succeeded or not — the driver closes
    // when the last reference to it drops, so a failed seed leaves no connection behind.
    let outcome = ctx.call(args[1], &[slot]);
    ctx.free(slot);
    let result = outcome?;
    ctx.free(result);
    Ok(CtxOut::Out(NativeOut::Unit))
}
