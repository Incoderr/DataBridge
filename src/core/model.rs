//! Domain model shared by the UI and every database driver.
//!
//! These types form the future `DbSession` contract: each driver maps its
//! dialect rows into [`Value`] and its introspection into [`SchemaMeta`] /
//! [`TableMeta`]. The mock in `core::mock` generates data from these types
//! deterministically.

// ---------- Types (the future backend contract) ----------

use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connected,
    Disconnected,
    Error,
    #[allow(dead_code)]
    Connecting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DbType {
    PostgreSql,
    MySql,
    Sqlite,
    MongoDb,
    Redis,
    /// Stage-4 placeholders: listed on the wizard's Choose screen and
    /// served by the deterministic mock until their drivers land.
    MariaDb,
    ClickHouse,
    Cassandra,
    ScyllaDb,
}

impl DbType {
    pub fn label(&self) -> &'static str {
        match self {
            DbType::PostgreSql => "PostgreSQL",
            DbType::MySql => "MySQL",
            DbType::Sqlite => "SQLite",
            DbType::MongoDb => "MongoDB",
            DbType::Redis => "Redis",
            DbType::MariaDb => "MariaDB",
            DbType::ClickHouse => "ClickHouse",
            DbType::Cassandra => "Cassandra",
            DbType::ScyllaDb => "ScyllaDB",
        }
    }

    /// simple-icons brand color, e.g. `0x4169E1`.
    pub fn brand_color(&self) -> u32 {
        match self {
            DbType::PostgreSql => 0x4169E1,
            DbType::MySql => 0x4479A1,
            DbType::Sqlite => 0x003B57,
            DbType::MongoDb => 0x47A248,
            DbType::Redis => 0xFF4438,
            DbType::MariaDb => 0x003545,
            DbType::ClickHouse => 0xFFCC01,
            DbType::Cassandra => 0x1287B1,
            DbType::ScyllaDb => 0x6CD5E7,
        }
    }

    /// Asset path of the brand glyph.
    pub fn brand_icon(&self) -> &'static str {
        match self {
            DbType::PostgreSql => "brand/postgresql.svg",
            DbType::MySql => "brand/mysql.svg",
            DbType::Sqlite => "brand/sqlite.svg",
            DbType::MongoDb => "brand/mongodb.svg",
            DbType::Redis => "brand/redis.svg",
            DbType::MariaDb => "brand/mariadb.svg",
            DbType::ClickHouse => "brand/clickhouse.svg",
            DbType::Cassandra => "brand/apachecassandra.svg",
            DbType::ScyllaDb => "brand/scylladb.svg",
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            DbType::PostgreSql => 5432,
            DbType::MySql => 3306,
            DbType::Sqlite => 0,
            DbType::MongoDb => 27017,
            DbType::Redis => 6379,
            // ClickHouse native protocol; the HTTP interface is 8123.
            DbType::MariaDb => 3306,
            DbType::ClickHouse => 9000,
            DbType::Cassandra => 9042,
            DbType::ScyllaDb => 9042,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DbConnection {
    pub id: String,
    pub name: String,
    pub db_type: DbType,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    /// Kept in memory only; Stage 3 moves it to the OS keyring.
    pub password: String,
    /// When set, drivers connect via this URL (parsed natively by sqlx,
    /// preserving query options like `sslmode`). The structured fields are
    /// display-only, best-effort extracted from the URL.
    pub url: Option<String>,
    pub status: ConnectionStatus,
    /// how many ms the (mock) ping took
    pub latency_ms: u32,
}

/// Connection-target identity: two recents entries describe the same
/// connection when their targets match (ids, names and credentials are
/// irrelevant). URL mode parses the URL — so the same server matches
/// with or without a password — while structured-fields mode compares
/// the fields. This is the dedupe key of the recents list: repeated
/// connects of one server must reuse its entry, not stack duplicates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetKey {
    pub db_type: DbType,
    /// URL scheme when the connection was created in URL mode, e.g.
    /// `postgres`; `None` for structured-fields connections.
    pub scheme: Option<String>,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
}

impl TargetKey {
    /// Build from raw parts — either an in-memory [`DbConnection`] or
    /// its stored JSON copy (whose `url` is password-stripped). A URL
    /// overrides the structured fields; a port missing from the URL
    /// falls back to the passed-in one (already normalized by the
    /// wizard's validation).
    pub fn build(
        db_type: DbType,
        url: Option<&str>,
        host: &str,
        port: u16,
        database: &str,
        user: &str,
    ) -> Self {
        let Some(url) = url else {
            return Self {
                db_type,
                scheme: None,
                host: host.into(),
                port,
                database: database.into(),
                user: user.into(),
            };
        };
        let scheme = url.split_once("://").map(|(s, _)| s.to_string());
        match parse_db_url(url) {
            Some(x) => Self {
                db_type,
                scheme,
                host: x.host,
                port: if x.port > 0 { x.port } else { port },
                database: x.database,
                user: x.user,
            },
            None => Self {
                db_type,
                scheme,
                host: host.into(),
                port,
                database: database.into(),
                user: user.into(),
            },
        }
    }
}

