//! The SQLite [`SqlDriver`] (aether DB0) — the first concrete driver, wrapping a
//! [`rusqlite::Connection`]. Behind the `ring-sqlite` feature so a build that never touches
//! `para.db` links no SQLite. The [`SqlValue`] ↔ rusqlite mapping lives here and nowhere else.
//!
//! **Change notifications (DB5).** SQLite is a library, not a server — it has no `LISTEN`/`NOTIFY`.
//! But its per-connection **update hook** fires on every row change through a connection, and the
//! parallel server runs its worker isolates as *threads of one process* (separate heaps, shared
//! address space). So this driver bridges them with a **process-global notification bus** ([`BUS`]):
//! the update hook (and an explicit `notify`) publishes a channel to the bus, and every connection
//! `listen`ing on it — in this isolate or any sibling isolate — sees it on its next `notifications`
//! poll. Only channel-name **strings** cross the bus (no Noeta values, no heaps), so it is
//! `Send`-safe. This gives in-process and cross-*isolate* reactivity; it does NOT reach a separate
//! OS **process** writing the same file (nothing shares the static there) — that is SQLite's hard
//! limit, where Postgres's real LISTEN/NOTIFY is required instead.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};

use rusqlite::Connection;
use rusqlite::types::{Value, ValueRef};

use crate::driver::{Row, SqlDriver, SqlValue};

/// The process-global notification bus: every live [`SqliteDriver`]'s subscriber, weakly held so a
/// dropped connection unregisters itself. A `const`-constructible `Mutex` static (no init needed).
static BUS: Mutex<Vec<Weak<Subscriber>>> = Mutex::new(Vec::new());

/// One connection's bus membership: the channels it `listen`s on, and the channels that have fired on
/// them since the last `notifications` drain. Both `Send + Sync`, so a publish from any isolate thread
/// reaches a subscriber owned by another.
#[derive(Debug, Default)]
struct Subscriber {
    interested: Mutex<HashSet<String>>,
    pending: Mutex<HashSet<String>>,
}

/// Register a fresh subscriber on the bus (and reap any that have since been dropped).
fn bus_register(sub: &Arc<Subscriber>) {
    let mut bus = BUS.lock().expect("notification bus not poisoned");
    bus.retain(|w| w.strong_count() > 0);
    bus.push(Arc::downgrade(sub));
}

/// Publish `channel` to every subscriber `listen`ing on it (this isolate's and every sibling's).
fn bus_publish(channel: &str) {
    let bus = BUS.lock().expect("notification bus not poisoned");
    for weak in bus.iter() {
        if let Some(sub) = weak.upgrade()
            && sub
                .interested
                .lock()
                .expect("subscriber not poisoned")
                .contains(channel)
        {
            sub.pending
                .lock()
                .expect("subscriber not poisoned")
                .insert(channel.to_string());
        }
    }
}

/// A SQLite-backed [`SqlDriver`] over an owned [`rusqlite::Connection`], plus its bus subscriber. The
/// connection is not cloneable — which is exactly why the extern value ([`crate::conn::ConnectionBox`])
/// shares it through an `Arc<Mutex<…>>` rather than cloning it.
#[derive(Debug)]
pub struct SqliteDriver {
    conn: Connection,
    sub: Arc<Subscriber>,
}

impl SqliteDriver {
    /// The SQL dialect this driver speaks, named **once**: [`SqlDriver::dialect`] reports it (which is
    /// what selects a `migrations/sqlite/` override) and [`SqlDriver::lower_schema`] renders with it,
    /// so the dialect the engine believes is connected and the DDL that reaches the database cannot
    /// come apart.
    const DIALECT: crate::schema::Dialect = crate::schema::Dialect::Sqlite;
}

impl SqliteDriver {
    /// Wrap an open connection: register a bus subscriber and install the update hook that publishes
    /// a changed table's name (= the channel a `db.watch` on that table listens on) to the bus, so any
    /// write through this connection wakes every watcher on the table — in this isolate or a sibling.
    fn wrap(conn: Connection) -> SqliteDriver {
        let sub = Arc::new(Subscriber::default());
        bus_register(&sub);
        // The hook captures nothing (it calls a free fn over the static bus), so it is `Send + 'static`.
        conn.update_hook(Some(|_action, _db: &str, table: &str, _rowid: i64| {
            bus_publish(table)
        }));
        SqliteDriver { conn, sub }
    }

