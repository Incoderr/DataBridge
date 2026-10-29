//! MySQL driver: `DbSession` over a lazily-connected sqlx `MySqlPool`.
//!
//! MySQL "schemas" are databases. Introspection runs in batches per
//! schema; row counts are not fetched at introspection time (the grid
//! computes the real total on the first page load).

use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions};
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

pub struct MySqlSession {
    opts: MySqlConnectOptions,
    /// When set, parsed natively by sqlx (preserving query options).
    url: Option<String>,
    pool: OnceCell<MySqlPool>,
}

impl MySqlSession {
    pub fn new(conn: &DbConnection) -> Self {
        let mut opts = MySqlConnectOptions::new()
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

    async fn pool(&self) -> Result<&MySqlPool, DbError> {
        self.pool
            .get_or_try_init(|| async {
                let opts = match &self.url {
                    Some(url) => MySqlConnectOptions::from_str(url).map_err(|e| {
                        DbError::with_source("invalid MySQL connection URL", e)
                    })?,
                    None => self.opts.clone(),
                };
                MySqlPoolOptions::new()
                    .max_connections(4)
                    .acquire_timeout(Duration::from_secs(8))
                    .connect_with(opts)
                    .await
                    .map_err(|e| DbError::with_source("failed to connect to MySQL", e))
            })
            .await
    }
}

/// `` `schema`.`table` `` with embedded backticks doubled. Both parts come
/// from our own introspection, never from user input.
fn mysql_ident(schema: &str, name: &str) -> String {
    format!(
        "`{}`.`{}`",
        schema.replace('`', "``"),
        name.replace('`', "``")
    )
}

/// `column_type` carries the width (`tinyint(1)`); `data_type` is the
/// bare type name.
fn mysql_kind(column_type: &str, data_type: &str) -> ColumnKind {
    if column_type == "tinyint(1)" {
        return ColumnKind::Bool;
    }
    match data_type {
        "tinyint" | "smallint" | "mediumint" | "int" | "bigint" | "year" => ColumnKind::Int,
        "decimal" | "numeric" | "float" | "double" => ColumnKind::Float,
        "json" => ColumnKind::Json,
        "binary" | "varbinary" | "blob" | "tinyblob" | "mediumblob" | "longblob" => {
            ColumnKind::Binary
        }
        "date" | "datetime" | "timestamp" | "time" => ColumnKind::Date,
        _ => ColumnKind::Text,
    }
}

#[async_trait]
impl DbSession for MySqlSession {
    async fn ping(&self) -> Result<Ping, DbError> {
        let pool = self.pool().await?;
        let t0 = Instant::now();
        sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(pool)
            .await
            .map_err(|e| DbError::with_source("MySQL ping failed", e))?;
        Ok(Ping {
            status: ConnectionStatus::Connected,
            latency_ms: t0.elapsed().as_millis() as u32,
        })
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaMeta>, DbError> {
        let pool = self.pool().await?.clone();
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys') \
             ORDER BY schema_name",
        )
        .fetch_all(&pool)
        .await
        .map_err(|e| DbError::with_source("failed to list MySQL databases", e))?;
        let mut out = Vec::with_capacity(names.len());
        for name in names {
            out.push(schema_meta(&pool, name).await?);
        }
        Ok(out)
    }

