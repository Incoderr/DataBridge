//! SQLite driver: `DbSession` over a lazily-connected sqlx `SqlitePool`.
//!
//! A SQLite file has exactly one schema, exposed as `main`. Column types
//! use PRAGMA table_info over the introspected (validated) table names —
//! PRAGMA takes no bound parameters, so the identifier is quoted with
//! embedded quotes doubled.

use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
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

pub struct SqliteSession {
    opts: SqliteConnectOptions,
    /// When set, parsed natively by sqlx (preserving query options).
    url: Option<String>,
    pool: OnceCell<SqlitePool>,
}

impl SqliteSession {
    /// `conn.database` holds the file path (the wizard's "File path" field).
    pub fn new(conn: &DbConnection) -> Self {
        let opts = SqliteConnectOptions::new()
            .filename(&conn.database)
            .create_if_missing(false);
        Self {
            opts,
            url: conn.url.clone(),
            pool: OnceCell::new(),
        }
    }

    async fn pool(&self) -> Result<&SqlitePool, DbError> {
        self.pool
            .get_or_try_init(|| async {
                let opts = match &self.url {
                    Some(url) => SqliteConnectOptions::from_str(url).map_err(|e| {
                        DbError::with_source("invalid SQLite connection URL", e)
                    })?,
                    None => self.opts.clone(),
                };
                SqlitePoolOptions::new()
                    .max_connections(4)
                    .acquire_timeout(Duration::from_secs(8))
                    .connect_with(opts)
                    .await
                    .map_err(|e| DbError::with_source("failed to open the SQLite file", e))
            })
            .await
    }
}

/// `"name"` with embedded quotes doubled; comes from our introspection.
fn sqlite_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Type affinity per the SQLite docs, applied to the declared type.
fn sqlite_kind(declared: &str) -> ColumnKind {
    let t = declared.to_uppercase();
    if t.contains("INT") {
        ColumnKind::Int
    } else if t.contains("CHAR") || t.contains("CLOB") || t.contains("TEXT") {
        ColumnKind::Text
    } else if t.contains("BLOB") || t.is_empty() {
        ColumnKind::Binary
    } else if t.contains("REAL") || t.contains("FLOA") || t.contains("DOUB") {
        ColumnKind::Float
    } else if t.contains("DATE") || t.contains("TIME") {
        ColumnKind::Date
    } else if t.contains("BOOL") {
        ColumnKind::Bool
    } else if t.contains("JSON") {
        ColumnKind::Json
    } else {
        ColumnKind::Text
    }
}

#[async_trait]
impl DbSession for SqliteSession {
    async fn ping(&self) -> Result<Ping, DbError> {
        let pool = self.pool().await?;
        let t0 = Instant::now();
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(pool)
            .await
            .map_err(|e| DbError::with_source("SQLite ping failed", e))?;
        Ok(Ping {
            status: ConnectionStatus::Connected,
            latency_ms: t0.elapsed().as_millis() as u32,
        })
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaMeta>, DbError> {
        let pool = self.pool().await?;
        let rels: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, type FROM sqlite_schema \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::with_source("failed to list SQLite objects", e))?;

        let mut tables: Vec<Arc<TableMeta>> = Vec::new();
        let mut views: Vec<Arc<TableMeta>> = Vec::new();
        for (name, ty) in rels {
            let columns = sqlite_columns(pool, &name).await?;
            let is_view = ty == "view";
            let meta = Arc::new(TableMeta {
                name: name.clone(),
                schema: "main".into(),
                kind: if is_view { NodeKind::View } else { NodeKind::Table },
                row_count: 0,
                columns,
            });
            if is_view {
                views.push(meta);
            } else {
                tables.push(meta);
            }
        }
        Ok(vec![SchemaMeta {
            name: "main".into(),
            tables,
            views,
            functions: Vec::new(),
        }])
    }

