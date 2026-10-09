//! PostgreSQL driver: `DbSession` over a lazily-connected sqlx `PgPool`.
//!
//! Introspection runs in batches per schema (relations, columns, primary
//! keys, routines) instead of per table, and row counts are deliberately
//! not fetched (`COUNT(*)` on a production table can be prohibitive; the
//! grid computes the real total on the first page load).

use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{Column as _, Row, TypeInfo as _};
use tokio::sync::OnceCell;

use crate::core::error::DbError;
use crate::core::model::is_select_like;
use crate::core::model::{
    CellChange, ColumnKind, ColumnMeta, ConnectionStatus, DbConnection, NodeKind, Page, Ping,
    QueryPage, Relation, SchemaMeta, TableMeta, TableRef,
};
use crate::core::session::DbSession;
use crate::drivers::util::{bind_cell, dyn_cell};

pub struct PostgresSession {
    opts: PgConnectOptions,
    /// When set, parsed natively by sqlx (preserving query options).
    url: Option<String>,
    pool: OnceCell<PgPool>,
}

impl PostgresSession {
    pub fn new(conn: &DbConnection) -> Self {
        let mut opts = PgConnectOptions::new()
            .host(&conn.host)
            .port(conn.port)
            .username(&conn.user)
            .database(&conn.database);
        if !conn.password.is_empty() {
            opts = opts.password(&conn.password);
        }
        Self {
            opts,
            url: conn.url.clone(),
            pool: OnceCell::new(),
        }
    }

    /// The pool is created on first use (connect errors surface as
    /// `DbError` on the first session call, e.g. the wizard's ping).
    async fn pool(&self) -> Result<&PgPool, DbError> {
        self.pool
            .get_or_try_init(|| async {
                let opts = match &self.url {
                    Some(url) => PgConnectOptions::from_str(url).map_err(|e| {
                        DbError::with_source("invalid PostgreSQL connection URL", e)
                    })?,
                    None => self.opts.clone(),
                };
                PgPoolOptions::new()
                    .max_connections(4)
                    .acquire_timeout(Duration::from_secs(8))
                    .connect_with(opts)
                    .await
                    .map_err(|e| DbError::with_source("failed to connect to PostgreSQL", e))
            })
            .await
    }
}

/// `"schema"."table"` with embedded quotes doubled. Both parts come from
/// our own introspection, never from user input.
fn pg_ident(schema: &str, name: &str) -> String {
    format!(
        "\"{}\".\"{}\"",
        schema.replace('"', "\"\""),
        name.replace('"', "\"\"")
    )
}

/// Maps both the short `udt_name` ("int8") and the descriptive
/// `data_type` ("integer") spellings.
fn pg_kind(t: &str) -> ColumnKind {
    match t {
        "int2" | "int4" | "int8" | "smallint" | "integer" | "bigint" => ColumnKind::Int,
        "float4" | "float8" | "numeric" | "real" | "double precision" => ColumnKind::Float,
        "bool" | "boolean" => ColumnKind::Bool,
        "json" | "jsonb" => ColumnKind::Json,
        "uuid" => ColumnKind::Uuid,
        "bytea" => ColumnKind::Binary,
        "date" | "timestamp" | "timestamptz" | "timestamp with time zone"
        | "timestamp without time zone" | "time" | "timetz" | "time with time zone"
        | "time without time zone" | "interval" => ColumnKind::Date,
        _ => ColumnKind::Text,
    }
}

fn type_label(udt: &str, char_len: Option<i32>, num_prec: Option<i32>, num_scale: Option<i32>) -> String {
    if let Some(n) = char_len {
        if n > 0 {
            return format!("{udt}({n})");
        }
    }
    if udt == "numeric" {
        if let (Some(p), Some(s)) = (num_prec, num_scale) {
            return format!("numeric({p},{s})");
        }
    }
    udt.to_string()
}