    /// Open the in-memory database (`sqlite::memory:` / `:memory:`).
    pub fn open_in_memory() -> Result<SqliteDriver, String> {
        Connection::open_in_memory()
            .map(SqliteDriver::wrap)
            .map_err(|e| e.to_string())
    }

    /// Open (creating if absent) the database at `path` (`sqlite:app.db`).
    pub fn open_path(path: &str) -> Result<SqliteDriver, String> {
        Connection::open(path)
            .map(SqliteDriver::wrap)
            .map_err(|e| e.to_string())
    }
}

impl SqlDriver for SqliteDriver {
    fn execute(&mut self, sql: &str, params: &[SqlValue]) -> Result<i64, String> {
        let bound = to_rusqlite(params);
        self.conn
            .execute(sql, rusqlite::params_from_iter(bound.iter()))
            .map(|affected| affected as i64)
            .map_err(|e| e.to_string())
    }

    fn query(&mut self, sql: &str, params: &[SqlValue]) -> Result<Vec<Row>, String> {
        let bound = to_rusqlite(params);
        let mut stmt = self.conn.prepare(sql).map_err(|e| e.to_string())?;
        // Each column's name AND the intent its DECLARED type records. The declaration is the only
        // place the schema's meaning survives — SQLite's five storage classes cannot carry it — so it
        // is read here, once per statement, and nowhere above.
        let columns: Vec<(String, ColumnIntent)> = stmt
            .columns()
            .iter()
            .map(|c| (c.name().to_string(), ColumnIntent::of(c.decl_type())))
            .collect();
        let mut rows = stmt
            .query(rusqlite::params_from_iter(bound.iter()))
            .map_err(|e| e.to_string())?;

        let mut out: Vec<Row> = Vec::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let mut record: Row = Vec::with_capacity(columns.len());
            for (i, (name, intent)) in columns.iter().enumerate() {
                let value = row
                    .get_ref(i)
                    .map(|v| from_value_ref(v, *intent))
                    .map_err(|e| e.to_string())?;
                record.push((name.clone(), value));
            }
            out.push(record);
        }
        Ok(out)
    }

    fn execute_batch(&mut self, sql: &str) -> Result<(), String> {
        // rusqlite's `execute_batch` runs every `;`-separated statement in the script — exactly what a
        // migration file body (and the runner's `BEGIN`/`COMMIT`) needs, and what the single-statement
        // `execute` cannot do.
        self.conn.execute_batch(sql).map_err(|e| e.to_string())
    }

    fn dialect(&self) -> Option<crate::schema::Dialect> {
        Some(Self::DIALECT)
    }

    fn lower_schema(&self, statements: &[crate::schema::Statement]) -> Result<String, String> {
        // The dialect is the only thing this driver contributes; the rendering itself is the one
        // shared implementation, so SQLite and Postgres can never grow divergent DDL writers.
        Ok(crate::schema::lower(statements, Self::DIALECT))
    }

    fn reset(&mut self) -> Result<(), String> {
        // Drop every user object. SQLite has no `DROP SCHEMA`, so enumerate `sqlite_master` and drop
        // each table/view/trigger (an index is dropped with its table; the `sqlite_%` internal objects
        // are left alone). Foreign-key enforcement is turned off for the duration so drop order among
        // referencing tables never matters.
        let objects: Vec<(String, String)> = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT type, name FROM sqlite_master \
                     WHERE type IN ('table', 'view', 'trigger') AND name NOT LIKE 'sqlite_%'",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<_, _>>().map_err(|e| e.to_string())?
        };
        let mut script = String::from("PRAGMA foreign_keys = OFF;\n");
        for (kind, name) in objects {
            // `kind` is a fixed sqlite_master token (table/view/trigger); the name is quoted so an
            // arbitrary identifier cannot break out of the statement.
            let quoted = name.replace('"', "\"\"");
            script.push_str(&format!(
                "DROP {} IF EXISTS \"{quoted}\";\n",
                kind.to_uppercase()
            ));
        }
        script.push_str("PRAGMA foreign_keys = ON;\n");
        self.conn.execute_batch(&script).map_err(|e| e.to_string())
    }

    fn listen(&mut self, channel: &str) -> Result<(), String> {
        // Subscribe this connection to `channel` on the process bus. The update hook auto-publishes a
        // changed table's name, and `notify` publishes explicitly; both wake a watcher listening here.
        self.sub
            .interested
            .lock()
            .expect("subscriber not poisoned")
            .insert(channel.to_string());
        Ok(())
    }

    fn notifications(&mut self) -> Result<Vec<String>, String> {
        // Drain the channels that fired since the last poll (non-blocking) — the same contract as
        // Postgres's, so `Watch::pump` is driver-agnostic.
        Ok(self
            .sub
            .pending
            .lock()
            .expect("subscriber not poisoned")
            .drain()
            .collect())
    }

    fn notify(&mut self, channel: &str) -> Result<(), String> {
        // Explicit publish (the write-side companion to `listen`), in addition to the automatic update
        // hook — so a manual `conn.notify(ch)` wakes watchers even without a row change.
        bus_publish(channel);
        Ok(())
    }
}

