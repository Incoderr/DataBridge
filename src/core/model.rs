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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DbType {
    PostgreSql,
    MySql,
    Sqlite,
    MongoDb,
    Redis,
}

impl DbType {
    pub fn label(&self) -> &'static str {
        match self {
            DbType::PostgreSql => "PostgreSQL",
            DbType::MySql => "MySQL",
            DbType::Sqlite => "SQLite",
            DbType::MongoDb => "MongoDB",
            DbType::Redis => "Redis",
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
        }
    }

    pub fn default_port(&self) -> u16 {
        match self {
            DbType::PostgreSql => 5432,
            DbType::MySql => 3306,
            DbType::Sqlite => 0,
            DbType::MongoDb => 27017,
            DbType::Redis => 6379,
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
#[derive(Debug, Clone)]
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