#[async_trait]
impl DbSession for PostgresSession {
    async fn ping(&self) -> Result<Ping, DbError> {
        let pool = self.pool().await?;
        let t0 = Instant::now();
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(pool)
            .await
            .map_err(|e| DbError::with_source("PostgreSQL ping failed", e))?;
        Ok(Ping {
            status: ConnectionStatus::Connected,
            latency_ms: t0.elapsed().as_millis() as u32,
        })
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaMeta>, DbError> {
        let pool = self.pool().await?.clone();
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT nspname FROM pg_namespace \
             WHERE nspname NOT IN ('pg_catalog', 'information_schema') \
               AND nspname NOT LIKE 'pg\\_temp%' \
               AND nspname NOT LIKE 'pg\\_toast%' \
             ORDER BY nspname",
        )
        .fetch_all(&pool)
        .await
        .map_err(|e| DbError::with_source("failed to list PostgreSQL schemas", e))?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            out.push(schema_meta(&pool, name).await?);
        }
        Ok(out)
    }

    async fn list_relations(&self, schema: &str) -> Result<Vec<Relation>, DbError> {
        let pool = self.pool().await?;
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT tc.table_name, kcu.column_name, ccu.table_name, ccu.column_name \
             FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON kcu.constraint_name = tc.constraint_name \
              AND kcu.table_schema = tc.table_schema \
             JOIN information_schema.constraint_column_usage ccu \
               ON ccu.constraint_name = tc.constraint_name \
              AND ccu.table_schema = tc.table_schema \
             WHERE tc.constraint_type = 'FOREIGN KEY' AND tc.table_schema = $1",
        )
        .bind(schema)
        .fetch_all(pool)
        .await
        .map_err(|e| {
            DbError::with_source(format!("failed to read foreign keys of {schema}"), e)
        })?;
        Ok(rows
            .into_iter()
            .map(|(ft, fc, tt, tc)| Relation {
                from_table: ft,
                from_column: fc,
                to_table: tt,
                to_column: tc,
            })
            .collect())
    }

    async fn fetch_rows(&self, table: TableRef, offset: u64, limit: u64) -> Result<Page, DbError> {
        let pool = self.pool().await?;
        let t0 = Instant::now();
        let ident = pg_ident(&table.schema, &table.name);
        let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {ident}"))
            .fetch_one(pool)
            .await
            .map_err(|e| {
                DbError::with_source(format!("failed to count {}.{}", table.schema, table.name), e)
            })?;
        let rows = sqlx::query(&format!("SELECT * FROM {ident} LIMIT $1 OFFSET $2"))
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(pool)
            .await
            .map_err(|e| {
                DbError::with_source(format!("failed to read {}.{}", table.schema, table.name), e)
            })?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut r = Vec::with_capacity(row.len());
            for i in 0..row.len() {
                r.push(dyn_cell!(row, i));
            }
            out.push(r);
        }
        Ok(Page {
            rows: out,
            total: total as u64,
            truncated: total as u64 > offset + limit,
            elapsed_ms: t0.elapsed().as_millis() as u32,
        })
    }

    async fn execute(&self, sql: &str, limit: u64) -> Result<QueryPage, DbError> {
        let pool = self.pool().await?;
        let t0 = Instant::now();
        if !is_select_like(sql) {
            let res = sqlx::query(sql)
                .execute(pool)
                .await
                .map_err(|e| DbError::with_source("query failed", e))?;
            return Ok(QueryPage {
                columns: Vec::new(),
                rows: Vec::new(),
                rows_affected: res.rows_affected(),
                truncated: false,
                elapsed_ms: t0.elapsed().as_millis() as u32,
            });
        }
        let rows = sqlx::query(sql)
            .fetch_all(pool)
            .await
            .map_err(|e| DbError::with_source("query failed", e))?;
        let mut columns: Vec<ColumnMeta> = rows
            .first()
            .map(|r| {
                r.columns()
                    .iter()
                    .map(|c| ColumnMeta {
                        name: c.name().to_string(),
                        type_: c.type_info().to_string(),
                        nullable: true,
                        primary_key: false,
                        auto_increment: false,
                        has_default: false,
                        kind: pg_kind(c.type_info().name()),
                    })
                    .collect()
            })
            .unwrap_or_default();
        if columns.is_empty() && rows.is_empty() {
            // `SELECT` with no rows still knows its column list through
            // the statement metadata — fall back to a re-run via `fetch`
            // is overkill; report an empty result set.
            columns = Vec::new();
        }
        let truncated = rows.len() > limit as usize;
        let mut out = Vec::with_capacity(rows.len().min(limit as usize));
        for row in rows.into_iter().take(limit as usize) {
            let mut r = Vec::with_capacity(row.len());
            for i in 0..row.len() {
                r.push(dyn_cell!(row, i));
            }
            out.push(r);
        }
        Ok(QueryPage {
            columns,
            rows: out,
            rows_affected: 0,
            truncated,
            elapsed_ms: t0.elapsed().as_millis() as u32,
        })
    }

    async fn update_row(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
        changes: Vec<CellChange>,
    ) -> Result<u64, DbError> {
        let pool = self.pool().await?;
        if changes.is_empty() {
            return Ok(0);
        }
        let ident = pg_ident(&table.schema, &table.name);
        let mut sets = String::new();
        for (i, ch) in changes.iter().enumerate() {
            if i > 0 {
                sets.push_str(", ");
            }
            sets.push_str(&format!("\"{}\" = ${}", ch.column.replace('"', "\"\""), i + 1));
        }
        let mut where_ = String::new();
        for (i, key) in pk.iter().enumerate() {
            if i > 0 {
                where_.push_str(" AND ");
            }
            where_.push_str(&format!(
                "\"{}\" = ${}",
                key.column.replace('"', "\"\""),
                changes.len() + i + 1
            ));
        }
        let sql = format!("UPDATE {ident} SET {sets} WHERE {where_}");
        let mut q = sqlx::query(&sql);
        for ch in &changes {
            q = bind_cell!(q, ch);
        }
        for key in &pk {
            q = bind_cell!(q, key);
        }
        let res = q
            .execute(pool)
            .await
            .map_err(|e| DbError::with_source("failed to update the row", e))?;
        Ok(res.rows_affected())
    }

    async fn insert_row(&self, table: TableRef, values: Vec<CellChange>) -> Result<u64, DbError> {
        let pool = self.pool().await?;
        if values.is_empty() {
            return Ok(0);
        }
        let ident = pg_ident(&table.schema, &table.name);
        let cols = values
            .iter()
            .map(|v| format!("\"{}\"", v.column.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        let marks = (1..=values.len())
            .map(|i| format!("${i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("INSERT INTO {ident} ({cols}) VALUES ({marks})");
        let mut q = sqlx::query(&sql);
        for v in &values {
            q = bind_cell!(q, v);
        }
        let res = q
            .execute(pool)
            .await
            .map_err(|e| DbError::with_source("failed to insert the row", e))?;
        Ok(res.rows_affected())
    }

    async fn delete_row(&self, table: TableRef, pk: Vec<CellChange>) -> Result<u64, DbError> {
        let pool = self.pool().await?;
        let ident = pg_ident(&table.schema, &table.name);
        let mut where_ = String::new();
        for (i, key) in pk.iter().enumerate() {
            if i > 0 {
                where_.push_str(" AND ");
            }
            where_.push_str(&format!("\"{}\" = ${}", key.column.replace('"', "\"\""), i + 1));
        }
        let sql = format!("DELETE FROM {ident} WHERE {where_}");
        let mut q = sqlx::query(&sql);
        for key in &pk {
            q = bind_cell!(q, key);
        }
        let res = q
            .execute(pool)
            .await
            .map_err(|e| DbError::with_source("failed to delete the row", e))?;
        Ok(res.rows_affected())
    }
}

/// Introspect one schema: relations with batched columns and primary keys.
async fn schema_meta(pool: &PgPool, schema: String) -> Result<SchemaMeta, DbError> {
    // relkind: 'r' table, 'p' partitioned table, 'v' view, 'm' matview.
    let rels: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.relname, c.relkind::text AS relkind \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm') \
         ORDER BY c.relname",
    )
    .bind(&schema)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::with_source(format!("failed to list relations of {schema}"), e))?;

    let cols: Vec<
        (String, String, String, String, Option<i32>, Option<i32>, Option<i32>, Option<String>, String),
    > = sqlx::query_as(
        "SELECT table_name, column_name, udt_name, is_nullable, \
                character_maximum_length, numeric_precision, numeric_scale, \
                column_default, is_identity::text \
         FROM information_schema.columns \
         WHERE table_schema = $1 \
         ORDER BY table_name, ordinal_position",
    )
    .bind(&schema)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::with_source(format!("failed to read columns of {schema}"), e))?;

    let pks: Vec<(String, String)> = sqlx::query_as(
        "SELECT tc.table_name, kcu.column_name \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
           ON kcu.constraint_name = tc.constraint_name \
          AND kcu.table_schema = tc.table_schema \
         WHERE tc.constraint_type = 'PRIMARY KEY' AND tc.table_schema = $1",
    )
    .bind(&schema)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::with_source(format!("failed to read primary keys of {schema}"), e))?;

    let funcs: Vec<(String, String)> = sqlx::query_as(
        "SELECT routine_name, data_type \
         FROM information_schema.routines \
         WHERE routine_schema = $1 AND routine_type = 'FUNCTION' \
         ORDER BY routine_name",
    )
    .bind(&schema)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::with_source(format!("failed to list functions of {schema}"), e))?;

    let pk_set: HashSet<(String, String)> = pks.into_iter().collect();

    // Columns grouped per table, in ordinal order.
    let mut by_table: BTreeMap<String, Vec<ColumnMeta>> = BTreeMap::new();
    for (t, c, udt, nullable, char_len, num_prec, num_scale, default, identity) in cols {
        let is_identity = identity == "YES";
        let is_serial = default
            .as_deref()
            .is_some_and(|d| d.starts_with("nextval("));
        by_table.entry(t).or_default().push(ColumnMeta {
            name: c,
            type_: type_label(&udt, char_len, num_prec, num_scale),
            nullable: nullable == "YES",
            primary_key: false,
            auto_increment: is_identity || is_serial,
            has_default: is_identity || default.is_some(),
            kind: pg_kind(&udt),
        });
    }

    let mut tables: Vec<Arc<TableMeta>> = Vec::new();
    let mut views: Vec<Arc<TableMeta>> = Vec::new();
    for (name, relkind) in rels {
        let mut columns = by_table.remove(&name).unwrap_or_default();
        for c in &mut columns {
            c.primary_key = pk_set.contains(&(name.clone(), c.name.clone()));
        }
        let is_view = relkind == "v" || relkind == "m";
        let meta = Arc::new(TableMeta {
            name: name.clone(),
            schema: schema.clone(),
            kind: if is_view { NodeKind::View } else { NodeKind::Table },
            // No COUNT(*) at introspection time; see the module docs.
            row_count: 0,
            columns,
        });
        if is_view {
            views.push(meta);
        } else {
            tables.push(meta);
        }
    }

    let functions = funcs
        .into_iter()
        .map(|(name, ret)| {
            Arc::new(TableMeta {
                name,
                schema: schema.clone(),
                kind: NodeKind::Function,
                row_count: 0,
                columns: vec![ColumnMeta {
                    name: "returns".into(),
                    type_: ret.clone(),
                    nullable: true,
                    primary_key: false,
                    auto_increment: false,
                    has_default: false,
                    kind: pg_kind(&ret),
                }],
            })
        })
        .collect();

    Ok(SchemaMeta {
        name: schema,
        tables,
        views,
        functions,
    })
}