/// Marshal the neutral parameters into owned rusqlite values (each implements `ToSql`). A `Bool`
/// binds as SQLite's integer 0/1, its natural storage class — and [`ColumnIntent::Bool`] is what reads
/// it back as a boolean, so the round trip closes.
fn to_rusqlite(params: &[SqlValue]) -> Vec<Value> {
    params
        .iter()
        .map(|p| match p {
            SqlValue::Int(n) => Value::Integer(*n),
            SqlValue::Float(f) => Value::Real(*f),
            SqlValue::Text(s) => Value::Text(s.clone()),
            SqlValue::Bool(b) => Value::Integer(i64::from(*b)),
            SqlValue::Bytes(b) => Value::Blob(b.clone()),
            SqlValue::Null => Value::Null,
        })
        .collect()
}

// --- Declared-type intent: what SQLite's storage classes cannot say ------------------------------

/// What a column's **declared type** asks this driver to present, in the cases where SQLite's dynamic
/// storage has lost the distinction the schema drew.
///
/// SQLite has exactly five storage classes — NULL, INTEGER, REAL, TEXT, BLOB — and **no boolean among
/// them**: `done BOOLEAN` stores `true` as the integer `1`, and the stored class alone can never say
/// whether that `1` is a boolean or a count. The declared type can, and this driver is the only layer
/// in the stack that can see it (`sqlite3_column_decltype`). Every layer above receives a
/// [`SqlValue`] — a Noeta value kind — so if the boolean is not recovered *here*, it is gone: a model
/// with a `bool` field then meets a JSON number and the row does not decode at all.
///
/// The classification follows [SQLite's own type-affinity rules](https://sqlite.org/datatype3.html#affinity)
/// (substring matching, in order) with two deliberate departures, both because affinity answers a
/// different question — *how does SQLite store this* — than the one asked here, *what did the schema
/// mean*:
///
/// * `BOOL`/`BOOLEAN` is recognized first. SQLite gives it NUMERIC affinity, i.e. "an integer", which
///   is exactly the information loss this enum exists to undo.
/// * `DATE`/`TIME`/`DATETIME`/`TIMESTAMP` is [`ColumnIntent::Unconstrained`], not numeric. SQLite has
///   no date storage class either, but unlike a boolean a date has **three** conventional encodings
///   (ISO-8601 TEXT, a unix-epoch INTEGER, a Julian-day REAL) and the declaration does not say which.
///   Only the stored class distinguishes them, so it is left alone — coercing would corrupt two
///   encodings out of three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnIntent {
    /// Declared `BOOLEAN`/`BOOL` — stored as INTEGER 0/1, presented as a real `bool`.
    Bool,
    /// Declared with an integer type (`INTEGER`, `BIGINT`, `INT2`, …): any width, one `int`.
    Int,
    /// Declared with a real or fixed-point type (`REAL`, `DOUBLE`, `FLOAT`, `NUMERIC`, `DECIMAL`).
    Float,
    /// Declared with a text type (`TEXT`, `VARCHAR`, `CHAR`, `CLOB`).
    Text,
    /// No declared type (an expression column — `SELECT count(*)` — or an untyped column), a `BLOB`,
    /// a date/time, or a spelling this driver does not recognize. The stored class is authoritative.
    Unconstrained,
}

