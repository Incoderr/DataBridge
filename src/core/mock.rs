//! Deterministic mock session - a faithful port of the web app's
//! `src/lib/mockData.ts`. The same (connection, schema, table, offset,
//! limit) always produces the same rows. Serves as the built-in demo
//! driver (dev/demo without a live DB) and seeds the wizard's connection
//! list until Stage 3 adds persistence.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;

use super::error::DbError;
use super::model::{
    ColumnKind, ColumnMeta, ConnectionStatus, DbConnection, DbType, NodeKind, Page, Ping,
    Relation, SchemaMeta, TableMeta, TableRef, Value,
};
use super::session::DbSession;

// ---------- Seeds ----------

pub fn seed_connections() -> Vec<DbConnection> {
    vec![
        DbConnection {
            id: "pg-prod".into(),
            name: "production_postgres".into(),
            db_type: DbType::PostgreSql,
            host: "db.internal.acme.io".into(),
            port: 5432,
            database: "acme_core".into(),
            user: "app_reader".into(),
            password: String::new(),
            url: None,
            status: ConnectionStatus::Connected,
            latency_ms: 18,
        },
        DbConnection {
            id: "mysql-analytics".into(),
            name: "analytics_mysql".into(),
            db_type: DbType::MySql,
            host: "analytics.acme.io".into(),
            port: 3306,
            database: "warehouse".into(),
            user: "bi_user".into(),
            password: String::new(),
            url: None,
            status: ConnectionStatus::Connected,
            latency_ms: 41,
        },
        DbConnection {
            id: "sqlite-local".into(),
            name: "local_books.sqlite".into(),
            db_type: DbType::Sqlite,
            host: "localhost".into(),
            port: 0,
            database: "/home/user/data/books.sqlite".into(),
            user: String::new(),
            password: String::new(),
            url: None,
            status: ConnectionStatus::Connected,
            latency_ms: 2,
        },
        DbConnection {
            id: "staging-pg".into(),
            name: "staging_pg (offline)".into(),
            db_type: DbType::PostgreSql,
            host: "staging-db.acme.io".into(),
            port: 5432,
            database: "acme_staging".into(),
            user: "staging_reader".into(),
            password: String::new(),
            url: None,
            status: ConnectionStatus::Error,
            latency_ms: 0,
        },
        DbConnection {
            id: "unauth-mongo".into(),
            name: "logs_mongo (auth)".into(),
            db_type: DbType::MongoDb,
            host: "mongo.acme.io".into(),
            port: 27017,
            database: "logs".into(),
            user: "logger".into(),
            password: String::new(),
            url: None,
            status: ConnectionStatus::Disconnected,
            latency_ms: 0,
        },
    ]
}

struct ConnectionTree {
    connection_id: String,
    schemas: Vec<SchemaMeta>,
}

fn col(
    name: &str,
    type_: &str,
    kind: ColumnKind,
    nullable: bool,
    primary_key: bool,
) -> ColumnMeta {
    ColumnMeta {
        name: name.into(),
        type_: type_.into(),
        kind,
        nullable,
        primary_key,
    }
}

fn table(schema: &str, name: &str, columns: Vec<ColumnMeta>, row_count: u64) -> Arc<TableMeta> {
    Arc::new(TableMeta {
        name: name.into(),
        schema: schema.into(),
        kind: NodeKind::Table,
        columns,
        row_count,
    })
}

fn view(schema: &str, name: &str, columns: Vec<ColumnMeta>, row_count: u64) -> Arc<TableMeta> {
    let mut meta = (*table(schema, name, columns, row_count)).clone();
    meta.kind = NodeKind::View;
    Arc::new(meta)
}

fn function(schema: &str, name: &str, columns: Vec<ColumnMeta>) -> Arc<TableMeta> {
    let mut meta = (*table(schema, name, columns, 0)).clone();
    meta.kind = NodeKind::Function;
    meta.row_count = 0;
    Arc::new(meta)
}

