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
        // One round trip for every object and its columns: the
        // `pragma_table_info` table-valued function joined per object
        // (instead of one `PRAGMA table_info` query per table).
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            i32,
            Option<String>,
            i32,
        )> = sqlx::query_as(
                "SELECT m.name, m.type, m.sql, p.name, p.type, p.\"notnull\", p.dflt_value, p.pk \
                 FROM sqlite_schema AS m, pragma_table_info(m.name) AS p \
                 WHERE m.type IN ('table', 'view') AND m.name NOT LIKE 'sqlite_%' \
                 ORDER BY m.name, p.cid",
            )
            .fetch_all(pool)
            .await
            .map_err(|e| DbError::with_source("failed to list SQLite objects", e))?;

        let mut tables: Vec<Arc<TableMeta>> = Vec::new();
        let mut views: Vec<Arc<TableMeta>> = Vec::new();
        // (name, is_view, rowid table, columns) of the object being collected.
        let mut current: Option<(String, bool, bool, Vec<ColumnMeta>)> = None;
        let mut flush = |obj: Option<(String, bool, bool, Vec<ColumnMeta>)>| {
            let Some((name, is_view, has_rowid, mut columns)) = obj else {
                return;
            };
            if !is_view && has_rowid {
                mark_rowid_alias(&mut columns);
            }
            let meta = Arc::new(TableMeta {
                name,
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
        };
        for (obj, ty, sql, col, declared, notnull, dflt, pk) in rows {
            if current.as_ref().is_none_or(|(name, ..)| name != &obj) {
                flush(current.take());
                let has_rowid = !sql
                    .unwrap_or_default()
                    .to_ascii_uppercase()
                    .contains("WITHOUT ROWID");
                current = Some((obj, ty == "view", has_rowid, Vec::new()));
            }
            if let Some((.., columns)) = current.as_mut() {
                columns.push(column_meta(col, declared, notnull, dflt, pk));
            }
        }
        flush(current.take());
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
        // One round trip: `pragma_foreign_key_list` joined per table. A bare
        // `REFERENCES t` (no column) has a NULL `to` — it means t's primary
        // key, resolved here with a correlated `pragma_table_info`.
        let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT m.name, f.\"table\", f.\"from\", \
                    COALESCE(f.\"to\", (SELECT p.name FROM pragma_table_info(f.\"table\") AS p \
                                         WHERE p.pk = 1)) \
             FROM sqlite_schema AS m, pragma_foreign_key_list(m.name) AS f \
             WHERE m.type = 'table' AND m.name NOT LIKE 'sqlite_%' \
             ORDER BY m.name, f.id, f.seq",
        )
        .fetch_all(pool)
        .await
        .map_err(|e| DbError::with_source("failed to read SQLite foreign keys", e))?;
        Ok(rows
            .into_iter()
            .filter_map(|(from_table, to_table, from_column, to_column)| {
                Some(Relation {
                    from_table,
                    from_column,
                    to_table,
                    to_column: to_column?,
                })
            })
            .collect())
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
        // `IS`, not `=`: SQLite lets non-INTEGER primary keys hold NULL,
        // and `= NULL` never matches such a row.
        let where_ = pk
            .iter()
            .map(|key| format!("\"{}\" IS ?", key.column.replace('"', "\"\"")))
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
        // `IS`, not `=`: SQLite lets non-INTEGER primary keys hold NULL,
        // and `= NULL` never matches such a row.
        let where_ = pk
            .iter()
            .map(|key| format!("\"{}\" IS ?", key.column.replace('"', "\"\"")))
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

    async fn drop_table(&self, table: TableRef) -> Result<(), DbError> {
        let Some(keyword) = table.kind.drop_keyword() else {
            return Err(DbError::new(format!("{} cannot be dropped from here", table.name)));
        };
        let pool = self.pool().await?;
        let sql = format!("DROP {keyword} {}", sqlite_ident(&table.name));
        sqlx::query(&sql)
            .execute(pool)
            .await
            .map_err(|e| DbError::with_source(format!("failed to drop {}", table.name), e))?;
        Ok(())
    }
}

/// `PRAGMA table_info` per table: cid, name, type, notnull, dflt_value, pk.
/// One `pragma_table_info` row → [`ColumnMeta`].
fn column_meta(
    name: String,
    declared: Option<String>,
    notnull: i32,
    dflt: Option<String>,
    pk: i32,
) -> ColumnMeta {
    let declared = declared.unwrap_or_default();
    let label = if declared.is_empty() {
        "BLOB".to_string()
    } else {
        declared.clone()
    };
    let kind = sqlite_kind(&declared);
    ColumnMeta {
        name,
        type_: label,
        // SQLite tolerates NULL in non-INTEGER primary keys (a legacy quirk);
        // treat keys as required so the editor never creates such rows.
        nullable: notnull == 0 && pk == 0,
        primary_key: pk > 0,
        // Set afterwards by `mark_rowid_alias` — it needs the whole table.
        auto_increment: false,
        has_default: dflt.is_some(),
        kind,
    }
}

/// SQLite generates a value on insert only for the rowid alias: a table's
/// *sole* primary-key column declared exactly `INTEGER` (not `INT`,
/// `BIGINT`, …) in a rowid table. Every other `PRIMARY KEY` must be given.
fn mark_rowid_alias(columns: &mut [ColumnMeta]) {
    let mut pks = columns.iter_mut().filter(|c| c.primary_key);
    let (Some(only), None) = (pks.next(), pks.next()) else {
        return;
    };
    if only.type_.trim().eq_ignore_ascii_case("integer") {
        only.auto_increment = true;
        only.has_default = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, declared: &str, pk: i32) -> ColumnMeta {
        column_meta(name.into(), Some(declared.into()), 1, None, pk)
    }

    #[test]
    fn only_sole_integer_pk_is_rowid_alias() {
        let mut int_pk = vec![col("CategoryID", "INT", 1), col("name", "TEXT", 0)];
        mark_rowid_alias(&mut int_pk);
        assert!(!int_pk[0].auto_increment);

        let mut integer_pk = vec![col("id", "INTEGER", 1)];
        mark_rowid_alias(&mut integer_pk);
        assert!(integer_pk[0].auto_increment);

        let mut composite = vec![col("a", "INTEGER", 1), col("b", "INTEGER", 2)];
        mark_rowid_alias(&mut composite);
        assert!(composite.iter().all(|c| !c.auto_increment));
    }
}
