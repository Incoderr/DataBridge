//! Deterministic mock session - a faithful port of the web app's
//! `src/lib/mockData.ts`. The same (connection, schema, table, offset,
//! limit) always produces the same rows. Serves as the built-in demo
//! driver (dev/demo without a live DB) and seeds the wizard's connection
//! list until Stage 3 adds persistence.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;

use super::error::DbError;
use super::model::{
    CellChange, ColumnKind, ColumnMeta, ConnectionStatus, DbConnection, DbType, DocNode, NodeKind, Page,
    Ping, QueryPage, Relation, Row, RowQuery, SchemaMeta, TableMeta, TableRef, Value,
};
use super::model::is_select_like;
use super::session::{CancelToken, DbSession, cancellable};

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
            ssh: None,
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
            ssh: None,
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
            ssh: None,
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
            ssh: None,
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
            ssh: None,
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
    let name: String = name.into();
    // Demo-data conventions: integer PKs are identity-like, and the
    // `*_at` timestamps have DEFAULT now()-ish values — the editor shows
    // them as auto/default instead of required.
    let auto_increment = primary_key && kind == ColumnKind::Int;
    let has_default =
        auto_increment || (kind == ColumnKind::Date && name.ends_with("_at") && !nullable);
    ColumnMeta {
        type_: type_.into(),
        kind,
        nullable,
        primary_key,
        name,
        auto_increment,
        has_default,
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
            // Demo documents: the Postgres demo tables, served as
            // collections with full documents (`mock_documents`).
            DbType::MongoDb => trees().iter().find(|t| t.connection_id == "pg-prod"),
            DbType::Redis => trees().iter().find(|t| t.connection_id == "staging-pg"),
            // Stage-4 placeholders borrow the closest demo tree.
            DbType::MariaDb | DbType::ClickHouse => {
                trees().iter().find(|t| t.connection_id == "mysql-analytics")
            }
            DbType::Cassandra | DbType::ScyllaDb => {
                trees().iter().find(|t| t.connection_id == "unauth-mongo")
            }
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
//
// Demo rows are a pure function of (table, column, row_id) — no storage,
// any page of a million-row table is instant. The values are shaped like
// real data so the grid and the charts have something to show: skewed
// categories (more page views than purchases), log-normal money with a
// long tail, a year of timestamps that grows towards the end and dips on
// weekends, a `daily_events` series with trend + weekly seasonality, a
// funnel whose steps thin out, sequential integer ids.

/// Java-style string hash (seeds `_id`s of the demo documents).
fn md5(s: &str) -> i32 {
    let mut h: i32 = 0;
    for c in s.chars() {
        h = h.wrapping_mul(31).wrapping_add(c as i32);
    }
    h
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

/// The demo's "today": 2026-11-15 as days since 1970-01-01.
const DEMO_END_DAY: i64 = 20_772;
/// Timestamps span the last year.
const DEMO_SPAN_DAYS: i64 = 365;

/// SplitMix64 finalizer — well-spread bits from a weak seed.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Uniform [0, 1) for one (table, column, row, salt).
fn unit(table: &str, column: &str, row: i64, salt: u64) -> f64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in table.bytes().chain([b'.']).chain(column.bytes()) {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    (mix(h ^ mix(row as u64 ^ salt.rotate_left(32))) >> 11) as f64 / (1u64 << 53) as f64
}

/// Standard normal from two uniforms (Box–Muller).
fn normal(u1: f64, u2: f64) -> f64 {
    (-2.0 * (1.0 - u1).ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// Weighted choice.
fn pick_w<'a>(items: &[(&'a str, u32)], u: f64) -> &'a str {
    let total: u32 = items.iter().map(|i| i.1).sum();
    let mut x = (u * total as f64) as u32;
    for (s, w) in items {
        if x < *w {
            return s;
        }
        x -= w;
    }
    items[items.len() - 1].0
}

fn pick<'a>(arr: &[&'a str], u: f64) -> &'a str {
    arr[((u * arr.len() as f64) as usize).min(arr.len() - 1)]
}