/// The static schema trees per connection id.
fn trees() -> &'static Vec<ConnectionTree> {
    static TREES: OnceLock<Vec<ConnectionTree>> = OnceLock::new();
    TREES.get_or_init(|| {
        vec![
            ConnectionTree {
                connection_id: "pg-prod".into(),
                schemas: vec![
                    SchemaMeta {
                        name: "public".into(),
                        tables: vec![
                            table(
                                "public",
                                "users",
                                vec![
                                    col("id", "int8", ColumnKind::Int, false, true),
                                    col("email", "varchar(255)", ColumnKind::Text, false, false),
                                    col("name", "varchar(120)", ColumnKind::Text, true, false),
                                    col("is_active", "boolean", ColumnKind::Bool, false, false),
                                    col("created_at", "timestamptz", ColumnKind::Date, false, false),
                                    col("balance", "numeric(12,2)", ColumnKind::Float, true, false),
                                ],
                                24_812,
                            ),
                            table(
                                "public",
                                "orders",
                                vec![
                                    col("id", "int8", ColumnKind::Int, false, true),
                                    col("user_id", "int8", ColumnKind::Int, false, false),
                                    col("total", "numeric(10,2)", ColumnKind::Float, false, false),
                                    col("status", "varchar(20)", ColumnKind::Text, false, false),
                                    col("placed_at", "timestamptz", ColumnKind::Date, false, false),
                                ],
                                158_204,
                            ),
                            table(
                                "public",
                                "products",
                                vec![
                                    col("id", "int8", ColumnKind::Int, false, true),
                                    col("sku", "varchar(48)", ColumnKind::Text, false, false),
                                    col("title", "varchar(200)", ColumnKind::Text, false, false),
                                    col("price", "numeric(10,2)", ColumnKind::Float, false, false),
                                    col("in_stock", "int4", ColumnKind::Int, false, false),
                                    col("attributes", "jsonb", ColumnKind::Json, true, false),
                                ],
                                3_410,
                            ),
                            table(
                                "public",
                                "sessions",
                                vec![
                                    col("id", "uuid", ColumnKind::Uuid, false, true),
                                    col("user_id", "int8", ColumnKind::Int, false, false),
                                    col("token", "varchar(64)", ColumnKind::Text, false, false),
                                    col("expires_at", "timestamptz", ColumnKind::Date, false, false),
                                ],
                                9_221,
                            ),
                        ],
                        views: vec![view(
                            "public",
                            "user_summary",
                            vec![
                                col("user_id", "int8", ColumnKind::Int, false, false),
                                col("email", "varchar(255)", ColumnKind::Text, false, false),
                                col("order_count", "int8", ColumnKind::Int, false, false),
                                col("lifetime_value", "numeric(14,2)", ColumnKind::Float, false, false),
                            ],
                            8_142,
                        )],
                        functions: vec![function(
                            "public",
                            "calculate_tax(amount numeric)",
                            vec![col("amount", "numeric", ColumnKind::Float, false, false)],
                        )],
                    },
                    SchemaMeta {
                        name: "auth".into(),
                        tables: vec![
                            table(
                                "auth",
                                "credentials",
                                vec![
                                    col("user_id", "int8", ColumnKind::Int, false, true),
                                    col("provider", "varchar(40)", ColumnKind::Text, false, false),
                                    col("provider_uid", "varchar(255)", ColumnKind::Text, false, false),
                                    col("password_hash", "varchar(128)", ColumnKind::Text, true, false),
                                    col("last_login", "timestamptz", ColumnKind::Date, true, false),
                                ],
                                24_812,
                            ),
                            table(
                                "auth",
                                "audit_log",
                                vec![
                                    col("id", "int8", ColumnKind::Int, false, true),
                                    col("actor", "varchar(120)", ColumnKind::Text, false, false),
                                    col("action", "varchar(60)", ColumnKind::Text, false, false),
                                    col("metadata", "jsonb", ColumnKind::Json, true, false),
                                    col("at", "timestamptz", ColumnKind::Date, false, false),
                                ],
                                312_045,
                            ),
                        ],
                        views: vec![],
                        functions: vec![],
                    },
                ],
            },
            ConnectionTree {
                connection_id: "mysql-analytics".into(),
                schemas: vec![SchemaMeta {
                    name: "warehouse".into(),
                    tables: vec![
                        table(
                            "warehouse",
                            "events",
                            vec![
                                col("id", "bigint", ColumnKind::Int, false, true),
                                col("event_type", "varchar(40)", ColumnKind::Text, false, false),
                                col("user_id", "bigint", ColumnKind::Int, true, false),
                                col("payload", "json", ColumnKind::Json, true, false),
                                col("ts", "datetime", ColumnKind::Date, false, false),
                            ],
                            1_028_221,
                        ),
                        table(
                            "warehouse",
                            "funnel_steps",
                            vec![
                                col("session_id", "varchar(64)", ColumnKind::Text, false, true),
                                col("step", "int", ColumnKind::Int, false, false),
                                col("value", "decimal(10,4)", ColumnKind::Float, true, false),
                                col("ts", "datetime", ColumnKind::Date, false, false),
                            ],
                            532_810,
                        ),
                    ],
                    views: vec![view(
                        "warehouse",
                        "daily_events",
                        vec![
                            col("day", "date", ColumnKind::Date, false, false),
                            col("event_count", "bigint", ColumnKind::Int, false, false),
                            col("unique_users", "bigint", ColumnKind::Int, false, false),
                        ],
                        365,
                    )],
                    functions: vec![],
                }],
            },
            ConnectionTree {
                connection_id: "sqlite-local".into(),
                schemas: vec![SchemaMeta {
                    name: "main".into(),
                    tables: vec![
                        table(
                            "main",
                            "books",
                            vec![
                                col("id", "INTEGER", ColumnKind::Int, false, true),
                                col("title", "TEXT", ColumnKind::Text, false, false),
                                col("author", "TEXT", ColumnKind::Text, true, false),
                                col("year", "INTEGER", ColumnKind::Int, true, false),
                                col("rating", "REAL", ColumnKind::Float, true, false),
                                col("tags", "TEXT", ColumnKind::Text, true, false),
                            ],
                            1_204,
                        ),
                        table(
                            "main",
                            "authors",
                            vec![
                                col("id", "INTEGER", ColumnKind::Int, false, true),
                                col("name", "TEXT", ColumnKind::Text, false, false),
                                col("country", "TEXT", ColumnKind::Text, true, false),
                                col("birth_year", "INTEGER", ColumnKind::Int, true, false),
                            ],
                            412,
                        ),
                    ],
                    views: vec![],
                    functions: vec![],
                }],
            },
            // staging-pg and unauth-mongo: unreachable / auth needed -> empty
            ConnectionTree {
                connection_id: "staging-pg".into(),
                schemas: vec![],
            },
            ConnectionTree {
                connection_id: "unauth-mongo".into(),
                schemas: vec![],
            },
        ]
    })
}

