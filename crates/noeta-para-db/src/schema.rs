//! The **portable schema DSL** — the peer of the query builder, one level up. Where
//! [`crate::query`-shaped](crate) Noeta composes a statement with neutral `?` placeholders that each
//! driver rewrites into its own binding syntax, a schema statement is composed as a **backend-neutral
//! description of the change** ([`Statement`]) that each driver lowers into its own DDL. The dialect
//! lives at the driver seam ([`crate::driver::SqlDriver::lower_schema`]) exactly like the `?`→`$N`
//! rewrite does, so nothing above the seam ever branches on the backend.
//!
//! # The three pieces
//!
//! 1. **A source syntax** — a small fluent notation, deliberately shaped like the Noeta builder that
//!    renders it, so a migration file reads as code:
//!
//!    ```text
//!    create_table("todos")
//!        .id()
//!        .text("title").not_null()
//!        .bool("done").default(false)
//!        .timestamps()
//!    ```
//!
//!    [`parse`] turns that text into statements. It is the body of a `.schema` migration file, and it
//!    is what `para.db.schema`'s Noeta builder renders — **one** grammar, so the file format and the
//!    programmatic builder cannot drift apart.
//!
//! 2. **A neutral IR** — [`Statement`] and friends: what to create, not how to spell it.
//!
//! 3. **A per-dialect lowering** — [`lower`], parameterized by [`Dialect`]. The driver picks the
//!    dialect; the rendering itself is one implementation, so the two backends can never grow
//!    independent DDL writers.
//!
//! # What is deliberately NOT here
//!
//! Only constructs that lower to genuinely equivalent DDL on both backends are in the vocabulary.
//! Types with no honest counterpart (`uuid`, `json`/`jsonb`, `bytea`/`blob`, exact `decimal`),
//! partial/expression indexes, check constraints, views, triggers, extensions, and every other
//! dialect-specific object are **out of scope by design**: a raw `.sql` migration is the escape
//! hatch, and mixing the two in one `migrations/` directory is expected. Approximating a Postgres
//! `jsonb` column with a SQLite `TEXT` column would compile and then behave differently, which is
//! worse than not offering it.

use std::fmt;

// --- Dialect ------------------------------------------------------------------------------------

/// The SQL dialect a schema statement is lowered into. Each [`crate::driver::SqlDriver`] names its
/// own; this enum exists so the *rendering* is one implementation rather than one per driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// SQLite: `INTEGER PRIMARY KEY AUTOINCREMENT` identities, `REAL` floats.
    Sqlite,
    /// PostgreSQL: `BIGSERIAL PRIMARY KEY` identities, `DOUBLE PRECISION` floats.
    Postgres,
}

// --- The neutral IR -----------------------------------------------------------------------------

/// A portable column type — the set that maps onto both backends with the same storage and the same
/// value round-trip through [`crate::driver::SqlValue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    /// The table's surrogate identity: an auto-assigned integer primary key.
    Id,
    /// Unbounded UTF-8 text (`TEXT` on both).
    Text,
    /// A 32-bit integer (`INTEGER` on both).
    Int,
    /// A 64-bit integer (`BIGINT` on both) — the type that matches an [`ColumnType::Id`] foreign key.
    BigInt,
    /// A 64-bit float (`REAL` on SQLite, `DOUBLE PRECISION` on Postgres — both IEEE-754 binary64).
    Float,
    /// A boolean (`BOOLEAN` on both; SQLite stores it as 0/1 and reads it back as an integer).
    Bool,
    /// A date-and-time without a zone (`TIMESTAMP` on both).
    Timestamp,
}

impl ColumnType {
    /// The DDL type name for `dialect`. [`ColumnType::Id`] is not a type name but a whole column
    /// definition, so it is rendered by [`render_column`] instead and never reaches here.
    fn sql(self, dialect: Dialect) -> &'static str {
        match (self, dialect) {
            (ColumnType::Id, _) => unreachable!("an identity column is rendered whole"),
            (ColumnType::Text, _) => "TEXT",
            (ColumnType::Int, _) => "INTEGER",
            (ColumnType::BigInt, _) => "BIGINT",
            (ColumnType::Float, Dialect::Sqlite) => "REAL",
            (ColumnType::Float, Dialect::Postgres) => "DOUBLE PRECISION",
            (ColumnType::Bool, _) => "BOOLEAN",
            (ColumnType::Timestamp, _) => "TIMESTAMP",
        }
    }
}

/// A column default. Only literals and `CURRENT_TIMESTAMP` — an arbitrary SQL expression would be
/// dialect-specific by construction, so it belongs in a raw migration.
#[derive(Debug, Clone, PartialEq)]
pub enum DefaultValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    /// `CURRENT_TIMESTAMP` — the one non-literal default that is spelled identically on both backends.
    Now,
}

impl DefaultValue {
    /// The DDL literal. Text is single-quoted with `''` escaping (identical on both backends).
    fn sql(&self) -> String {
        match self {
            DefaultValue::Bool(true) => "TRUE".to_string(),
            DefaultValue::Bool(false) => "FALSE".to_string(),
            DefaultValue::Int(n) => n.to_string(),
            DefaultValue::Float(f) => {
                // Always render a decimal point so a whole float is still a float literal.
                if f.fract() == 0.0 && f.is_finite() {
                    format!("{f:.1}")
                } else {
                    f.to_string()
                }
            }
            DefaultValue::Text(s) => format!("'{}'", s.replace('\'', "''")),
            DefaultValue::Now => "CURRENT_TIMESTAMP".to_string(),
        }
    }
}

/// What a foreign key does to the referencing row when the referenced row changes. The five standard
/// actions, spelled identically on both backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefAction {
    Cascade,
    Restrict,
    SetNull,
    SetDefault,
    NoAction,
}

impl RefAction {
    fn sql(self) -> &'static str {
        match self {
            RefAction::Cascade => "CASCADE",
            RefAction::Restrict => "RESTRICT",
            RefAction::SetNull => "SET NULL",
            RefAction::SetDefault => "SET DEFAULT",
            RefAction::NoAction => "NO ACTION",
        }
    }

    /// Parse the DSL spelling (`"cascade"`, `"set_null"`, …), case-insensitively and accepting either
    /// `_` or `-`/space between words.
    fn parse(raw: &str) -> Option<RefAction> {
        let normalized = raw.to_ascii_lowercase().replace(['-', ' '], "_");
        Some(match normalized.as_str() {
            "cascade" => RefAction::Cascade,
            "restrict" => RefAction::Restrict,
            "set_null" => RefAction::SetNull,
            "set_default" => RefAction::SetDefault,
            "no_action" => RefAction::NoAction,
            _ => return None,
        })
    }
}

