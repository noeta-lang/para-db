//! The **swappable driver seam** (aether DB0): a backend-agnostic SQL surface every concrete
//! database driver implements. SQLite is the first impl ([`crate::sqlite`]); Postgres/MySQL arrive
//! as further [`SqlDriver`] impls with **no change** to the Noeta surface or the extern type — the
//! `db.connect` dsn scheme is the only place a new driver is wired in.

/// A backend-agnostic scalar crossing the driver boundary — the value kinds SQL columns and bound
/// parameters take, each one a Noeta value kind on the other side of the ABI (`int`, `float`,
/// `string`, `bool`, `bytes`, `none`). Kept deliberately small; a richer driver maps its own types
/// onto these. Decimal/date land in a later slice with the columnar surface.
///
/// **Every driver owes this surface an honest value kind**, not the one its storage happens to use:
/// SQLite has no boolean storage class and stores one as an integer, so its driver reads the column's
/// *declared* type and hands a [`SqlValue::Bool`] back (see [`crate::sqlite::ColumnIntent`]). A driver
/// that leaked its storage class here would make every consumer above it — the repository's typed
/// row→model mapping most of all — re-guess what the schema meant.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    /// Binary column data (SQLite BLOB, Postgres `bytea`), verbatim — a Noeta `bytes` value. NOT
    /// decoded as text: binary that is not valid UTF-8 would be silently corrupted by the
    /// replacement character, and a driver never guesses that a blob was really a string.
    Bytes(Vec<u8>),
    Null,
}

/// One result row: `(column name, value)` pairs in the query's column order. A `Map<string, dyn>`
/// on the Noeta side — the simplest row surface (struct mapping arrives in DB2).
pub type Row = Vec<(String, SqlValue)>;

/// The swappable database driver. `Send` so a [`crate::conn::ConnectionBox`]'s `Arc<Mutex<Box<dyn
/// SqlDriver>>>` may cross the executor. **Transactions are ordinary statements** —
/// `execute("BEGIN")` / `execute("COMMIT")` / `execute("ROLLBACK")` — deliberately NOT a borrowed
/// `rusqlite::Transaction` handle (which would borrow the `Connection` and so could never live
/// inside an extern box); the unit-of-work flush (DB2) drives them by name.
pub trait SqlDriver: Send {
    /// Run a non-query statement (`INSERT`/`UPDATE`/`DELETE`/DDL/`BEGIN`/`COMMIT`), returning the
    /// number of rows affected. `Err(message)` on a driver/SQL error.
    fn execute(&mut self, sql: &str, params: &[SqlValue]) -> Result<i64, String>;

    /// Run a query, returning every row as column-name → value pairs. `Err(message)` on error.
    fn query(&mut self, sql: &str, params: &[SqlValue]) -> Result<Vec<Row>, String>;

    /// Execute a **multi-statement** SQL script with no bind parameters — the whole `sql` string, which
    /// may contain several `;`-separated statements, run in order. [`SqlDriver::execute`] runs a
    /// *single* statement (rusqlite/Postgres both prepare one); this is the companion the migration
    /// runner ([`crate::migrate`]) needs to apply a migration file's verbatim body and to issue
    /// `BEGIN`/`COMMIT`/`ROLLBACK`. The body is passed through **unrewritten** (no `?`→`$N`
    /// translation), so a migration is written in the target dialect's native SQL. A driver that
    /// cannot run a script leaves the default error.
    fn execute_batch(&mut self, sql: &str) -> Result<(), String> {
        let _ = sql;
        Err("this driver does not support multi-statement batch execution".to_string())
    }

    /// Lower a **portable schema-DSL** statement list ([`crate::schema`]) into this backend's DDL —
    /// the schema peer of the `?`→`$N` placeholder rewrite, and for the same reason: a dialect
    /// difference is absorbed *here*, at the one seam a backend is wired in, so the migration engine
    /// and the Noeta layers above it never branch on the driver. The returned script is a
    /// `;`-terminated batch for [`SqlDriver::execute_batch`]. A driver that has no dialect mapping
    /// leaves the default error, and only raw-SQL migrations work against it.
    fn lower_schema(&self, statements: &[crate::schema::Statement]) -> Result<String, String> {
        let _ = statements;
        Err(
            "this driver does not lower the portable schema DSL — write the migration as raw SQL"
                .to_string(),
        )
    }

    /// **Destructively** reset the database to an empty schema — drop every object this connection
    /// owns. The dialect-specific wipe lives in each driver (SQLite drops every user table/view/
    /// trigger; Postgres `DROP SCHEMA public CASCADE; CREATE SCHEMA public`), so the migration runner
    /// stays backend-agnostic. Backs `noeta migrate --reset`; a driver that cannot safely wipe itself
    /// leaves the default error.
    fn reset(&mut self) -> Result<(), String> {
        Err("this driver does not support a schema reset".to_string())
    }

    /// **Change notifications** (reactive DB↔UI, aether DB5) — subscribe this connection to a
    /// notification `channel` (Postgres `LISTEN`). A driver without a push channel leaves the default
    /// (unsupported), so the reactive layer degrades to in-process invalidation only.
    fn listen(&mut self, channel: &str) -> Result<(), String> {
        let _ = channel;
        Err("this driver does not support change notifications (LISTEN/NOTIFY)".to_string())
    }

    /// Poll pending change notifications **non-blocking**, returning the channels that fired since the
    /// last poll (empty if none). Pumped from the app's loop — never blocks it. Default: none.
    fn notifications(&mut self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }

    /// Fire a change notification on `channel` (Postgres `NOTIFY`) — the write-side companion to
    /// [`SqlDriver::listen`], so a mutation can wake watchers on the same channel. A driver without a
    /// push channel leaves the default (a no-op), so the reactive layer stays in-process only.
    fn notify(&mut self, channel: &str) -> Result<(), String> {
        let _ = channel;
        Ok(())
    }
}