    async fn list_relations(&self, schema: &str) -> Result<Vec<Relation>, DbError> {
        let pool = self.pool().await?;
        if schema != "main" {
            return Ok(Vec::new());
        }
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_schema \
             WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%'",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::with_source("failed to list SQLite objects", e))?;
        let mut out = Vec::new();
        for t in tables {
            // PRAGMA takes no bound parameters: the table name comes from
            // our own introspection and is quoted with embedded quotes doubled.
            let rows = sqlx::query(&format!(
                "PRAGMA foreign_key_list({})",
                sqlite_ident(&t)
            ))
            .fetch_all(pool)
            .await
            .map_err(|e| {
                DbError::with_source(format!("failed to read foreign keys of {t}"), e)
            })?;
            for row in rows {
                // columns: id, seq, table, from, to, on_update, on_delete, match
                let to_table: String = row
                    .try_get(2)
                    .map_err(|e| DbError::with_source(format!("bad FK row in {t}"), e))?;
                let from_col: String = row
                    .try_get(3)
                    .map_err(|e| DbError::with_source(format!("bad FK row in {t}"), e))?;
                let to_col: Option<String> = row
                    .try_get(4)
                    .map_err(|e| DbError::with_source(format!("bad FK row in {t}"), e))?;
                if let Some(to_col) = to_col {
                    out.push(Relation {
                        from_table: t.clone(),
                        from_column: from_col,
                        to_table,
                        to_column: to_col,
                    });
                }
            }
        }
        Ok(out)
    }

    async fn fetch_rows(&self, table: TableRef, offset: u64, limit: u64) -> Result<Page, DbError> {
        let pool = self.pool().await?;
        let t0 = Instant::now();
        let ident = sqlite_ident(&table.name);
        let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {ident}"))
            .fetch_one(pool)
            .await
            .map_err(|e| DbError::with_source(format!("failed to count {}", table.name), e))?;
        let rows = sqlx::query(&format!("SELECT * FROM {ident} LIMIT ? OFFSET ?"))
            .bind(limit as i64)
            .bind(offset as i64)
            .fetch_all(pool)
            .await
            .map_err(|e| DbError::with_source(format!("failed to read {}", table.name), e))?;
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
        let columns: Vec<ColumnMeta> = rows
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
                        kind: sqlite_kind(c.type_info().name()),
                    })
                    .collect()
            })
            .unwrap_or_default();
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
        let ident = sqlite_ident(&table.name);
        let sets = changes
            .iter()
            .map(|ch| format!("\"{}\" = ?", ch.column.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        let where_ = pk
            .iter()
            .map(|key| format!("\"{}\" = ?", key.column.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
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
        let ident = sqlite_ident(&table.name);
        let cols = values
            .iter()
            .map(|v| format!("\"{}\"", v.column.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        let marks = vec!["?"; values.len()].join(", ");
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
        let ident = sqlite_ident(&table.name);
        let where_ = pk
            .iter()
            .map(|key| format!("\"{}\" = ?", key.column.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
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

/// `PRAGMA table_info` per table: cid, name, type, notnull, dflt_value, pk.
async fn sqlite_columns(pool: &SqlitePool, table: &str) -> Result<Vec<ColumnMeta>, DbError> {
    let rows = sqlx::query(&format!(
        "PRAGMA table_info({})",
        sqlite_ident(table)
    ))
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::with_source(format!("failed to read columns of {table}"), e))?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let name: String = row
            .try_get(1)
            .map_err(|e| DbError::with_source(format!("bad column name in {table}"), e))?;
        let declared: Option<String> = row.try_get(2).map_err(|e| {
            DbError::with_source(format!("bad column type in {table}"), e)
        })?;
        let notnull: i32 = row
            .try_get(3)
            .map_err(|e| DbError::with_source(format!("bad notnull flag in {table}"), e))?;
        let pk: i32 = row
            .try_get(5)
            .map_err(|e| DbError::with_source(format!("bad pk flag in {table}"), e))?;
        let dflt: Option<String> = row.try_get(4).map_err(|e| {
            DbError::with_source(format!("bad default value in {table}"), e)
        })?;
        let declared = declared.unwrap_or_default();
        let label = if declared.is_empty() {
            "BLOB".to_string()
        } else {
            declared.clone()
        };
        let kind = sqlite_kind(&declared);
        // `INTEGER PRIMARY KEY` is the rowid alias: SQLite generates it
        // on insert when omitted.
        let auto_increment = pk > 0 && kind == ColumnKind::Int;
        out.push(ColumnMeta {
            name,
            type_: label,
            nullable: notnull == 0,
            primary_key: pk > 0,
            auto_increment,
            has_default: auto_increment || dflt.is_some(),
            kind,
        });
    }
    Ok(out)
}