/// A foreign-key reference from one column to another table's column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub table: String,
    pub column: String,
    pub on_delete: Option<RefAction>,
    pub on_update: Option<RefAction>,
}

/// One column: its name, type, and the constraints that are portable per-column.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    pub not_null: bool,
    pub unique: bool,
    pub primary_key: bool,
    pub default: Option<DefaultValue>,
    pub references: Option<ForeignKey>,
}

impl Column {
    /// A bare column of `ty` named `name`, nullable and unconstrained — SQL's own defaults, so what
    /// the DSL leaves unsaid is what SQL leaves unsaid.
    fn new(name: String, ty: ColumnType) -> Column {
        Column {
            name,
            ty,
            not_null: false,
            unique: false,
            primary_key: false,
            default: None,
            references: None,
        }
    }
}

/// A `create_table` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub name: String,
    pub if_not_exists: bool,
    pub columns: Vec<Column>,
    /// A composite primary key (`primary_key("a", "b")`); empty when the key is a column's own.
    pub primary_key: Vec<String>,
    /// Composite `UNIQUE (…)` constraints (`unique("a", "b")`).
    pub uniques: Vec<Vec<String>>,
}

/// A `create_index` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIndex {
    pub table: String,
    /// An explicit `name(…)`; when absent the name is derived as `idx_<table>_<columns>`.
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub unique: bool,
    pub if_not_exists: bool,
}

impl CreateIndex {
    /// The index's name — the explicit one, else `idx_<table>_<col>_<col>…`. Derived rather than left
    /// to the backend because SQLite and Postgres name an unnamed index differently, and a later
    /// `drop_index` has to be able to spell it.
    pub fn index_name(&self) -> String {
        match &self.name {
            Some(name) => name.clone(),
            None => format!("idx_{}_{}", self.table, self.columns.join("_")),
        }
    }
}

/// One action inside an `alter_table` chain. Each lowers to its own `ALTER TABLE` statement: SQLite
/// accepts exactly one action per statement, so one-per-statement is the portable shape.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterAction {
    AddColumn(Column),
    DropColumn(String),
    RenameColumn { from: String, to: String },
    RenameTable(String),
}

/// An `alter_table` statement: a table and the actions to apply to it, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterTable {
    pub name: String,
    pub actions: Vec<AlterAction>,
}

/// One schema change, backend-neutral.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable { name: String, if_exists: bool },
    CreateIndex(CreateIndex),
    DropIndex { name: String, if_exists: bool },
    AlterTable(AlterTable),
}

// --- Errors -------------------------------------------------------------------------------------

/// A schema-DSL source error, always carrying the 1-based line it was found on so a migration file
/// error points at the offending call rather than at the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError {
    pub line: usize,
    pub message: String,
}