fn tree_for(conn: &DbConnection) -> Option<&'static ConnectionTree> {
    trees()
        .iter()
        .find(|t| t.connection_id == conn.id)
        .or_else(|| match conn.db_type {
            DbType::PostgreSql => trees().iter().find(|t| t.connection_id == "pg-prod"),
            DbType::MySql => trees().iter().find(|t| t.connection_id == "mysql-analytics"),
            DbType::Sqlite => trees().iter().find(|t| t.connection_id == "sqlite-local"),
            DbType::MongoDb => trees().iter().find(|t| t.connection_id == "unauth-mongo"),
            DbType::Redis => trees().iter().find(|t| t.connection_id == "staging-pg"),
        })
}

fn find_meta(conn: &DbConnection, schema: &str, name: &str) -> Option<Arc<TableMeta>> {
    let tree = tree_for(conn)?;
    let sch = tree.schemas.iter().find(|s| s.name == schema)?;
    sch.tables
        .iter()
        .chain(sch.views.iter())
        .chain(sch.functions.iter())
        .find(|t| t.name == name)
        .cloned()
}

// ---------- Deterministic record generator ----------

fn md5(s: &str) -> i32 {
    let mut h: i32 = 0;
    for c in s.chars() {
        h = h.wrapping_mul(31).wrapping_add(c as i32);
    }
    h
}