impl DbConnection {
    /// The dedupe key of this connection (see [`TargetKey`]).
    pub fn target_key(&self) -> TargetKey {
        TargetKey::build(
            self.db_type,
            self.url.as_deref(),
            &self.host,
            self.port,
            &self.database,
            &self.user,
        )
    }
}

/// Display-oriented parse of `scheme://[user[:password]@]host[:port][/db][?…]`.
/// The drivers re-parse with sqlx (keeping query options), so this only
/// feeds the host/port/user labels; percent-encoding is left as-is.
pub struct UrlParts {
    pub user: String,
    pub host: String,
    pub port: u16,
    pub database: String,
}

pub fn parse_db_url(url: &str) -> Option<UrlParts> {
    let rest = url.split_once("://")?.1;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(ix) => (&rest[..ix], &rest[ix..]),
        None => (rest, ""),
    };
    let (user_part, host_part) = match authority.rsplit_once('@') {
        Some((u, h)) => (u, h),
        None => ("", authority),
    };
    let user = user_part.split_once(':').map(|(u, _)| u).unwrap_or(user_part);
    let (host, port) = if let Some(close) = host_part.find(']') {
        // IPv6 literal: the port, if any, follows the closing bracket.
        let (h, tail) = host_part.split_at(close + 1);
        (h, tail.strip_prefix(':').and_then(|s| s.parse().ok()).unwrap_or(0))
    } else {
        match host_part.rsplit_once(':') {
            Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                (h, p.parse().unwrap_or(0))
            }
            Some((h, "")) => (h, 0),
            _ => (host_part, 0),
        }
    };
    let database = path
        .trim_start_matches('/')
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    Some(UrlParts {
        user: user.to_string(),
        host: host.to_string(),
        port,
        database: database.to_string(),
    })
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Connection,
    Schema,
    Table,
    View,
    Function,
    Procedure,
}

impl NodeKind {
    /// The `DROP <keyword>` for objects the tree's context menu can drop;
    /// `None` for everything else (routines stay out of reach).
    pub fn drop_keyword(self) -> Option<&'static str> {
        match self {
            NodeKind::Table => Some("TABLE"),
            NodeKind::View => Some("VIEW"),
            _ => None,
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Int,
    Float,
    Text,
    Json,
    Bool,
    Date,
    Uuid,
    Binary,
}

#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    /// Dialect type label, e.g. "varchar(255)", "numeric(10,2)", "INTEGER".
    pub type_: String,
    pub nullable: bool,
    pub primary_key: bool,
    /// Auto-increment / identity / serial: generated by the database on
    /// insert — the editor leaves it empty and omits it from INSERTs.
    pub auto_increment: bool,
    /// The column has a DEFAULT (or is identity): empty input on insert
    /// falls back to it instead of being reported as required.
    pub has_default: bool,
    pub kind: ColumnKind,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TableMeta {
    pub name: String,
    pub schema: String,
    pub kind: NodeKind,
    pub columns: Vec<ColumnMeta>,
    /// total rows (mock)
    pub row_count: u64,
}

/// A cell value in a row. Rows are ordered by their table's columns.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// Raw binary (BLOB / BYTEA); rendered as a truncated hex preview.
    Bytes(Vec<u8>),
}

impl Value {
    /// The raw string form (as `String(v)` in the web version).
    pub fn display(&self) -> String {
        match self {
            Value::Null => "null".into(),
            Value::Int(v) => v.to_string(),
            Value::Float(v) => format!("{v:.2}"),
            Value::Bool(v) => v.to_string(),
            Value::Str(s) => s.clone(),
            Value::Bytes(b) => {
                let head: String = b.iter().take(32).map(|x| format!("{x:02x}")).collect();
                if b.len() > 32 {
                    format!("0x{head}… ({} bytes)", b.len())
                } else {
                    format!("0x{head}")
                }
            }
        }
    }

    /// The editable text form for the simplified editor: unlike
    /// [`Self::display`], floats keep full precision and NULL is empty.
    pub fn edit_string(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Float(v) => format!("{v}"),
            other => other.display(),
        }
    }
}

pub type Row = Vec<Value>;

#[derive(Debug, Clone)]
pub struct SchemaMeta {
    pub name: String,
    pub tables: Vec<Arc<TableMeta>>,
    pub views: Vec<Arc<TableMeta>>,
    pub functions: Vec<Arc<TableMeta>>,
}

/// One page of fetched rows; the session reports how long the query took.
#[allow(dead_code)]
pub struct Page {
    pub rows: Vec<Row>,
    pub total: u64,
    pub truncated: bool,
    pub elapsed_ms: u32,
}

/// Result of an arbitrary SQL statement (Stage 5): either a result set
/// (`columns` + `rows`) or a mutation report (`rows_affected` > 0).
pub struct QueryPage {
    /// Best-effort column metadata of the result set; empty for mutations.
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Row>,
    /// Rows touched by a mutation (INSERT/UPDATE/DELETE/DDL reports 0).
    pub rows_affected: u64,
    pub truncated: bool,
    pub elapsed_ms: u32,
}