impl SchemaError {
    fn new(line: usize, message: impl Into<String>) -> SchemaError {
        SchemaError {
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for SchemaError {}

type Parsed<T> = Result<T, SchemaError>;

// --- Lexer --------------------------------------------------------------------------------------

/// One literal argument to a DSL call.
#[derive(Debug, Clone, PartialEq)]
enum Literal {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

/// One `.name(args)` link of a chain (the root call is the same shape without the dot).
#[derive(Debug, Clone, PartialEq)]
struct Call {
    name: String,
    args: Vec<Literal>,
    line: usize,
}

/// A lexical token of the DSL.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Literal(Literal),
    Dot,
    Open,
    Close,
    Comma,
}

/// A token with the line it started on.
#[derive(Debug, Clone, PartialEq)]
struct Spanned {
    token: Token,
    line: usize,
}

/// Tokenize the DSL source. Line comments are `//` (Noeta) and `--` (SQL); both are accepted so a
/// `.schema` file reads naturally to either audience.
fn lex(src: &str) -> Parsed<Vec<Spanned>> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        // Line comments.
        if (c == '/' && chars.get(i + 1) == Some(&'/'))
            || (c == '-' && chars.get(i + 1) == Some(&'-'))
        {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        match c {
            '.' => {
                out.push(Spanned {
                    token: Token::Dot,
                    line,
                });
                i += 1;
            }
            '(' => {
                out.push(Spanned {
                    token: Token::Open,
                    line,
                });
                i += 1;
            }
            ')' => {
                out.push(Spanned {
                    token: Token::Close,
                    line,
                });
                i += 1;
            }
            ',' => {
                out.push(Spanned {
                    token: Token::Comma,
                    line,
                });
                i += 1;
            }
            '"' => {
                let (text, next, newlines) = lex_string(&chars, i, line)?;
                out.push(Spanned {
                    token: Token::Literal(Literal::Str(text)),
                    line,
                });
                line += newlines;
                i = next;
            }
            c if c.is_ascii_digit()
                || (c == '-' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) =>
            {
                let start = i;
                if chars[i] == '-' {
                    i += 1;
                }
                let mut seen_dot = false;
                while i < chars.len()
                    && (chars[i].is_ascii_digit()
                        || (chars[i] == '.' && !seen_dot && is_digit_at(&chars, i + 1)))
                {
                    if chars[i] == '.' {
                        seen_dot = true;
                    }
                    i += 1;
                }
                let text: String = chars[start..i].iter().collect();
                let literal =
                    if seen_dot {
                        Literal::Float(text.parse::<f64>().map_err(|_| {
                            SchemaError::new(line, format!("`{text}` is not a number"))
                        })?)
                    } else {
                        Literal::Int(text.parse::<i64>().map_err(|_| {
                            SchemaError::new(line, format!("`{text}` is not a whole number"))
                        })?)
                    };
                out.push(Spanned {
                    token: Token::Literal(literal),
                    line,
                });
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let token = match word.as_str() {
                    "true" => Token::Literal(Literal::Bool(true)),
                    "false" => Token::Literal(Literal::Bool(false)),
                    _ => Token::Ident(word),
                };
                out.push(Spanned { token, line });
            }
            other => {
                return Err(SchemaError::new(
                    line,
                    format!("unexpected character `{other}`"),
                ));
            }
        }
    }
    Ok(out)
}

fn is_digit_at(chars: &[char], i: usize) -> bool {
    chars.get(i).is_some_and(char::is_ascii_digit)
}

/// Read a double-quoted string starting at `chars[start]`, returning its contents, the index just
/// past the closing quote, and how many newlines it spanned. `\"`, `\\`, `\n`, `\t` are escapes.
fn lex_string(chars: &[char], start: usize, line: usize) -> Parsed<(String, usize, usize)> {
    let mut i = start + 1;
    let mut text = String::new();
    let mut newlines = 0usize;
    while i < chars.len() {
        match chars[i] {
            '"' => return Ok((text, i + 1, newlines)),
            '\\' => {
                let escaped = chars
                    .get(i + 1)
                    .copied()
                    .ok_or_else(|| SchemaError::new(line, "a string ends with a dangling `\\`"))?;
                text.push(match escaped {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
                i += 2;
            }
            c => {
                if c == '\n' {
                    newlines += 1;
                }
                text.push(c);
                i += 1;
            }
        }
    }
    Err(SchemaError::new(line, "an unterminated string literal"))
}

// --- Parser: tokens → call chains ---------------------------------------------------------------

/// Split the token stream into statements, each a chain of [`Call`]s (`root(...)` then every
/// `.link(...)` that follows). A chain ends at the first token that is not a `.`, so statements need
/// no separator: one per top-level call, however it is wrapped across lines.
fn chains(tokens: &[Spanned]) -> Parsed<Vec<Vec<Call>>> {
    let mut out: Vec<Vec<Call>> = Vec::new();
    let mut i = 0usize;
    while i < tokens.len() {
        let call = read_call(tokens, &mut i)?;
        let mut chain = vec![call];
        while matches!(tokens.get(i).map(|t| &t.token), Some(Token::Dot)) {
            i += 1;
            chain.push(read_call(tokens, &mut i)?);
        }
        out.push(chain);
    }
    Ok(out)
}

/// Read one `name(arg, …)` call, advancing `i` past it.
fn read_call(tokens: &[Spanned], i: &mut usize) -> Parsed<Call> {
    let Some(head) = tokens.get(*i) else {
        return Err(SchemaError::new(
            0,
            "the source ends where a call was expected",
        ));
    };
    let Token::Ident(name) = &head.token else {
        return Err(SchemaError::new(
            head.line,
            format!("expected a call name, found {}", describe(&head.token)),
        ));
    };
    let name = name.clone();
    let line = head.line;
    *i += 1;
    match tokens.get(*i).map(|t| &t.token) {
        Some(Token::Open) => *i += 1,
        _ => {
            return Err(SchemaError::new(
                line,
                format!("`{name}` must be called: write `{name}(…)`"),
            ));
        }
    }

    let mut args = Vec::new();
    loop {
        match tokens.get(*i) {
            None => return Err(SchemaError::new(line, format!("`{name}(` is never closed"))),
            Some(Spanned {
                token: Token::Close,
                ..
            }) => {
                *i += 1;
                break;
            }
            Some(Spanned {
                token: Token::Literal(literal),
                ..
            }) => {
                args.push(literal.clone());
                *i += 1;
                match tokens.get(*i).map(|t| &t.token) {
                    Some(Token::Comma) => *i += 1,
                    Some(Token::Close) => {}
                    None => {
                        return Err(SchemaError::new(line, format!("`{name}(` is never closed")));
                    }
                    _ => {
                        return Err(SchemaError::new(
                            line,
                            format!("expected `,` or `)` in the arguments of `{name}`"),
                        ));
                    }
                }
            }
            Some(other) => {
                return Err(SchemaError::new(
                    other.line,
                    format!(
                        "`{name}` takes literal arguments only, found {}",
                        describe(&other.token)
                    ),
                ));
            }
        }
    }
    Ok(Call { name, args, line })
}

impl Call {
    fn new(name: &str, args: Vec<Literal>, line: usize) -> Call {
        Call {
            name: name.to_string(),
            args,
            line,
        }
    }
}

fn describe(token: &Token) -> String {
    match token {
        Token::Ident(name) => format!("`{name}`"),
        Token::Literal(Literal::Str(s)) => format!("the string \"{s}\""),
        Token::Literal(Literal::Int(n)) => format!("the number {n}"),
        Token::Literal(Literal::Float(f)) => format!("the number {f}"),
        Token::Literal(Literal::Bool(b)) => format!("`{b}`"),
        Token::Dot => "`.`".to_string(),
        Token::Open => "`(`".to_string(),
        Token::Close => "`)`".to_string(),
        Token::Comma => "`,`".to_string(),
    }
}

// --- Builder: call chains → statements ----------------------------------------------------------

/// Parse schema-DSL source into backend-neutral [`Statement`]s. Every identifier is validated here
/// (see [`ident`]), so a lowered statement can splice names into DDL without quoting or injection
/// risk, and every unportable combination is rejected with a message that names the portable
/// alternative rather than emitting DDL that only one backend accepts.
pub fn parse(src: &str) -> Parsed<Vec<Statement>> {
    let tokens = lex(src)?;
    let chains = chains(&tokens)?;
    chains.into_iter().map(build).collect()
}

/// Turn one call chain into a statement.
fn build(chain: Vec<Call>) -> Parsed<Statement> {
    let (root, rest) = chain.split_first().expect("a chain has a root call");
    match root.name.as_str() {
        "create_table" => build_create_table(root, rest).map(Statement::CreateTable),
        "drop_table" => {
            let name = one_ident(root)?;
            let if_exists = only_flag(rest, "if_exists", "drop_table")?;
            Ok(Statement::DropTable { name, if_exists })
        }
        "create_index" => build_create_index(root, rest).map(Statement::CreateIndex),
        "drop_index" => {
            let name = one_ident(root)?;
            let if_exists = only_flag(rest, "if_exists", "drop_index")?;
            Ok(Statement::DropIndex { name, if_exists })
        }
        "alter_table" => build_alter_table(root, rest).map(Statement::AlterTable),
        other => Err(SchemaError::new(
            root.line,
            format!(
                "unknown schema statement `{other}` (expected one of: create_table, drop_table, \
                 create_index, drop_index, alter_table). Anything outside the portable vocabulary \
                 belongs in a raw `.sql` migration."
            ),
        )),
    }
}

/// The `drop_table` / `drop_index` tail: nothing, or a single `if_exists()`.
fn only_flag(rest: &[Call], flag: &str, root: &str) -> Parsed<bool> {
    let mut set = false;
    for call in rest {
        if call.name == flag && call.args.is_empty() {
            set = true;
        } else {
            return Err(SchemaError::new(
                call.line,
                format!("`{root}` accepts only `.{flag}()`, not `.{}(…)`", call.name),
            ));
        }
    }
    Ok(set)
}

/// The single identifier argument of a root call (`create_table("todos")`).
fn one_ident(call: &Call) -> Parsed<String> {
    match call.args.as_slice() {
        [Literal::Str(name)] => ident(name, call.line),
        _ => Err(SchemaError::new(
            call.line,
            format!(
                "`{}` takes exactly one name, e.g. `{}(\"todos\")`",
                call.name, call.name
            ),
        )),
    }
}

/// Validate a SQL identifier: ASCII letters/digits/underscore, not starting with a digit. Unquoted
/// names are emitted verbatim, so this is both the portability gate (no case-folding surprise between
/// SQLite's case-preserving unquoted names and Postgres's lowercasing) and the injection gate.
fn ident(raw: &str, line: usize) -> Parsed<String> {
    let ok = !raw.is_empty()
        && raw.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !raw.starts_with(|c: char| c.is_ascii_digit());
    if ok {
        Ok(raw.to_string())
    } else {
        Err(SchemaError::new(
            line,
            format!(
                "`{raw}` is not a portable identifier — use letters, digits, and underscores, \
                 starting with a letter or underscore (a name needing quoting is dialect-specific; \
                 use a raw `.sql` migration)"
            ),
        ))
    }
}

/// Every argument of a call as validated identifiers (for the composite `primary_key`/`unique`).
fn ident_args(call: &Call) -> Parsed<Vec<String>> {
    call.args
        .iter()
        .map(|arg| match arg {
            Literal::Str(s) => ident(s, call.line),
            other => Err(SchemaError::new(
                call.line,
                format!(
                    "`{}` takes column names as strings, found {}",
                    call.name,
                    describe(&Token::Literal(other.clone()))
                ),
            )),
        })
        .collect()
}

/// The column type a `create_table` link declares, if it declares one.
fn column_type(name: &str) -> Option<ColumnType> {
    Some(match name {
        "id" => ColumnType::Id,
        "text" => ColumnType::Text,
        "int" => ColumnType::Int,
        "bigint" => ColumnType::BigInt,
        "float" => ColumnType::Float,
        "bool" => ColumnType::Bool,
        "timestamp" => ColumnType::Timestamp,
        _ => return None,
    })
}

fn build_create_table(root: &Call, rest: &[Call]) -> Parsed<CreateTable> {
    let mut table = CreateTable {
        name: one_ident(root)?,
        if_not_exists: false,
        columns: Vec::new(),
        primary_key: Vec::new(),
        uniques: Vec::new(),
    };

    for call in rest {
        if let Some(ty) = column_type(&call.name) {
            table.columns.push(new_column(call, ty)?);
            continue;
        }
        match call.name.as_str() {
            "timestamps" => {
                no_args(call)?;
                for name in ["created_at", "updated_at"] {
                    let mut column = Column::new(name.to_string(), ColumnType::Timestamp);
                    column.not_null = true;
                    column.default = Some(DefaultValue::Now);
                    table.columns.push(column);
                }
            }
            "if_not_exists" => {
                no_args(call)?;
                table.if_not_exists = true;
            }
            "primary_key" if !call.args.is_empty() => {
                if !table.primary_key.is_empty() {
                    return Err(SchemaError::new(
                        call.line,
                        "a table has at most one composite `primary_key(…)`",
                    ));
                }
                table.primary_key = ident_args(call)?;
            }
            "unique" if !call.args.is_empty() => table.uniques.push(ident_args(call)?),
            _ => {
                let column = table.columns.last_mut().ok_or_else(|| {
                    SchemaError::new(
                        call.line,
                        format!(
                            "`.{}(…)` modifies the column before it, but no column has been \
                             declared yet",
                            call.name
                        ),
                    )
                })?;
                apply_modifier(column, call, "create_table")?;
            }
        }
    }

    if table.columns.is_empty() {
        return Err(SchemaError::new(
            root.line,
            format!("table `{}` declares no columns", table.name),
        ));
    }
    let column_pk = table
        .columns
        .iter()
        .any(|c| c.primary_key || c.ty == ColumnType::Id);
    if column_pk && !table.primary_key.is_empty() {
        return Err(SchemaError::new(
            root.line,
            format!(
                "table `{}` declares both a column primary key (`id()`/`.primary_key()`) and a \
                 composite `primary_key(…)` — pick one",
                table.name
            ),
        ));
    }
    if table
        .columns
        .iter()
        .filter(|c| c.ty == ColumnType::Id)
        .count()
        > 1
    {
        return Err(SchemaError::new(
            root.line,
            format!(
                "table `{}` declares more than one `id()` column",
                table.name
            ),
        ));
    }
    if table.columns.iter().filter(|c| c.primary_key).count() > 1 {
        return Err(SchemaError::new(
            root.line,
            format!(
                "table `{}` marks more than one column `.primary_key()` — use the table-level \
                 `primary_key(\"a\", \"b\")` for a composite key",
                table.name
            ),
        ));
    }
    Ok(table)
}

/// Build a fresh column from a type-declaring call (`text("title")`, `id()`, `id("uid")`).
fn new_column(call: &Call, ty: ColumnType) -> Parsed<Column> {
    let name = match (ty, call.args.as_slice()) {
        // `id()` names itself `id`; `id("uid")` overrides.
        (ColumnType::Id, []) => "id".to_string(),
        (_, [Literal::Str(name)]) => ident(name, call.line)?,
        _ => {
            return Err(SchemaError::new(
                call.line,
                format!(
                    "`{}` takes one column name, e.g. `{}(\"title\")`",
                    call.name, call.name
                ),
            ));
        }
    };
    let mut column = Column::new(name, ty);
    if ty == ColumnType::Id {
        // An identity column is the primary key and can never be null — say so in the IR so a
        // later `.not_null()` is redundant rather than contradictory.
        column.not_null = true;
    }
    Ok(column)
}

/// Apply one column-modifier link to the column it follows.
fn apply_modifier(column: &mut Column, call: &Call, context: &str) -> Parsed<()> {
    match call.name.as_str() {
        "not_null" => {
            no_args(call)?;
            column.not_null = true;
        }
        "unique" => {
            no_args(call)?;
            column.unique = true;
        }
        "primary_key" => {
            no_args(call)?;
            column.primary_key = true;
        }
        "default" => {
            let value = match call.args.as_slice() {
                [Literal::Bool(b)] => DefaultValue::Bool(*b),
                [Literal::Int(n)] => DefaultValue::Int(*n),
                [Literal::Float(f)] => DefaultValue::Float(*f),
                [Literal::Str(s)] => DefaultValue::Text(s.clone()),
                _ => {
                    return Err(SchemaError::new(
                        call.line,
                        "`default(…)` takes exactly one literal (a string, number, or boolean)",
                    ));
                }
            };
            column.default = Some(value);
        }
        "default_now" => {
            no_args(call)?;
            column.default = Some(DefaultValue::Now);
        }
        "references" => {
            let (table, referenced) = match call.args.as_slice() {
                [Literal::Str(table), Literal::Str(col)] => {
                    (ident(table, call.line)?, ident(col, call.line)?)
                }
                _ => {
                    return Err(SchemaError::new(
                        call.line,
                        "`references(…)` takes the referenced table and column, e.g. \
                         `references(\"users\", \"id\")`",
                    ));
                }
            };
            column.references = Some(ForeignKey {
                table,
                column: referenced,
                on_delete: None,
                on_update: None,
            });
        }
        "on_delete" | "on_update" => {
            let action = ref_action(call)?;
            let key = column.references.as_mut().ok_or_else(|| {
                SchemaError::new(
                    call.line,
                    format!(
                        "`.{}(…)` needs a `.references(table, column)` before it",
                        call.name
                    ),
                )
            })?;
            if call.name == "on_delete" {
                key.on_delete = Some(action);
            } else {
                key.on_update = Some(action);
            }
        }
        other => {
            return Err(SchemaError::new(
                call.line,
                format!(
                    "unknown `{context}` modifier `.{other}(…)` (expected one of: not_null, \
                     unique, primary_key, default, default_now, references, on_delete, on_update)"
                ),
            ));
        }
    }
    Ok(())
}

fn ref_action(call: &Call) -> Parsed<RefAction> {
    match call.args.as_slice() {
        [Literal::Str(raw)] => RefAction::parse(raw).ok_or_else(|| {
            SchemaError::new(
                call.line,
                format!(
                    "`{raw}` is not a referential action (expected cascade, restrict, set_null, \
                     set_default, or no_action)"
                ),
            )
        }),
        _ => Err(SchemaError::new(
            call.line,
            format!(
                "`{}(…)` takes one action, e.g. `{}(\"cascade\")`",
                call.name, call.name
            ),
        )),
    }
}

fn no_args(call: &Call) -> Parsed<()> {
    if call.args.is_empty() {
        Ok(())
    } else {
        Err(SchemaError::new(
            call.line,
            format!("`{}` takes no arguments", call.name),
        ))
    }
}

fn build_create_index(root: &Call, rest: &[Call]) -> Parsed<CreateIndex> {
    let mut index = CreateIndex {
        table: one_ident(root)?,
        name: None,
        columns: Vec::new(),
        unique: false,
        if_not_exists: false,
    };
    for call in rest {
        match call.name.as_str() {
            "column" => index.columns.push(one_ident(call)?),
            "columns" => index.columns.extend(ident_args(call)?),
            "name" => index.name = Some(one_ident(call)?),
            "unique" => {
                no_args(call)?;
                index.unique = true;
            }
            "if_not_exists" => {
                no_args(call)?;
                index.if_not_exists = true;
            }
            other => {
                return Err(SchemaError::new(
                    call.line,
                    format!(
                        "unknown `create_index` modifier `.{other}(…)` (expected one of: column, \
                         columns, name, unique, if_not_exists)"
                    ),
                ));
            }
        }
    }
    if index.columns.is_empty() {
        return Err(SchemaError::new(
            root.line,
            format!(
                "the index on `{}` names no column — add `.column(\"…\")`",
                index.table
            ),
        ));
    }
    Ok(index)
}

fn build_alter_table(root: &Call, rest: &[Call]) -> Parsed<AlterTable> {
    let mut alter = AlterTable {
        name: one_ident(root)?,
        actions: Vec::new(),
    };
    for call in rest {
        if let Some(kind) = call.name.strip_prefix("add_")
            && let Some(ty) = column_type(kind)
        {
            if ty == ColumnType::Id {
                return Err(SchemaError::new(
                    call.line,
                    "an identity column cannot be added to an existing table portably — create the \
                     table with `id()`, or write a raw `.sql` migration",
                ));
            }
            let column = new_column(&Call::new(kind, call.args.clone(), call.line), ty)?;
            alter.actions.push(AlterAction::AddColumn(column));
            continue;
        }
        match call.name.as_str() {
            "drop_column" => alter
                .actions
                .push(AlterAction::DropColumn(one_ident(call)?)),
            "rename_column" => {
                let (from, to) = match call.args.as_slice() {
                    [Literal::Str(from), Literal::Str(to)] => {
                        (ident(from, call.line)?, ident(to, call.line)?)
                    }
                    _ => {
                        return Err(SchemaError::new(
                            call.line,
                            "`rename_column(…)` takes the old and new names, e.g. \
                             `rename_column(\"body\", \"content\")`",
                        ));
                    }
                };
                alter.actions.push(AlterAction::RenameColumn { from, to });
            }
            "rename_to" => alter
                .actions
                .push(AlterAction::RenameTable(one_ident(call)?)),
            _ => {
                // Otherwise it modifies the column the preceding `add_*` introduced.
                let Some(AlterAction::AddColumn(column)) = alter.actions.last_mut() else {
                    return Err(SchemaError::new(
                        call.line,
                        format!(
                            "unknown `alter_table` action `.{}(…)` (expected one of: add_text, \
                             add_int, add_bigint, add_float, add_bool, add_timestamp, drop_column, \
                             rename_column, rename_to)",
                            call.name
                        ),
                    ));
                };
                apply_modifier(column, call, "alter_table")?;
            }
        }
    }
    if alter.actions.is_empty() {
        return Err(SchemaError::new(
            root.line,
            format!("`alter_table(\"{}\")` names no change", alter.name),
        ));
    }
    validate_alter(root, &alter)?;
    Ok(alter)
}

/// Reject the `alter_table` shapes that only one backend accepts, naming the portable alternative.
/// SQLite's `ALTER TABLE ADD COLUMN` is far narrower than Postgres's — it cannot add a `NOT NULL`
/// column without a default (even to an empty table), cannot add a `UNIQUE` or `PRIMARY KEY` column,
/// and cannot add a column whose default is not a constant. Emitting DDL that works on Postgres and
/// fails on SQLite would make a "portable" migration backend-dependent, so these are errors here.
fn validate_alter(root: &Call, alter: &AlterTable) -> Parsed<()> {
    if alter.actions.len() > 1
        && alter
            .actions
            .iter()
            .any(|a| matches!(a, AlterAction::RenameTable(_)))
    {
        return Err(SchemaError::new(
            root.line,
            "`rename_to(…)` must be the only action in its `alter_table` chain — later actions \
             would name a table that no longer exists",
        ));
    }
    for action in &alter.actions {
        let AlterAction::AddColumn(column) = action else {
            continue;
        };
        let name = &column.name;
        if column.not_null && column.default.is_none() {
            return Err(SchemaError::new(
                root.line,
                format!(
                    "adding `{name}` as `not_null` needs a `default(…)` — SQLite cannot add a NOT \
                     NULL column without one"
                ),
            ));
        }
        if column.unique {
            return Err(SchemaError::new(
                root.line,
                format!(
                    "a column added to an existing table cannot be `unique()` — add it plain, then \
                     `create_index(\"{}\").column(\"{name}\").unique()`",
                    alter.name
                ),
            ));
        }
        if column.primary_key {
            return Err(SchemaError::new(
                root.line,
                format!("`{name}` cannot become a primary key by being added to an existing table"),
            ));
        }
        if matches!(column.default, Some(DefaultValue::Now)) {
            return Err(SchemaError::new(
                root.line,
                format!(
                    "adding `{name}` with `default_now()` is not portable — SQLite requires a \
                     constant default on ADD COLUMN; add it with a literal default, or backfill in \
                     a raw `.sql` migration"
                ),
            ));
        }
        if column.references.is_some() && column.default.is_some() {
            return Err(SchemaError::new(
                root.line,
                format!(
                    "adding the foreign-key column `{name}` with a default is not portable — \
                     SQLite requires a NULL default on an added REFERENCES column"
                ),
            ));
        }
    }
    Ok(())
}

// --- Lowering -----------------------------------------------------------------------------------

/// Lower backend-neutral statements into `dialect`'s DDL — a `;`-terminated script ready for
/// [`crate::driver::SqlDriver::execute_batch`]. Infallible: [`parse`] has already rejected everything
/// that cannot be expressed on both backends, so lowering is pure rendering.
pub fn lower(statements: &[Statement], dialect: Dialect) -> String {
    let mut out = String::new();
    for statement in statements {
        match statement {
            Statement::CreateTable(table) => out.push_str(&lower_create_table(table, dialect)),
            Statement::DropTable { name, if_exists } => {
                out.push_str("DROP TABLE ");
                if *if_exists {
                    out.push_str("IF EXISTS ");
                }
                out.push_str(name);
                out.push_str(";\n");
            }
            Statement::CreateIndex(index) => out.push_str(&lower_create_index(index)),
            Statement::DropIndex { name, if_exists } => {
                out.push_str("DROP INDEX ");
                if *if_exists {
                    out.push_str("IF EXISTS ");
                }
                out.push_str(name);
                out.push_str(";\n");
            }
            Statement::AlterTable(alter) => out.push_str(&lower_alter_table(alter, dialect)),
        }
    }
    out
}

fn lower_create_table(table: &CreateTable, dialect: Dialect) -> String {
    let mut parts: Vec<String> = table
        .columns
        .iter()
        .map(|c| render_column(c, dialect))
        .collect();
    if !table.primary_key.is_empty() {
        parts.push(format!("PRIMARY KEY ({})", table.primary_key.join(", ")));
    }
    for unique in &table.uniques {
        parts.push(format!("UNIQUE ({})", unique.join(", ")));
    }
    let head = if table.if_not_exists {
        "CREATE TABLE IF NOT EXISTS"
    } else {
        "CREATE TABLE"
    };
    format!(
        "{head} {} (\n    {}\n);\n",
        table.name,
        parts.join(",\n    ")
    )
}

/// One column definition. The identity column is the single place the two backends genuinely differ
/// in kind rather than in spelling: SQLite's `INTEGER PRIMARY KEY AUTOINCREMENT` aliases the rowid,
/// Postgres's `BIGSERIAL PRIMARY KEY` owns a sequence. Both give a 64-bit auto-assigned key.
fn render_column(column: &Column, dialect: Dialect) -> String {
    if column.ty == ColumnType::Id {
        return match dialect {
            Dialect::Sqlite => format!("{} INTEGER PRIMARY KEY AUTOINCREMENT", column.name),
            Dialect::Postgres => format!("{} BIGSERIAL PRIMARY KEY", column.name),
        };
    }
    let mut sql = format!("{} {}", column.name, column.ty.sql(dialect));
    if column.primary_key {
        sql.push_str(" PRIMARY KEY");
    }
    if column.not_null {
        sql.push_str(" NOT NULL");
    }
    if let Some(default) = &column.default {
        sql.push_str(" DEFAULT ");
        sql.push_str(&default.sql());
    }
    if column.unique {
        sql.push_str(" UNIQUE");
    }
    if let Some(key) = &column.references {
        sql.push_str(&format!(" REFERENCES {} ({})", key.table, key.column));
        if let Some(action) = key.on_delete {
            sql.push_str(&format!(" ON DELETE {}", action.sql()));
        }
        if let Some(action) = key.on_update {
            sql.push_str(&format!(" ON UPDATE {}", action.sql()));
        }
    }
    sql
}

fn lower_create_index(index: &CreateIndex) -> String {
    let unique = if index.unique { "UNIQUE " } else { "" };
    let guard = if index.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    format!(
        "CREATE {unique}INDEX {guard}{} ON {} ({});\n",
        index.index_name(),
        index.table,
        index.columns.join(", ")
    )
}

fn lower_alter_table(alter: &AlterTable, dialect: Dialect) -> String {
    let mut out = String::new();
    for action in &alter.actions {
        let clause = match action {
            AlterAction::AddColumn(column) => {
                format!("ADD COLUMN {}", render_column(column, dialect))
            }
            AlterAction::DropColumn(name) => format!("DROP COLUMN {name}"),
            AlterAction::RenameColumn { from, to } => format!("RENAME COLUMN {from} TO {to}"),
            AlterAction::RenameTable(name) => format!("RENAME TO {name}"),
        };
        out.push_str(&format!("ALTER TABLE {} {clause};\n", alter.name));
    }
    out
}

/// Parse and lower in one step — the whole DSL pipeline, used by the migration engine and by
/// `Connection.apply_schema`.
pub fn compile(src: &str, dialect: Dialect) -> Result<String, SchemaError> {
    Ok(lower(&parse(src)?, dialect))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite(src: &str) -> String {
        compile(src, Dialect::Sqlite).unwrap()
    }

    fn postgres(src: &str) -> String {
        compile(src, Dialect::Postgres).unwrap()
    }

    fn err(src: &str) -> String {
        parse(src).unwrap_err().to_string()
    }

    const TODOS: &str = r#"
        create_table("todos")
            .id()
            .text("title").not_null()
            .bool("done").default(false)
            .timestamps()
    "#;

    #[test]
    fn the_identity_column_is_the_one_real_dialect_difference() {
        assert!(sqlite(TODOS).contains("id INTEGER PRIMARY KEY AUTOINCREMENT"));
        assert!(postgres(TODOS).contains("id BIGSERIAL PRIMARY KEY"));
    }

    #[test]
    fn the_sketch_lowers_to_both_dialects_in_full() {
        assert_eq!(
            sqlite(TODOS),
            "CREATE TABLE todos (\n    \
             id INTEGER PRIMARY KEY AUTOINCREMENT,\n    \
             title TEXT NOT NULL,\n    \
             done BOOLEAN DEFAULT FALSE,\n    \
             created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,\n    \
             updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP\n\
             );\n"
        );
        assert_eq!(
            postgres(TODOS),
            "CREATE TABLE todos (\n    \
             id BIGSERIAL PRIMARY KEY,\n    \
             title TEXT NOT NULL,\n    \
             done BOOLEAN DEFAULT FALSE,\n    \
             created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,\n    \
             updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP\n\
             );\n"
        );
    }

    #[test]
    fn float_is_the_other_type_that_differs_in_spelling() {
        let src = r#"create_table("m").int("a").float("b").bigint("c").timestamp("d")"#;
        assert!(sqlite(src).contains("b REAL"));
        assert!(postgres(src).contains("b DOUBLE PRECISION"));
        // The rest are spelled identically on both.
        for rendered in [sqlite(src), postgres(src)] {
            assert!(rendered.contains("a INTEGER"), "{rendered}");
            assert!(rendered.contains("c BIGINT"), "{rendered}");
            assert!(rendered.contains("d TIMESTAMP"), "{rendered}");
        }
    }

    #[test]
    fn defaults_render_as_portable_literals() {
        let src = r#"
            create_table("t")
                .text("a").default("it's fine")
                .int("b").default(-3)
                .float("c").default(1.0)
                .bool("d").default(true)
                .timestamp("e").default_now()
        "#;
        let ddl = sqlite(src);
        assert!(ddl.contains("a TEXT DEFAULT 'it''s fine'"), "{ddl}");
        assert!(ddl.contains("b INTEGER DEFAULT -3"), "{ddl}");
        assert!(ddl.contains("c REAL DEFAULT 1.0"), "{ddl}");
        assert!(ddl.contains("d BOOLEAN DEFAULT TRUE"), "{ddl}");
        assert!(
            ddl.contains("e TIMESTAMP DEFAULT CURRENT_TIMESTAMP"),
            "{ddl}"
        );
        // Postgres renders every one of them identically — only types differ.
        assert!(postgres(src).contains("a TEXT DEFAULT 'it''s fine'"));
    }

    #[test]
    fn keys_constraints_and_references_are_spelled_identically() {
        let src = r#"
            create_table("posts")
                .id()
                .bigint("author_id").not_null().references("users", "id").on_delete("cascade")
                .text("slug").unique()
                .if_not_exists()
        "#;
        for ddl in [sqlite(src), postgres(src)] {
            assert!(
                ddl.starts_with("CREATE TABLE IF NOT EXISTS posts ("),
                "{ddl}"
            );
            assert!(
                ddl.contains("author_id BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE"),
                "{ddl}"
            );
            assert!(ddl.contains("slug TEXT UNIQUE"), "{ddl}");
        }
    }

    #[test]
    fn composite_keys_become_table_level_constraints() {
        let src = r#"
            create_table("memberships")
                .bigint("user_id")
                .bigint("group_id")
                .text("role")
                .primary_key("user_id", "group_id")
                .unique("group_id", "role")
        "#;
        let ddl = sqlite(src);
        assert!(ddl.contains("PRIMARY KEY (user_id, group_id)"), "{ddl}");
        assert!(ddl.contains("UNIQUE (group_id, role)"), "{ddl}");
        assert_eq!(ddl, postgres(src), "no dialect difference here");
    }

    #[test]
    fn indexes_derive_a_stable_name_unless_one_is_given() {
        assert_eq!(
            sqlite(r#"create_index("todos").column("done").column("title")"#),
            "CREATE INDEX idx_todos_done_title ON todos (done, title);\n"
        );
        assert_eq!(
            sqlite(
                r#"create_index("todos").columns("a", "b").unique().if_not_exists().name("by_ab")"#
            ),
            "CREATE UNIQUE INDEX IF NOT EXISTS by_ab ON todos (a, b);\n"
        );
        // Both backends spell an index the same way.
        assert_eq!(
            postgres(r#"create_index("todos").column("done")"#),
            "CREATE INDEX idx_todos_done ON todos (done);\n"
        );
    }

    #[test]
    fn drops_are_portable_with_and_without_the_guard() {
        assert_eq!(
            sqlite(r#"drop_table("todos").if_exists()"#),
            "DROP TABLE IF EXISTS todos;\n"
        );
        assert_eq!(sqlite(r#"drop_table("todos")"#), "DROP TABLE todos;\n");
        assert_eq!(
            postgres(r#"drop_index("idx_todos_done").if_exists()"#),
            "DROP INDEX IF EXISTS idx_todos_done;\n"
        );
    }

    #[test]
    fn alter_lowers_one_action_per_statement() {
        let src = r#"
            alter_table("todos")
                .add_text("note")
                .add_bool("archived").not_null().default(false)
                .drop_column("legacy")
                .rename_column("title", "subject")
        "#;
        assert_eq!(
            sqlite(src),
            "ALTER TABLE todos ADD COLUMN note TEXT;\n\
             ALTER TABLE todos ADD COLUMN archived BOOLEAN NOT NULL DEFAULT FALSE;\n\
             ALTER TABLE todos DROP COLUMN legacy;\n\
             ALTER TABLE todos RENAME COLUMN title TO subject;\n"
        );
        assert_eq!(sqlite(src), postgres(src));
        assert_eq!(
            sqlite(r#"alter_table("todos").rename_to("tasks")"#),
            "ALTER TABLE todos RENAME TO tasks;\n"
        );
    }

    #[test]
    fn several_statements_lower_in_source_order() {
        let ddl = sqlite(
            r#"
            create_table("a").int("x")
            create_index("a").column("x")
            drop_table("b").if_exists()
        "#,
        );
        let heads: Vec<&str> = ddl.lines().filter(|l| !l.starts_with("    ")).collect();
        assert_eq!(
            heads,
            vec![
                "CREATE TABLE a (",
                ");",
                "CREATE INDEX idx_a_x ON a (x);",
                "DROP TABLE IF EXISTS b;",
            ]
        );
    }

    #[test]
    fn comments_in_both_styles_are_ignored() {
        let ddl = sqlite(
            r#"
            // a Noeta-style comment
            create_table("t")   -- and a SQL-style one
                .int("x")
        "#,
        );
        assert!(ddl.contains("x INTEGER"), "{ddl}");
    }

    #[test]
    fn an_identifier_that_would_need_quoting_is_refused() {
        let message = err(r#"create_table("my table").int("x")"#);
        assert!(message.contains("not a portable identifier"), "{message}");
        assert!(message.contains("raw `.sql`"), "{message}");
        // …and so is one that could inject DDL.
        assert!(err(r#"drop_table("t; DROP TABLE users")"#).contains("not a portable identifier"));
    }

    #[test]
    fn an_unknown_statement_points_at_raw_sql() {
        let message = err(r#"create_view("v").text("x")"#);
        assert!(
            message.contains("unknown schema statement `create_view`"),
            "{message}"
        );
        assert!(message.contains("raw `.sql` migration"), "{message}");
    }

    #[test]
    fn errors_name_the_line_they_are_on() {
        let message = err("create_table(\"a\")\n    .int(\"x\")\n    .frobnicate()\n");
        assert!(message.starts_with("line 3:"), "{message}");
        assert!(message.contains("frobnicate"), "{message}");
    }

    #[test]
    fn a_modifier_with_no_column_before_it_is_refused() {
        let message = err(r#"create_table("t").not_null()"#);
        assert!(
            message.contains("no column has been declared yet"),
            "{message}"
        );
    }

    #[test]
    fn contradictory_primary_keys_are_refused() {
        let message = err(r#"create_table("t").id().int("a").primary_key("a")"#);
        assert!(message.contains("pick one"), "{message}");
        let message = err(r#"create_table("t").int("a").primary_key().int("b").primary_key()"#);
        assert!(message.contains("composite"), "{message}");
    }

    #[test]
    fn an_empty_table_or_index_is_refused() {
        assert!(err(r#"create_table("t")"#).contains("declares no columns"));
        assert!(err(r#"create_index("t")"#).contains("names no column"));
        assert!(err(r#"alter_table("t")"#).contains("names no change"));
    }

    #[test]
    fn the_unportable_add_column_shapes_are_refused_with_the_alternative() {
        // SQLite cannot add a NOT NULL column without a default…
        let message = err(r#"alter_table("t").add_text("a").not_null()"#);
        assert!(message.contains("needs a `default(…)`"), "{message}");
        // …nor a UNIQUE one (the portable answer is a unique index)…
        let message = err(r#"alter_table("t").add_text("a").unique()"#);
        assert!(message.contains("create_index(\"t\")"), "{message}");
        // …nor a non-constant default…
        let message = err(r#"alter_table("t").add_timestamp("a").default_now()"#);
        assert!(message.contains("constant default"), "{message}");
        // …nor an identity column.
        let message = err(r#"alter_table("t").add_id()"#);
        assert!(
            message.contains("cannot be added to an existing table"),
            "{message}"
        );
    }

    #[test]
    fn a_rename_cannot_share_its_chain() {
        let message = err(r#"alter_table("t").rename_to("u").add_text("a")"#);
        assert!(message.contains("only action"), "{message}");
    }

    #[test]
    fn a_bad_referential_action_lists_the_portable_ones() {
        let message =
            err(r#"create_table("t").bigint("a").references("u", "id").on_delete("explode")"#);
        assert!(message.contains("set_null"), "{message}");
        // The spelling is forgiving about case and separator.
        assert!(
            sqlite(r#"create_table("t").bigint("a").references("u", "id").on_delete("SET-NULL")"#)
                .contains("ON DELETE SET NULL")
        );
    }

    #[test]
    fn on_delete_without_references_is_refused() {
        let message = err(r#"create_table("t").bigint("a").on_delete("cascade")"#);
        assert!(
            message.contains("needs a `.references(table, column)`"),
            "{message}"
        );
    }

    #[test]
    fn malformed_source_is_reported_not_panicked() {
        assert!(err("create_table(\"t\"").contains("never closed"));
        assert!(err("create_table").contains("must be called"));
        assert!(err("create_table(\"t\").int(x)").contains("literal arguments only"));
        assert!(err("create_table(\"t").contains("unterminated string"));
        assert!(err("create_table(\"t\") $").contains("unexpected character"));
    }

    #[test]
    fn escapes_and_negative_numbers_lex() {
        let ddl =
            sqlite(r#"create_table("t").text("a").default("say \"hi\"").int("b").default(-42)"#);
        assert!(ddl.contains(r#"DEFAULT 'say "hi"'"#), "{ddl}");
        assert!(ddl.contains("DEFAULT -42"), "{ddl}");
    }

    #[test]
    fn an_id_column_can_be_renamed() {
        assert!(sqlite(r#"create_table("t").id("uid")"#).contains("uid INTEGER PRIMARY KEY"));
        assert!(postgres(r#"create_table("t").id("uid")"#).contains("uid BIGSERIAL PRIMARY KEY"));
    }

    #[test]
    fn an_empty_source_lowers_to_nothing() {
        assert_eq!(parse("").unwrap(), Vec::new());
        assert_eq!(sqlite("// nothing but a comment\n"), "");
    }
}