impl ColumnIntent {
    /// Classify a column's declared type (`None` for an expression column, which has none).
    pub fn of(decl_type: Option<&str>) -> ColumnIntent {
        let Some(decl) = decl_type else {
            return ColumnIntent::Unconstrained;
        };
        let decl = decl.to_ascii_uppercase();
        let has = |needle: &str| decl.contains(needle);
        if has("BOOL") {
            ColumnIntent::Bool
        } else if has("DATE") || has("TIME") {
            // Ambiguous by construction — see the type-level docs.
            ColumnIntent::Unconstrained
        } else if has("INT") {
            ColumnIntent::Int
        } else if has("CHAR") || has("CLOB") || has("TEXT") {
            ColumnIntent::Text
        } else if has("REAL") || has("FLOA") || has("DOUB") || has("NUMERIC") || has("DEC") {
            ColumnIntent::Float
        } else {
            // Includes `BLOB`, where the stored class already carries everything.
            ColumnIntent::Unconstrained
        }
    }

    /// Present a stored value as this column's declaration says it should be read.
    ///
    /// **Only lossless recoveries are applied.** A value the declaration cannot account for is handed
    /// on as it was stored rather than mangled to fit: an integer `7` in a `BOOLEAN` column is not a
    /// boolean, and `3.5` in an `INTEGER` column is not an integer. Passing those through unchanged
    /// makes the disagreement between the table and the model surface as a decode error naming the
    /// column — which is the truth — instead of inventing `true` or `3`.
    fn present(self, value: SqlValue) -> SqlValue {
        match (self, value) {
            // The boolean SQLite could not store. 0/1 is the only honest boolean encoding; any other
            // integer is left as one, and the layer above reports the column as not a boolean.
            (ColumnIntent::Bool, SqlValue::Int(0)) => SqlValue::Bool(false),
            (ColumnIntent::Bool, SqlValue::Int(1)) => SqlValue::Bool(true),
            // A `REAL`/`NUMERIC` column can still hold an INTEGER (NUMERIC affinity keeps an integral
            // value integral, and a REAL column holding a large integer reads back as one), so a
            // `float` field would meet a JSON integer. Widen it — exactly when that is exact.
            (ColumnIntent::Float, SqlValue::Int(n)) if (n as f64) as i64 == n => {
                SqlValue::Float(n as f64)
            }
            // The mirror image: INTEGER affinity converts an integral real on the way in, but a value
            // written before the column existed (or through a column-less expression) can arrive as a
            // REAL. Narrow it when nothing is lost.
            (ColumnIntent::Int, SqlValue::Float(f))
                if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 =>
            {
                SqlValue::Int(f as i64)
            }
            (_, value) => value,
        }
    }
}