fn money(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

const EMAIL_USERS: &[&str] = &["ada", "alan", "grace", "edsger", "linus", "guido", "maria", "kai", "lu", "alex"];
const EMAIL_HOSTS: &[(&str, u32)] =
    &[("gmail.com", 40), ("yandex.ru", 15), ("outlook.com", 14), ("icloud.com", 10), ("acme.io", 8), ("proton.me", 6), ("example.com", 7)];
const NAMES: &[&str] = &[
    "Ada Lovelace", "Alan Turing", "Edsger Dijkstra", "Grace Hopper", "Donald Knuth", "Guido van Rossum",
    "Linus Torvalds", "Barbara Liskov", "Ken Thompson", "Margaret Hamilton", "Niklaus Wirth", "Frances Allen",
];
const STATUSES: &[(&str, u32)] =
    &[("paid", 44), ("shipped", 24), ("open", 11), ("pending", 8), ("refunded", 7), ("cancelled", 6)];
const TITLES: &[&str] = &[
    "The Pragmatic Programmer", "Refactoring", "Clean Code", "Domain-Driven Design", "The Phoenix Project",
    "PostgreSQL Up & Running", "Designing Data-Intensive Applications", "Structure and Interpretation of Computer Programs",
    "The Mythical Man-Month", "Code Complete", "Working Effectively with Legacy Code", "Site Reliability Engineering",
];
const AUTHORS: &[(&str, u32)] = &[
    ("Martin Kleppmann", 18), ("Robert Martin", 16), ("Martin Fowler", 15), ("Kent Beck", 12),
    ("Sandi Metz", 10), ("Kathy Sierra", 9), ("Brian Goetz", 8), ("Eric Evans", 7), ("Fred Brooks", 5),
];
const COUNTRIES: &[(&str, u32)] = &[
    ("US", 29), ("DE", 13), ("RU", 12), ("BR", 10), ("GB", 9), ("FR", 8), ("IN", 7), ("JP", 6), ("CA", 4), ("PL", 2),
];
const ACTIONS: &[(&str, u32)] =
    &[("login", 38), ("refresh", 24), ("update", 16), ("logout", 12), ("create", 7), ("delete", 3)];
const ACTORS: &[(&str, u32)] = &[("frontend", 52), ("app_reader", 23), ("cron_worker", 17), ("staging_reader", 8)];
const PROVIDERS: &[(&str, u32)] = &[("google", 46), ("email", 27), ("github", 18), ("apple", 9)];
const EVENT_TYPES: &[(&str, u32)] =
    &[("page_view", 41), ("view", 21), ("click", 19), ("signup", 7), ("purchase", 6), ("error", 6)];
const PRODUCT_WORDS: &[&str] = &["Desk", "Lamp", "Chair", "Monitor", "Keyboard", "Mouse", "Headphones", "Webcam", "Dock", "Cable"];
const PRODUCT_ADJ: &[&str] = &["Pro", "Mini", "Ergo", "Studio", "Air", "Max", "Lite", "Classic"];

/// A day of the last year: denser towards the end (growth), thinner on
/// weekends.
fn demo_day(u: f64, weekend_skip: f64) -> i64 {
    let mut day = DEMO_END_DAY - DEMO_SPAN_DAYS + (u.sqrt() * DEMO_SPAN_DAYS as f64) as i64;
    // 1970-01-01 was a Thursday: (day + 3) % 7 → 0 = Monday … 5/6 = weekend.
    let dow = (day + 3).rem_euclid(7);
    if dow >= 5 && weekend_skip < 0.45 {
        day -= dow - 4; // moved to Friday
    }
    day.min(DEMO_END_DAY)
}

fn date_text(day: i64) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `daily_events` values for one day index (0 = a year ago).
fn daily_series(row: i64) -> (i64, i64) {
    let day = DEMO_END_DAY - DEMO_SPAN_DAYS + 1 + row;
    let dow = (day + 3).rem_euclid(7);
    let trend = 1_800.0 + 5.5 * row as f64;
    let weekly = if dow >= 5 { 0.68 } else { 1.0 + 0.04 * (2 - (dow - 2).abs()) as f64 };
    // Holiday season bump in the last ~6 weeks.
    let season = 1.0 + 0.25 * ((row as f64 - 320.0) / 45.0).clamp(0.0, 1.0);
    let noise = 1.0 + 0.07 * normal(unit("daily_events", "n1", row, 1), unit("daily_events", "n2", row, 2));
    let events = (trend * weekly * season * noise).max(0.0) as i64;
    let share = 0.31 + 0.03 * normal(unit("daily_events", "u1", row, 3), unit("daily_events", "u2", row, 4));
    (events, (events as f64 * share) as i64)
}

/// The funnel step a `funnel_steps` row reached: each step rarer than the last.
fn funnel_step(t: &TableMeta, row: i64) -> i64 {
    pick_w(&[("1", 40), ("2", 25), ("3", 17), ("4", 11), ("5", 7)], unit(&t.name, "step", row, 1))
        .parse()
        .unwrap_or(1)
}

fn gen_value(t: &TableMeta, c: &ColumnMeta, row: i64) -> Value {
    let u = |salt: u64| unit(&t.name, &c.name, row, salt);
    // Nullable, non-key columns are sometimes empty.
    if c.nullable && !c.primary_key && u(99) < 0.04 {
        return Value::Null;
    }
    let lognormal = |median: f64, sigma: f64| median * (sigma * normal(u(1), u(2))).exp();

    // The daily series view: one row per day, ascending.
    if t.name == "daily_events" {
        let (events, users) = daily_series(row);
        return match c.name.as_str() {
            "day" => Value::Str(date_text(DEMO_END_DAY - DEMO_SPAN_DAYS + 1 + row)),
            "event_count" => Value::Int(events),
            "unique_users" => Value::Int(users),
            _ => Value::Null,
        };
    }

    match c.kind {
        // Integer keys are sequential (unique, and editable by key).
        ColumnKind::Int if c.primary_key || c.name == "id" => Value::Int(row + 1),
        ColumnKind::Int => Value::Int(match c.name.as_str() {
            // Power users: low ids are much more active.
            "user_id" => (24_812.0 * u(1).powf(2.2)) as i64 + 1,
            "step" => funnel_step(t, row),
            "in_stock" if u(3) < 0.08 => 0,
            "in_stock" => lognormal(60.0, 0.8).round() as i64,
            "order_count" => lognormal(4.0, 0.9).round() as i64,
            "year" => 2024 - (29.0 * u(1).powi(2)) as i64,
            "birth_year" => (1966.0 + 14.0 * normal(u(1), u(2))).clamp(1925.0, 2001.0) as i64,
            _ => (lognormal(120.0, 1.0)).round() as i64,
        }),
        ColumnKind::Float => Value::Float(match c.name.as_str() {
            "price" => (lognormal(35.0, 0.75).floor() + 0.99).min(4_999.99),
            "total" => money(lognormal(64.0, 0.85)),
            "balance" => money(lognormal(120.0, 1.1)),
            "lifetime_value" => money(lognormal(380.0, 1.0)),
            "rating" => ((4.05 + 0.5 * normal(u(1), u(2))).clamp(1.0, 5.0) * 10.0).round() / 10.0,
            // Funnel revenue grows with the step reached.
            "value" => {
                let step = funnel_step(t, row) as f64;
                ((lognormal(12.0, 0.6) * step) * 10_000.0).round() / 10_000.0
            }
            _ => money(lognormal(50.0, 0.9)),
        }),
        ColumnKind::Bool => Value::Bool(u(1) < if c.name == "is_active" { 0.82 } else { 0.5 }),
        ColumnKind::Date => {
            let day = if c.name == "expires_at" {
                DEMO_END_DAY + (u(1) * 30.0) as i64 // sessions expire within a month
            } else {
                demo_day(u(1), u(2))
            };
            if c.type_ == "date" {
                return Value::Str(date_text(day));
            }
            // Busy hours: a bell around 14:00.
            let hh = (14.0 + 4.0 * normal(u(3), u(4))).clamp(0.0, 23.0) as u64;
            let mm = (u(5) * 60.0) as u64;
            let ss = (u(6) * 60.0) as u64;
            Value::Str(format!("{} {hh:02}:{mm:02}:{ss:02}", date_text(day)))
        }
        ColumnKind::Uuid => {
            let a = mix(row as u64 ^ 0xA5A5);
            let b = mix(a);
            Value::Str(format!(
                "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
                a >> 32,
                (a >> 16) & 0xffff,
                a & 0xfff,
                0x8000 | (b >> 48) & 0x3fff,
                b & 0xffff_ffff_ffff
            ))
        }
        ColumnKind::Json => Value::Str(match t.name.as_str() {
            "events" => format!(
                "{{\"path\":\"/{}\",\"device\":\"{}\",\"ms\":{}}}",
                pick(&["", "pricing", "docs", "blog", "checkout", "account"], u(1)),
                pick_w(&[("desktop", 58), ("mobile", 36), ("tablet", 6)], u(2)),
                lognormal(180.0, 0.6) as i64
            ),
            "audit_log" => format!(
                "{{\"ip\":\"10.{}.{}.{}\",\"ok\":{}}}",
                (u(1) * 255.0) as u8,
                (u(2) * 255.0) as u8,
                (u(3) * 255.0) as u8,
                u(4) < 0.96
            ),
            _ => format!(
                "{{\"color\":\"{}\",\"weight_kg\":{:.1}}}",
                pick(&["black", "white", "graphite", "silver", "blue"], u(1)),
                lognormal(1.2, 0.7)
            ),
        }),
        ColumnKind::Binary => Value::Str(format!("0x{:04x}", mix(row as u64) & 0xffff)),
        ColumnKind::Text => Value::Str(match c.name.as_str() {
            "email" => format!(
                "{}{}@{}",
                pick(EMAIL_USERS, u(1)),
                row % 1000,
                pick_w(EMAIL_HOSTS, u(2))
            ),
            "name" => pick(NAMES, u(1)).to_string(),
            "status" => pick_w(STATUSES, u(1)).to_string(),
            "title" if t.name == "products" => {
                format!("{} {}", pick(PRODUCT_WORDS, u(1)), pick(PRODUCT_ADJ, u(2)))
            }
            "title" => pick(TITLES, u(1)).to_string(),
            "sku" => format!("SKU-{}", pad(10_000 + row as u64, 5)),
            "author" => pick_w(AUTHORS, u(1)).to_string(),
            "country" => pick_w(COUNTRIES, u(1)).to_string(),
            "action" => pick_w(ACTIONS, u(1)).to_string(),
            "actor" => pick_w(ACTORS, u(1)).to_string(),
            "provider" => pick_w(PROVIDERS, u(1)).to_string(),
            "provider_uid" => format!("{}:{}", pick_w(PROVIDERS, u(1)), mix(row as u64) % 10_000_000),
            "token" => format!("tok_{:016x}", mix(row as u64 ^ 0x70)),
            "event_type" => pick_w(EVENT_TYPES, u(1)).to_string(),
            "password_hash" => format!("$2b$12${:022x}", mix(row as u64 ^ 0xbc) as u128 * 7919),
            "session_id" => format!("s_{:012x}", mix(row as u64 ^ 0x5e) & 0xffff_ffff_ffff),
            "tags" => {
                let all = ["architecture", "testing", "databases", "career", "devops", "classic"];
                let a = pick(&all, u(1));
                let b = pick(&all, u(2));
                if a == b { a.to_string() } else { format!("{a},{b}") }
            }
            _ => format!("{}-{}", c.name, row + 1),
        }),
    }
}

// ---------- The overlay-aware page lives in the session impl below ----------

// ---------- MockSession: the built-in demo driver ----------

/// In-memory write overlay on top of the deterministic generator: the
/// demo driver supports the simplified editor without a real database.
/// Keyed by `schema.table` + the generator's `row_id`.
#[derive(Default)]
struct MockEdits {
    /// (table, row_id) → the full replacement row.
    updated: HashMap<(String, i64), Row>,
    /// table → rows appended after the generated ones.
    inserted: HashMap<String, Vec<Row>>,
    /// (table, row_id) marked deleted.
    deleted: HashSet<(String, i64)>,
    /// (schema, name) of dropped tables / views — hidden from the tree.
    dropped: HashSet<(String, String)>,
}

pub struct MockSession {
    conn: DbConnection,
    edits: Mutex<MockEdits>,
}

impl MockSession {
    pub fn new(conn: &DbConnection) -> Self {
        Self {
            conn: conn.clone(),
            edits: Mutex::new(MockEdits::default()),
        }
    }

    /// Simulated per-connection query latency (2..120 ms).
    fn latency_ms(&self) -> u32 {
        self.conn.latency_ms.clamp(2, 120)
    }

    /// The generated primary-key value of one row ("" when the table has
    /// no PK — such tables are not editable through the mock).
    fn pk_of(meta: &TableMeta, row_id: i64) -> String {
        meta.columns
            .iter()
            .filter(|c| c.primary_key)
            .map(|c| gen_value(meta, c, row_id).display())
            .collect::<Vec<_>>()
            .join("|")
    }

    /// Resolve the editor's pk cells to the generator's row_id.
    fn row_id_by_pk(meta: &TableMeta, pk: &[CellChange]) -> Option<i64> {
        let want = pk
            .iter()
            .map(|c| c.value.display())
            .collect::<Vec<_>>()
            .join("|");
        // Integer keys are `row_id + 1` (`gen_value`): check that row first.
        if let [CellChange { value: Value::Int(n), .. }] = pk {
            let id = n - 1;
            if (0..meta.row_count as i64).contains(&id) && Self::pk_of(meta, id) == want {
                return Some(id);
            }
        }
        (0..meta.row_count as i64).find(|&id| Self::pk_of(meta, id) == want)
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
        let mut schemas = tree_for(&self.conn)
            .map(|t| t.schemas.clone())
            .unwrap_or_default();
        let edits = self.edits.lock().unwrap();
        for sch in &mut schemas {
            let gone = |t: &Arc<TableMeta>| edits.dropped.contains(&(sch.name.clone(), t.name.clone()));
            sch.tables.retain(|t| !gone(t));
            sch.views.retain(|t| !gone(t));
        }
        Ok(schemas)
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

    async fn fetch_rows(
        &self,
        table: TableRef,
        query: RowQuery,
        offset: u64,
        limit: u64,
    ) -> Result<Page, DbError> {
        let Some(meta) = find_meta(&self.conn, &table.schema, &table.name) else {
            return Err(DbError::new(format!(
                "table {}.{} not found in this connection",
                table.schema, table.name
            )));
        };
        let ms = self.latency_ms();
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        let edits = self.edits.lock().unwrap();
        let mut page = if query == RowQuery::default() {
            mock_page(&edits, &meta, &table, ms, offset, limit)
        } else {
            mock_query_page(&edits, &meta, &table, &query, ms, offset, limit)
        };
        if self.conn.db_type == DbType::MongoDb {
            page.documents = Some(mock_documents(&meta, &page.rows));
        }
        Ok(page)
    }

    async fn execute(
        &self,
        sql: &str,
        limit: u64,
        cancel: &CancelToken,
    ) -> Result<QueryPage, DbError> {
        // `SELECT pg_sleep(5)` / `SELECT SLEEP(5)` really waits, so Stop
        // is demoable without a database.
        let ms = self.latency_ms() + mock_sleep_ms(sql);
        cancellable(
            async {
                tokio::time::sleep(Duration::from_millis(ms as u64)).await;
                Ok(())
            },
            cancel,
            None::<std::future::Ready<()>>,
        )
        .await?;
        // `SELECT * FROM [schema.]table [LIMIT n]` is served from the
        // generated rows, so the results grid and its chart have real data.
        if let Some((schema, name, n)) = simple_select(sql) {
            let tree = tree_for(&self.conn);
            let meta = tree.and_then(|t| {
                t.schemas
                    .iter()
                    .filter(|s| schema.as_ref().is_none_or(|want| s.name.eq_ignore_ascii_case(want)))
                    .flat_map(|s| s.tables.iter().chain(s.views.iter()))
                    .find(|t| t.name.eq_ignore_ascii_case(&name))
                    .cloned()
            });
            let Some(meta) = meta else {
                return Err(DbError::new(format!("query failed: relation \"{name}\" does not exist")));
            };
            let take = n.unwrap_or(limit).min(limit);
            let edits = self.edits.lock().unwrap();
            let page = mock_page(&edits, &meta, &TableRef::of(&meta), ms, 0, take);
            return Ok(QueryPage {
                columns: meta.columns.clone(),
                rows: page.rows,
                rows_affected: 0,
                // More rows exist than the editor keeps.
                truncated: n.is_none_or(|n| n > limit) && page.total > take,
                elapsed_ms: ms,
            });
        }
        // Anything else cannot run here; report a plausible outcome so the
        // editor flow is demoable end-to-end.
        if is_select_like(sql) {
            Ok(QueryPage {
                columns: vec![ColumnMeta {
                    name: "result".into(),
                    type_: "text".into(),
                    nullable: true,
                    primary_key: false,
                    auto_increment: false,
                    has_default: false,
                    kind: ColumnKind::Text,
                }],
                rows: vec![vec![Value::Str(
                    "demo mode: statements are not executed — connect to a real database".into(),
                )]],
                rows_affected: 0,
                truncated: limit == 0,
                elapsed_ms: ms,
            })
        } else {
            Ok(QueryPage {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: 1,
                truncated: false,
                elapsed_ms: ms,
            })
        }
    }

    async fn update_row(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
        changes: Vec<CellChange>,
    ) -> Result<u64, DbError> {
        let Some(meta) = find_meta(&self.conn, &table.schema, &table.name) else {
            return Err(DbError::new(format!(
                "table {}.{} not found in this connection",
                table.schema, table.name
            )));
        };
        if pk.is_empty() {
            return Err(DbError::new(
                "this table has no primary key — edit it via SQL instead",
            ));
        }
        let Some(row_id) = Self::row_id_by_pk(&meta, &pk) else {
            return Err(DbError::new("row not found (primary key mismatch)"));
        };
        let key = format!("{}.{}", table.schema, table.name);
        let mut edits = self.edits.lock().unwrap();
        // Base: the overlay row if present, else the generated one.
        let mut row: Row = edits
            .updated
            .get(&(key.clone(), row_id))
            .cloned()
            .unwrap_or_else(|| {
                meta.columns
                    .iter()
                    .map(|c| gen_value(&meta, c, row_id))
                    .collect()
            });
        for ch in &changes {
            if let Some(ix) = meta.columns.iter().position(|c| c.name == ch.column) {
                row[ix] = ch.value.clone();
            }
        }
        edits.updated.insert((key, row_id), row);
        Ok(1)
    }

    async fn insert_row(&self, table: TableRef, values: Vec<CellChange>) -> Result<u64, DbError> {
        let Some(meta) = find_meta(&self.conn, &table.schema, &table.name) else {
            return Err(DbError::new(format!(
                "table {}.{} not found in this connection",
                table.schema, table.name
            )));
        };
        // A full row aligned with the table's columns; missing ones are
        // NULL (the generated demo data has no defaults to draw from).
        let row: Row = meta
            .columns
            .iter()
            .map(|c| {
                values
                    .iter()
                    .find(|v| v.column == c.name)
                    .map(|v| v.value.clone())
                    .unwrap_or(Value::Null)
            })
            .collect();
        let key = format!("{}.{}", table.schema, table.name);
        self.edits.lock().unwrap().inserted.entry(key).or_default().push(row);
        Ok(1)
    }

    async fn delete_row(&self, table: TableRef, pk: Vec<CellChange>) -> Result<u64, DbError> {
        let Some(meta) = find_meta(&self.conn, &table.schema, &table.name) else {
            return Err(DbError::new(format!(
                "table {}.{} not found in this connection",
                table.schema, table.name
            )));
        };
        if pk.is_empty() {
            return Err(DbError::new(
                "this table has no primary key — delete it via SQL instead",
            ));
        }
        let Some(row_id) = Self::row_id_by_pk(&meta, &pk) else {
            return Err(DbError::new("row not found (primary key mismatch)"));
        };
        let key = format!("{}.{}", table.schema, table.name);
        let mut edits = self.edits.lock().unwrap();
        edits.updated.remove(&(key.clone(), row_id));
        edits.deleted.insert((key, row_id));
        Ok(1)
    }

    async fn drop_table(&self, table: TableRef) -> Result<(), DbError> {
        if table.kind.drop_keyword().is_none()
            || find_meta(&self.conn, &table.schema, &table.name).is_none()
        {
            return Err(DbError::new(format!(
                "{}.{} cannot be dropped",
                table.schema, table.name
            )));
        }
        // Demo latency, like every other mock call.
        tokio::time::sleep(Duration::from_millis(self.conn.latency_ms.max(40) as u64)).await;
        self.edits
            .lock()
            .unwrap()
            .dropped
            .insert((table.schema, table.name));
        Ok(())
    }
}

/// The overlay-aware page builder: generated rows minus deleted, with
/// updates applied, plus the session's inserted rows appended at the end.
fn mock_page(
    edits: &MockEdits,
    t: &TableMeta,
    table: &TableRef,
    latency_ms: u32,
    offset: u64,
    limit: u64,
) -> Page {
    let key = format!("{}.{}", table.schema, table.name);
    mock_page_rows(edits, t, key, latency_ms, offset, limit)
}

/// Demo documents for `--mock` MongoDB connections: each row as a
/// document, plus a nested object and an array so the JSON view and the
/// document tree have something to unfold.
fn mock_documents(meta: &TableMeta, rows: &[Row]) -> Vec<DocNode> {
    let leaf = |text: String, kind: Option<ColumnKind>| DocNode::Scalar { text, kind };
    rows.iter()
        .map(|row| {
            let mut fields: Vec<(String, DocNode)> = meta
                .columns
                .iter()
                .zip(row)
                .map(|(c, v)| {
                    let node = match v {
                        Value::Null => leaf("null".into(), None),
                        Value::Int(n) => leaf(n.to_string(), Some(ColumnKind::Int)),
                        Value::Float(f) => leaf(format!("{f:?}"), Some(ColumnKind::Float)),
                        Value::Bool(b) => leaf(b.to_string(), Some(ColumnKind::Bool)),
                        Value::Str(s) if c.kind == ColumnKind::Date => {
                            leaf(format!("ISODate(\"{s}\")"), Some(ColumnKind::Date))
                        }
                        Value::Str(s) => leaf(
                            serde_json::to_string(s).unwrap_or_default(),
                            Some(c.kind),
                        ),
                        Value::Bytes(b) => leaf(format!("<{} bytes>", b.len()), Some(ColumnKind::Binary)),
                    };
                    (c.name.clone(), node)
                })
                .collect();
            // A deterministic ObjectId-looking `_id`, like a real collection.
            let seed = format!("{}:{:?}", meta.name, row.first());
            let h = md5(&seed) as u32;
            let hex = format!("65{h:08x}{:014x}", (h as u64).wrapping_mul(0x9E37_79B9) & 0xFF_FFFF_FFFF_FFFF);
            fields.insert(0, ("_id".into(), leaf(format!("ObjectId(\"{hex}\")"), Some(ColumnKind::Uuid))));
            fields.push((
                "meta".into(),
                DocNode::Object(vec![
                    ("source".into(), leaf("\"demo\"".into(), Some(ColumnKind::Text))),
                    (
                        "tags".into(),
                        DocNode::Array(vec![
                            leaf("\"mock\"".into(), Some(ColumnKind::Text)),
                            leaf("\"--mock\"".into(), Some(ColumnKind::Text)),
                        ]),
                    ),
                ]),
            ));
            DocNode::Object(fields)
        })
        .collect()
}

fn mock_page_rows(
    edits: &MockEdits,
    t: &TableMeta,
    key: String,
    latency_ms: u32,
    offset: u64,
    limit: u64,
) -> Page {
    let deleted_here = edits.deleted.iter().filter(|e| e.0 == key).count() as u64;
    let inserted = edits.inserted.get(&key).map(|v| v.len() as u64).unwrap_or(0);
    let gen_count = t.row_count.saturating_sub(deleted_here);
    let total = gen_count + inserted;

    let mut rows = Vec::new();
    let mut i = 0u64;
    while i < limit && offset + i < total {
        let virtual_ix = offset + i;
        let row: Row = if virtual_ix < gen_count {
            // Map the virtual index onto the next non-deleted row_id.
            let mut seen = 0u64;
            let mut row_id = 0i64;
            for id in 0..t.row_count as i64 {
                if !edits.deleted.contains(&(key.clone(), id)) {
                    if seen == virtual_ix {
                        row_id = id;
                        break;
                    }
                    seen += 1;
                }
            }
            edits
                .updated
                .get(&(key.clone(), row_id))
                .cloned()
                .unwrap_or_else(|| t.columns.iter().map(|c| gen_value(t, c, row_id)).collect())
        } else {
            let ins = edits.inserted.get(&key).unwrap();
            ins[(virtual_ix - gen_count) as usize].clone()
        };
        rows.push(row);
        i += 1;
    }
    Page {
        truncated: total > limit,
        elapsed_ms: latency_ms,
        total,
        exact: true,
        rows,
        columns: None,
        documents: None,
    }
}

/// Rows a filtered/sorted demo page scans at most (generated on the fly).
const MOCK_SCAN: i64 = 20_000;

/// Filtered and/or sorted page: every (edited) row up to [`MOCK_SCAN`] is
/// generated, filtered and sorted in memory — demo data only.
fn mock_query_page(
    edits: &MockEdits,
    t: &TableMeta,
    table: &TableRef,
    query: &RowQuery,
    latency_ms: u32,
    offset: u64,
    limit: u64,
) -> Page {
    let key = format!("{}.{}", table.schema, table.name);
    let mut rows: Vec<Row> = (0..(t.row_count as i64).min(MOCK_SCAN))
        .filter(|id| !edits.deleted.contains(&(key.clone(), *id)))
        .map(|id| {
            edits
                .updated
                .get(&(key.clone(), id))
                .cloned()
                .unwrap_or_else(|| t.columns.iter().map(|c| gen_value(t, c, id)).collect())
        })
        .chain(edits.inserted.get(&key).cloned().unwrap_or_default())
        .collect();
    let ix = |name: &str| t.columns.iter().position(|c| c.name == name);
    rows.retain(|r| {
        query.filters.iter().all(|f| match ix(&f.column) {
            Some(i) => f.matches(&r[i]),
            None => false,
        })
    });
    if let Some((col, desc)) = &query.sort {
        if let Some(i) = ix(col) {
            rows.sort_by(|a, b| {
                let o = crate::core::model::cmp_values(&a[i], &b[i]);
                if *desc { o.reverse() } else { o }
            });
        }
    }
    let total = rows.len() as u64;
    let rows: Vec<Row> = rows.into_iter().skip(offset as usize).take(limit as usize).collect();
    Page {
        truncated: total > offset + limit,
        elapsed_ms: latency_ms,
        total,
        exact: true,
        rows,
        columns: None,
        documents: None,
    }
}

/// `SELECT * FROM [schema.]table [LIMIT n][;]` → (schema, table, n);
/// identifiers may be quoted with `"` or backticks. Anything richer is
/// `None` (the mock does not interpret SQL).
fn simple_select(sql: &str) -> Option<(Option<String>, String, Option<u64>)> {
    let sql = sql.trim().trim_end_matches(';').trim();
    let words: Vec<&str> = sql.split_whitespace().collect();
    let (target, rest) = match words.as_slice() {
        [s, "*", f, target, rest @ ..]
            if s.eq_ignore_ascii_case("select") && f.eq_ignore_ascii_case("from") =>
        {
            (*target, rest)
        }
        _ => return None,
    };
    let n = match rest {
        [] => None,
        [l, n] if l.eq_ignore_ascii_case("limit") => Some(n.parse::<u64>().ok()?),
        _ => return None,
    };
    let unquote = |s: &str| s.trim_matches(|c| c == '"' || c == '`').to_string();
    let (schema, name) = match target.split_once('.') {
        Some((s, t)) => (Some(unquote(s)), unquote(t)),
        None => (None, unquote(target)),
    };
    (!name.is_empty()).then_some((schema, name, n))
}

/// Seconds of a `sleep(n)` / `pg_sleep(n)` call in a mock statement, as
/// milliseconds (capped at a minute).
fn mock_sleep_ms(sql: &str) -> u32 {
    let lower = sql.to_ascii_lowercase();
    let Some(at) = lower.find("sleep(") else {
        return 0;
    };
    let arg: String = lower[at + 6..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || c.is_whitespace())
        .filter(|c| !c.is_whitespace())
        .collect();
    let secs: f64 = arg.parse().unwrap_or(0.0);
    (secs.clamp(0.0, 60.0) * 1000.0) as u32
}

#[cfg(test)]
mod tests {
    #[test]
    fn mock_sleep_reads_the_argument() {
        assert_eq!(super::mock_sleep_ms("SELECT pg_sleep(2)"), 2000);
        assert_eq!(super::mock_sleep_ms("select SLEEP( 0.5 )"), 500);
        assert_eq!(super::mock_sleep_ms("SELECT 1"), 0);
        assert_eq!(super::mock_sleep_ms("SELECT sleep(9999)"), 60_000);
    }

    #[test]
    fn demo_data_looks_real() {
        use super::*;
        let tree = trees().iter().find(|t| t.connection_id == "mysql-analytics").unwrap();
        let wh = &tree.schemas[0];
        let events = wh.tables.iter().find(|t| t.name == "events").unwrap();
        let col = |t: &TableMeta, n: &str| t.columns.iter().find(|c| c.name == n).unwrap().clone();

        // Deterministic, sequential keys.
        let id = col(events, "id");
        assert_eq!(gen_value(events, &id, 41), Value::Int(42));
        let et = col(events, "event_type");
        assert_eq!(gen_value(events, &et, 7), gen_value(events, &et, 7));

        // Skewed categories: page views far outnumber purchases.
        let mut views = 0;
        let mut purchases = 0;
        for r in 0..5_000 {
            match gen_value(events, &et, r) {
                Value::Str(s) if s == "page_view" => views += 1,
                Value::Str(s) if s == "purchase" => purchases += 1,
                _ => {}
            }
        }
        assert!(views > 4 * purchases, "{views} vs {purchases}");

        // Timestamps fall in the demo year.
        let ts = col(events, "ts");
        for r in 0..500 {
            let Value::Str(s) = gen_value(events, &ts, r) else { panic!() };
            assert!(s.as_str() >= "2025-11-15" && s.as_str() <= "2026-11-15 23:59:59", "{s}");
        }

        // The daily series trends upward and dips on weekends.
        let daily = wh.views.iter().find(|t| t.name == "daily_events").unwrap();
        let ev = col(daily, "event_count");
        let n = |r: i64| match gen_value(daily, &ev, r) { Value::Int(n) => n, _ => 0 };
        let first: i64 = (0..28).map(n).sum();
        let last: i64 = (300..328).map(n).sum();
        assert!(last > first * 3 / 2, "{first} → {last}");
        let day = col(daily, "day");
        assert_eq!(gen_value(daily, &day, 364), Value::Str("2026-11-15".into()));
    }

    #[test]
    fn simple_selects_are_recognised() {
        use super::simple_select;
        assert_eq!(simple_select("SELECT * FROM events LIMIT 100"), Some((None, "events".into(), Some(100))));
        assert_eq!(
            simple_select("select * from \"public\".`users`;"),
            Some((Some("public".into()), "users".into(), None))
        );
        assert_eq!(simple_select("SELECT id FROM events"), None);
        assert_eq!(simple_select("SELECT * FROM events WHERE id = 1"), None);
        assert_eq!(simple_select("SELECT * FROM events LIMIT x"), None);
    }
}