    async fn list_relations(&self, schema: &str) -> Result<Vec<Relation>, DbError> {
        let pool = self.pool().await?;
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT table_name, column_name, referenced_table_name, referenced_column_name \
             FROM information_schema.key_column_usage \
             WHERE table_schema = ? AND referenced_table_name IS NOT NULL",
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
        let ident = mysql_ident(&table.schema, &table.name);
        let total: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {ident}"))
            .fetch_one(pool)
            .await
            .map_err(|e| {
                DbError::with_source(format!("failed to count {}.{}", table.schema, table.name), e)
            })?;
        let rows = sqlx::query(&format!("SELECT * FROM {ident} LIMIT ? OFFSET ?"))
            .bind(limit)
            .bind(offset)
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
        let columns: Vec<ColumnMeta> = rows
            .first()
            .map(|r| {
                r.columns()
                    .iter()
                    .map(|c| {
                        let t = c.type_info().name().to_lowercase();
                        ColumnMeta {
                            name: c.name().to_string(),
                            type_: c.type_info().to_string(),
                            nullable: true,
                            primary_key: false,
                            auto_increment: false,
                            has_default: false,
                            kind: mysql_kind(&t, &t),
                        }
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
        let ident = mysql_ident(&table.schema, &table.name);
        let sets = changes
            .iter()
            .map(|ch| format!("`{}` = ?", ch.column.replace('`', "``")))
            .collect::<Vec<_>>()
            .join(", ");
        let where_ = pk
            .iter()
            .map(|key| format!("`{}` = ?", key.column.replace('`', "``")))
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
        let ident = mysql_ident(&table.schema, &table.name);
        let cols = values
            .iter()
            .map(|v| format!("`{}`", v.column.replace('`', "``")))
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
        let ident = mysql_ident(&table.schema, &table.name);
        let where_ = pk
            .iter()
            .map(|key| format!("`{}` = ?", key.column.replace('`', "``")))
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
        let sql = format!("DROP {keyword} {}", mysql_ident(&table.schema, &table.name));
        sqlx::query(&sql)
            .execute(pool)
            .await
            .map_err(|e| DbError::with_source(format!("failed to drop {}", table.name), e))?;
        Ok(())
    }
}

/// Introspect one database: tables/views with batched columns and keys.
async fn schema_meta(pool: &MySqlPool, schema: String) -> Result<SchemaMeta, DbError> {
    let rels: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name, table_type FROM information_schema.tables \
         WHERE table_schema = ? AND table_type IN ('BASE TABLE', 'VIEW') \
         ORDER BY table_name",
    )
    .bind(&schema)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::with_source(format!("failed to list tables of {schema}"), e))?;

    let cols: Vec<(String, String, String, String, String, Option<String>, String)> =
        sqlx::query_as(
            "SELECT table_name, column_name, column_type, data_type, is_nullable, \
                    column_default, extra \
             FROM information_schema.columns \
             WHERE table_schema = ? \
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
         WHERE tc.constraint_type = 'PRIMARY KEY' AND tc.table_schema = ?",
    )
    .bind(&schema)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::with_source(format!("failed to read primary keys of {schema}"), e))?;

    let funcs: Vec<(String, String)> = sqlx::query_as(
        "SELECT routine_name, data_type \
         FROM information_schema.routines \
         WHERE routine_schema = ? AND routine_type = 'FUNCTION' \
         ORDER BY routine_name",
    )
    .bind(&schema)
    .fetch_all(pool)
    .await
    .map_err(|e| DbError::with_source(format!("failed to list functions of {schema}"), e))?;

    let pk_set: HashSet<(String, String)> = pks.into_iter().collect();

    let mut by_table: BTreeMap<String, Vec<ColumnMeta>> = BTreeMap::new();
    for (t, c, column_type, data_type, nullable, default, extra) in cols {
        let auto_increment = extra.to_ascii_lowercase().contains("auto_increment");
        by_table.entry(t).or_default().push(ColumnMeta {
            name: c,
            type_: column_type.clone(),
            nullable: nullable == "YES",
            primary_key: false,
            auto_increment,
            has_default: auto_increment || default.is_some(),
            kind: mysql_kind(&column_type, &data_type),
        });
    }

    let mut tables: Vec<Arc<TableMeta>> = Vec::new();
    let mut views: Vec<Arc<TableMeta>> = Vec::new();
    for (name, table_type) in rels {
        let mut columns = by_table.remove(&name).unwrap_or_default();
        for c in &mut columns {
            c.primary_key = pk_set.contains(&(name.clone(), c.name.clone()));
        }
        let is_view = table_type == "VIEW";
        let meta = Arc::new(TableMeta {
            name: name.clone(),
            schema: schema.clone(),
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
                    kind: mysql_kind(&ret, &ret),
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