impl QueryPage {
    /// True when the statement returned a result set.
    #[allow(dead_code)]
    pub fn is_result_set(&self) -> bool {
        !self.columns.is_empty() || !self.rows.is_empty()
    }
}

/// One typed cell for the write path (simplified editor): the column
/// name, its kind (typed NULL binding + validation) and the new value.
#[derive(Debug, Clone)]
pub struct CellChange {
    pub column: String,
    pub kind: ColumnKind,
    pub value: Value,
}

impl CellChange {
    pub fn new(column: impl Into<String>, kind: ColumnKind, value: Value) -> Self {
        Self {
            column: column.into(),
            kind,
            value,
        }
    }
}

/// The statement's first keyword, lowercased — leading whitespace, `(`
/// and SQL comments (`-- …`, `/* … */`) skipped. Empty when there is none.
pub fn first_keyword(sql: &str) -> String {
    let mut rest = sql;
    loop {
        rest = rest.trim_start_matches(|c: char| c == '(' || c.is_whitespace());
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
    }
    rest.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Statement sniff for the query editor: row-returning statements go
/// through the fetch path (result rows), everything else through the
/// execute path (rows affected). Row-returning = a reading first keyword,
/// or a DML statement with a `RETURNING` clause. Multi-statement strings
/// are not supported by design — one editor run is one statement.
pub fn is_select_like(sql: &str) -> bool {
    match first_keyword(sql).as_str() {
        "select" | "with" | "show" | "explain" | "table" | "values" | "pragma" | "describe"
        | "desc" => true,
        "insert" | "update" | "delete" | "replace" => sql
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .any(|w| w.eq_ignore_ascii_case("returning")),
        _ => false,
    }
}

/// Parse the simplified editor's input for one column into a typed
/// [`Value`]. Empty input becomes NULL (only when the column allows it);
/// numeric and boolean columns parse strictly; binary columns are not
/// editable (the editor keeps them read-only).
pub fn parse_cell(c: &ColumnMeta, raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return if c.nullable {
            Ok(Value::Null)
        } else {
            Err(format!("{} is required", c.name))
        };
    }
    match c.kind {
        ColumnKind::Int => raw
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| format!("{} expects an integer", c.name)),
        ColumnKind::Float => raw
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| format!("{} expects a number", c.name)),
        ColumnKind::Bool => match raw.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(Value::Bool(true)),
            "false" | "0" | "no" => Ok(Value::Bool(false)),
            _ => Err(format!("{} expects true/false", c.name)),
        },
        ColumnKind::Binary => Err(format!("{} (binary) is read-only", c.name)),
        _ => Ok(Value::Str(raw.to_string())),
    }
}

/// Result of a liveness probe.
#[allow(dead_code)]
pub struct Ping {
    pub status: ConnectionStatus,
    pub latency_ms: u32,
}

/// Identifies a table/view/routine inside a connection.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct TableRef {
    pub schema: String,
    pub name: String,
    pub kind: NodeKind,
}

impl TableRef {
    pub fn of(meta: &TableMeta) -> Self {
        Self {
            schema: meta.schema.clone(),
            name: meta.name.clone(),
            kind: meta.kind,
        }
    }
}

/// A foreign-key edge between two tables of one schema — the input of the
/// schema diagram (relationships view).
#[derive(Debug, Clone)]
pub struct Relation {
    /// The table that declares the foreign key.
    pub from_table: String,
    pub from_column: String,
    /// The referenced (usually primary-key) table.
    pub to_table: String,
    pub to_column: String,
}

/// Result metadata status line: `N rows fetched in X ms`.
pub fn status_line(elapsed_ms: u32, row_count: usize, total: u64) -> String {
    let plural = if row_count == 1 { "" } else { "s" };
    if total > row_count as u64 {
        format!(
            "{row_count} row{plural} fetched in {elapsed_ms} ms (truncated; {} total)",
            group_digits(total)
        )
    } else {
        format!("{row_count} row{plural} fetched in {elapsed_ms} ms")
    }
}

/// `Number.toLocaleString("en-US")` equivalent: 24812 -> "24,812".
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_keyword_skips_comments_and_parens() {
        assert_eq!(first_keyword("  -- note\n/* x */ (SELECT 1)"), "select");
        assert_eq!(first_keyword("/* unterminated"), "");
    }

    #[test]
    fn select_like_sniff() {
        assert!(is_select_like("PRAGMA table_info(t)"));
        assert!(is_select_like("VALUES (1), (2)"));
        assert!(is_select_like("insert into t(a) values (1) RETURNING id"));
        assert!(!is_select_like("insert into t(returning_flag) values (1)"));
        assert!(!is_select_like("CREATE TABLE t (id int)"));
        assert!(!is_select_like("tablespace_thing"));
    }
}