fn pick<'a>(arr: &'a [&'a str], seed: i32) -> &'a str {
    arr[(seed.unsigned_abs() as usize) % arr.len()]
}

fn pad(n: u64, w: usize) -> String {
    format!("{n:0width$}", width = w)
}

/// civil-from-days (Howard Hinnant's algorithm); epoch is 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

const EMAILS: &[&str] = &[
    "runner@acme.io",
    "maria.d@example.com",
    "freddie@bies.eu",
    "lu@pp.dev",
    "alex@host.ru",
    "kai@null.cloud",
];
const NAMES: &[&str] = &[
    "Ada Lovelace",
    "Alan Turing",
    "Edsger Dijkstra",
    "Grace Hopper",
    "Knuth",
    "Guido",
    "Linus",
];
const STATUSES: &[&str] = &["open", "paid", "shipped", "refunded", "cancelled", "pending"];
const TITLES: &[&str] = &[
    "The Pragmatic Programmer",
    "Refactoring",
    "Clean Code",
    "DDD",
    "The Phoenix Project",
    "PostgreSQL Up & Running",
];
const AUTHORS: &[&str] = &["Sandi Metz", "Robert Martin", "Kathy Sierra", "Brian Goetz"];
const COUNTRIES: &[&str] = &["RU", "US", "DE", "JP", "BR", "FR"];
const ACTIONS: &[&str] = &["login", "logout", "update", "create", "delete", "refresh"];
const ACTORS: &[&str] = &["app_reader", "staging_reader", "cron_worker", "frontend"];
const PROVIDERS: &[&str] = &["google", "github", "email", "apple"];
const EVENT_TYPES: &[&str] = &["click", "view", "purchase", "signup", "error", "page_view"];

fn gen_value(c: &ColumnMeta, row_seed: i64) -> Value {
    let s = md5(&format!("{}:{}", c.name, row_seed));
    let abs = s.unsigned_abs() as u64;
    match c.kind {
        ColumnKind::Int => Value::Int((abs % 100_000) as i64),
        ColumnKind::Float => Value::Float((abs % 100_000) as f64 / 100.0),
        ColumnKind::Bool => Value::Bool(abs % 2 == 1),
        ColumnKind::Date => {
            let days = (abs % 1800) as i64;
            let (y, m, d) = civil_from_days(days + 18_262); // 1970-01-01 + 18262 = 2020-01-01
            let hh = (abs >> 4) % 24;
            let mm = (abs >> 8) % 60;
            let ss = (abs >> 12) % 60;
            Value::Str(format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}"))
        }
        ColumnKind::Uuid => {
            let b = |shift: u32, mask: u64, w: usize| pad((abs >> shift) & mask, w);
            Value::Str(format!(
                "{}{}{}-{}-{}-{}-{}{}",
                b(0, 0xff, 2),
                b(8, 0xff, 2),
                b(16, 0xff, 2),
                b(4, 0xf, 1),
                b(7, 0xff, 2),
                b(12, 0xff, 2),
                b(2, 0xffff, 4),
                b(18, 0xffff, 4),
            ))
        }
        ColumnKind::Json => Value::Str(format!(
            "{{\"k1\":\"{}\",\"n\":{},\"ok\":{}}}",
            pick(&["alpha", "beta", "gamma"], s),
            abs % 100,
            abs % 2 == 1
        )),
        ColumnKind::Binary => Value::Str(format!("0x{}{}", pad(abs & 0xff, 2), pad((abs >> 8) & 0xff, 2))),
        ColumnKind::Text => {
            let v: String = match c.name.as_str() {
                "email" => pick(EMAILS, s).to_string(),
                "name" => pick(NAMES, s).to_string(),
                "status" => pick(STATUSES, s).to_string(),
                "title" => pick(TITLES, s).to_string(),
                "sku" => pick(
                    &[&*format!("SKU-{}", pad(abs % 9999, 4)), &*format!("PRD-{}", pad((abs >> 8) % 9999, 4))],
                    s,
                )
                .to_string(),
                "author" => pick(AUTHORS, s).to_string(),
                "country" => pick(COUNTRIES, s).to_string(),
                "action" => pick(ACTIONS, s).to_string(),
                "actor" => pick(ACTORS, s).to_string(),
                "provider" => pick(PROVIDERS, s).to_string(),
                "provider_uid" => format!("{}{}", pick(&["gid:", "ghid:", "email:", "aid:"], s), abs),
                "token" => format!("tok-{}{}", pad(abs, 6), pad((abs >> 8) & 0xffffff, 6)),
                "event_type" => pick(EVENT_TYPES, s).to_string(),
                "password_hash" => format!("$2b$12${}{}", pad(abs, 6), pad((abs >> 8) & 0xffff, 4)),
                _ => format!("{}-{}", c.name, abs),
            };
            Value::Str(v)
        }
    }
}

/// Generates up to `limit` deterministic rows. Same (table, offset, limit)
/// => same rows. The latency sleep lives in the session future below.
fn page_for(t: &TableMeta, latency_ms: u32, offset: u64, limit: u64) -> Page {
    let mut rows = Vec::new();
    let mut i = 0u64;
    while i < limit && offset + i < t.row_count {
        let row_id = offset + i;
        rows.push(t.columns.iter().map(|c| gen_value(c, row_id as i64)).collect());
        i += 1;
    }
    Page {
        truncated: t.row_count > limit,
        elapsed_ms: latency_ms,
        total: t.row_count,
        rows,
    }
}

// ---------- MockSession: the built-in demo driver ----------

pub struct MockSession {
    conn: DbConnection,
}

impl MockSession {
    pub fn new(conn: &DbConnection) -> Self {
        Self { conn: conn.clone() }
    }

    /// Simulated per-connection query latency (2..120 ms).
    fn latency_ms(&self) -> u32 {
        self.conn.latency_ms.clamp(2, 120)
    }
}

#[async_trait]
impl DbSession for MockSession {
    async fn ping(&self) -> Result<Ping, DbError> {
        let ms = self.latency_ms();
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        Ok(Ping {
            status: ConnectionStatus::Connected,
            latency_ms: ms,
        })
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaMeta>, DbError> {
        Ok(tree_for(&self.conn)
            .map(|t| t.schemas.clone())
            .unwrap_or_default())
    }

    async fn list_relations(&self, schema: &str) -> Result<Vec<Relation>, DbError> {
        // The seed data only models FK edges in pg-prod's public schema.
        let is_pg_public = tree_for(&self.conn)
            .is_some_and(|t| t.connection_id == "pg-prod")
            && schema == "public";
        Ok(if is_pg_public {
            vec![
                Relation {
                    from_table: "orders".into(),
                    from_column: "user_id".into(),
                    to_table: "users".into(),
                    to_column: "id".into(),
                },
                Relation {
                    from_table: "sessions".into(),
                    from_column: "user_id".into(),
                    to_table: "users".into(),
                    to_column: "id".into(),
                },
            ]
        } else {
            Vec::new()
        })
    }

    async fn fetch_rows(&self, table: TableRef, offset: u64, limit: u64) -> Result<Page, DbError> {
        let Some(meta) = find_meta(&self.conn, &table.schema, &table.name) else {
            return Err(DbError::new(format!(
                "table {}.{} not found in this connection",
                table.schema, table.name
            )));
        };
        let ms = self.latency_ms();
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        Ok(page_for(&meta, ms, offset, limit))
    }
}