/// Read a column value out of a result row, presented as the column's declared type says (see
/// [`ColumnIntent`]). BLOB data crosses as [`SqlValue::Bytes`] verbatim — never lossily decoded as
/// UTF-8, which would silently replace every non-UTF-8 byte.
fn from_value_ref(value: ValueRef<'_>, intent: ColumnIntent) -> SqlValue {
    let stored = match value {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(n) => SqlValue::Int(n),
        ValueRef::Real(f) => SqlValue::Float(f),
        ValueRef::Text(bytes) => SqlValue::Text(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => SqlValue::Bytes(bytes.to_vec()),
    };
    intent.present(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Unique table/channel names per test — the notification bus is a process-global static shared by
    // all tests (which cargo runs concurrently), so distinct names keep them from cross-firing.

    #[test]
    fn a_write_on_one_connection_wakes_a_listener_on_another() {
        // Two independent connections (separate `:memory:` databases — they share no data, only the
        // process bus). This is the cross-*isolate* case: a write on one wakes a watcher on the other.
        let mut listener = SqliteDriver::open_in_memory().unwrap();
        let mut writer = SqliteDriver::open_in_memory().unwrap();
        listener.listen("t_cross_wake").unwrap();
        assert!(
            listener.notifications().unwrap().is_empty(),
            "nothing before any write"
        );

        writer
            .execute("CREATE TABLE t_cross_wake (id INTEGER)", &[])
            .unwrap(); // DDL does not fire the update hook
        assert!(
            listener.notifications().unwrap().is_empty(),
            "CREATE is not a row change"
        );
        writer
            .execute("INSERT INTO t_cross_wake (id) VALUES (1)", &[])
            .unwrap(); // a row change fires the hook → the bus → the listener

        assert_eq!(
            listener.notifications().unwrap(),
            vec!["t_cross_wake".to_string()]
        );
        assert!(listener.notifications().unwrap().is_empty(), "drained");
    }

    #[test]
    fn a_channel_not_listened_on_is_ignored() {
        let mut a = SqliteDriver::open_in_memory().unwrap();
        a.listen("t_ignore_orders").unwrap();
        let mut b = SqliteDriver::open_in_memory().unwrap();
        b.execute("CREATE TABLE t_ignore_users (id INTEGER)", &[])
            .unwrap();
        b.execute("INSERT INTO t_ignore_users (id) VALUES (1)", &[])
            .unwrap(); // fires the other table
        assert!(a.notifications().unwrap().is_empty());
    }

    // --- Declared-type intent (SQLite has no boolean storage class) -------------------------------

    #[test]
    fn a_declared_type_classifies_by_what_the_schema_meant() {
        let of = |d: &str| ColumnIntent::of(Some(d));
        // The whole point: SQLite gives BOOLEAN numeric affinity, and this does not.
        assert_eq!(of("BOOLEAN"), ColumnIntent::Bool);
        assert_eq!(of("bool"), ColumnIntent::Bool);
        assert_eq!(of("INTEGER"), ColumnIntent::Int);
        assert_eq!(of("BIGINT"), ColumnIntent::Int);
        assert_eq!(of("int2"), ColumnIntent::Int);
        assert_eq!(of("TEXT"), ColumnIntent::Text);
        assert_eq!(of("VARCHAR(80)"), ColumnIntent::Text);
        assert_eq!(of("REAL"), ColumnIntent::Float);
        assert_eq!(of("DOUBLE PRECISION"), ColumnIntent::Float);
        assert_eq!(of("FLOAT"), ColumnIntent::Float);
        assert_eq!(of("DECIMAL(10,2)"), ColumnIntent::Float);
        assert_eq!(of("NUMERIC"), ColumnIntent::Float);
        // A blob's stored class already carries everything, and an expression column has no
        // declaration at all.
        assert_eq!(of("BLOB"), ColumnIntent::Unconstrained);
        assert_eq!(ColumnIntent::of(None), ColumnIntent::Unconstrained);
        // Three conventional date encodings, and the declaration does not say which — so hands off.
        assert_eq!(of("DATE"), ColumnIntent::Unconstrained);
        assert_eq!(of("DATETIME"), ColumnIntent::Unconstrained);
        assert_eq!(of("TIMESTAMP"), ColumnIntent::Unconstrained);
    }

    #[test]
    fn only_lossless_recoveries_are_applied() {
        // The boolean SQLite cannot store.
        assert_eq!(
            ColumnIntent::Bool.present(SqlValue::Int(1)),
            SqlValue::Bool(true)
        );
        assert_eq!(
            ColumnIntent::Bool.present(SqlValue::Int(0)),
            SqlValue::Bool(false)
        );
        // Not a boolean at all — left as stored, so the disagreement can be reported instead of
        // invented away.
        assert_eq!(
            ColumnIntent::Bool.present(SqlValue::Int(7)),
            SqlValue::Int(7)
        );
        assert_eq!(
            ColumnIntent::Bool.present(SqlValue::Text("true".into())),
            SqlValue::Text("true".into())
        );
        // A NUMERIC/REAL column holding an integer widens exactly.
        assert_eq!(
            ColumnIntent::Float.present(SqlValue::Int(3)),
            SqlValue::Float(3.0)
        );
        // …but not past f64's exact integer range.
        let inexact = (1_i64 << 53) + 1;
        assert_eq!(
            ColumnIntent::Float.present(SqlValue::Int(inexact)),
            SqlValue::Int(inexact)
        );
        // An INTEGER column holding an integral real narrows; a fractional one does not.
        assert_eq!(
            ColumnIntent::Int.present(SqlValue::Float(4.0)),
            SqlValue::Int(4)
        );
        assert_eq!(
            ColumnIntent::Int.present(SqlValue::Float(4.5)),
            SqlValue::Float(4.5)
        );
        // An unconstrained column is passed through untouched, whatever it holds.
        for v in [
            SqlValue::Int(1),
            SqlValue::Float(1.5),
            SqlValue::Text("x".into()),
            SqlValue::Bytes(vec![0, 159]),
            SqlValue::Null,
        ] {
            assert_eq!(ColumnIntent::Unconstrained.present(v.clone()), v);
        }
    }

    #[test]
    fn a_boolean_column_round_trips_as_a_boolean() {
        let mut db = SqliteDriver::open_in_memory().unwrap();
        db.execute(
            "CREATE TABLE t_bool (id INTEGER PRIMARY KEY, title TEXT, done BOOLEAN)",
            &[],
        )
        .unwrap();
        // Write through the neutral surface as a real `Bool` …
        db.execute(
            "INSERT INTO t_bool (id, title, done) VALUES (?, ?, ?)",
            &[
                SqlValue::Int(1),
                SqlValue::Text("ok".into()),
                SqlValue::Bool(true),
            ],
        )
        .unwrap();
        // … and a literal 0 written as SQL, which is how any other client would spell `false`.
        db.execute("INSERT INTO t_bool VALUES (2, 'no', 0)", &[])
            .unwrap();
        let rows = db.query("SELECT * FROM t_bool ORDER BY id", &[]).unwrap();
        assert_eq!(
            rows,
            vec![
                vec![
                    ("id".to_string(), SqlValue::Int(1)),
                    ("title".to_string(), SqlValue::Text("ok".into())),
                    ("done".to_string(), SqlValue::Bool(true)),
                ],
                vec![
                    ("id".to_string(), SqlValue::Int(2)),
                    ("title".to_string(), SqlValue::Text("no".into())),
                    ("done".to_string(), SqlValue::Bool(false)),
                ],
            ],
            "a BOOLEAN column reads back as a boolean, not as SQLite's stored 0/1 integer"
        );
    }

    #[test]
    fn an_expression_column_has_no_declaration_and_keeps_its_stored_kind() {
        let mut db = SqliteDriver::open_in_memory().unwrap();
        db.execute("CREATE TABLE t_expr (flag BOOLEAN)", &[])
            .unwrap();
        db.execute("INSERT INTO t_expr (flag) VALUES (1), (1), (0)", &[])
            .unwrap();
        // `count(*)` and `sum(flag)` are integers even though `flag` is declared BOOLEAN: the
        // declared type belongs to the column, not to an expression over it.
        let rows = db
            .query("SELECT count(*) AS n, sum(flag) AS done FROM t_expr", &[])
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![
                ("n".to_string(), SqlValue::Int(3)),
                ("done".to_string(), SqlValue::Int(2)),
            ]]
        );
    }

    #[test]
    fn a_real_column_holding_an_integer_still_reads_as_a_float() {
        let mut db = SqliteDriver::open_in_memory().unwrap();
        // NUMERIC affinity keeps an integral value integral, so `price` is stored as INTEGER 3 —
        // exactly the case a `float` model field cannot decode.
        db.execute("CREATE TABLE t_num (price DECIMAL(10,2), ratio REAL)", &[])
            .unwrap();
        db.execute("INSERT INTO t_num VALUES (3, 2)", &[]).unwrap();
        let rows = db.query("SELECT * FROM t_num", &[]).unwrap();
        assert_eq!(
            rows,
            vec![vec![
                ("price".to_string(), SqlValue::Float(3.0)),
                ("ratio".to_string(), SqlValue::Float(2.0)),
            ]]
        );
    }

    #[test]
    fn a_blob_crosses_as_bytes_and_is_never_decoded_as_text() {
        let mut db = SqliteDriver::open_in_memory().unwrap();
        db.execute("CREATE TABLE t_blob (data BLOB)", &[]).unwrap();
        // Not valid UTF-8: a lossy text decode would replace both bytes and lose the data.
        let raw = vec![0xffu8, 0x00, 0xfe, b'h', b'i'];
        db.execute(
            "INSERT INTO t_blob (data) VALUES (?)",
            &[SqlValue::Bytes(raw.clone())],
        )
        .unwrap();
        let rows = db.query("SELECT data FROM t_blob", &[]).unwrap();
        assert_eq!(
            rows,
            vec![vec![("data".to_string(), SqlValue::Bytes(raw))]],
            "binary column data survives the round trip byte for byte"
        );
    }

    #[test]
    fn an_explicit_notify_wakes_a_listener_without_a_write() {
        let mut listener = SqliteDriver::open_in_memory().unwrap();
        listener.listen("t_explicit").unwrap();
        let mut other = SqliteDriver::open_in_memory().unwrap();
        other.notify("t_explicit").unwrap();
        assert_eq!(
            listener.notifications().unwrap(),
            vec!["t_explicit".to_string()]
        );
    }
}
