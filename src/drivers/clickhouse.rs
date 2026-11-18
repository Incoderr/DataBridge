//! ClickHouse driver over the HTTP interface (port 8123, TLS 8443) —
//! `reqwest` is already in the tree, so like MongoDB it has no cargo
//! feature and no driver crate.
//!
//! Every statement is one POST (body = SQL) answered in
//! `JSONCompactEachRowWithNamesAndTypes`: a names line, a types line, then
//! one JSON array per row. The body is read as a stream and dropped once
//! the editor's limit is reached (the server aborts on the closed socket),
//! so `SELECT *` on a huge table never buffers it all. A body that is not
//! that format (the user wrote `FORMAT CSV`) comes back as one `result`
//! text column.
//!
//! User input never reaches SQL text: grid filter values travel as
//! ClickHouse query parameters (`{f0:String}` + `param_f0=…`), inserts as
//! `FORMAT JSONEachRow` data; identifiers come from introspection and are
//! backtick-quoted. Databases → schemas (`system`/`INFORMATION_SCHEMA`
//! hidden), tables / views (View, MaterializedView, …) from one pass over
//! `system.tables` + `system.columns`. MATERIALIZED / ALIAS / EPHEMERAL
//! columns are left out: `SELECT *` does not return them and grid rows are
//! ordered by the table's columns.
//!
//! ClickHouse has no unique primary key (the sorting key may repeat), so
//! no column is marked `primary_key`: the record editor inserts but does
//! not offer edit / delete — those would be mutations hitting every row
//! with the same key. Stop kills the statement by its `query_id`
//! (`KILL QUERY … ASYNC`). Unfiltered totals come from
//! `system.tables.total_rows` (exact for MergeTree), the rest `count()`.

use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::core::error::DbError;
use crate::core::model::{
    CellChange, ColumnKind, ColumnMeta, ConnectionStatus, DbConnection, FilterOp, NodeKind, Page,
    Ping, QueryPage, Relation, RowQuery, SchemaMeta, TableMeta, TableRef, Value,
    total_without_count,
};
use crate::core::session::{CancelToken, DbSession, cancellable};

const HIDDEN_DBS: &str = "('system', 'INFORMATION_SCHEMA', 'information_schema')";
const FORMAT: &str = "JSONCompactEachRowWithNamesAndTypes";

/// Where and as whom to connect.
#[derive(Debug, Clone, PartialEq)]
struct Endpoint {
    /// `http(s)://host:port/`
    base: String,
    user: String,
    password: String,
    database: String,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn default_port(https: bool) -> u16 {
    if https { 8443 } else { 8123 }
}

fn native_port_error(port: u16) -> Option<DbError> {
    matches!(port, 9000 | 9440).then(|| {
        DbError::new(format!(
            "port {port} is ClickHouse's native protocol — DataBridge uses the HTTP interface \
             (8123, or 8443 with TLS)"
        ))
    })
}

/// `clickhouse://` / `ch://` (http; `?secure=true` for https),
/// `clickhouses://`, `http://`, `https://`; `user:password@host:port/db`.
fn parse_url(url: &str) -> Result<Endpoint, DbError> {
    let bad = || DbError::new("invalid ClickHouse URL — expected clickhouse://user:password@host:8123/database");
    let (scheme, rest) = url.trim().split_once("://").ok_or_else(bad)?;
    let (authority, tail) = match rest.find(['/', '?', '#']) {
        Some(ix) => rest.split_at(ix),
        None => (rest, ""),
    };
    let (path, query) = match tail.split_once('?') {
        Some((p, q)) => (p, q.split('#').next().unwrap_or("")),
        None => (tail.split('#').next().unwrap_or(""), ""),
    };
    let flag = |name: &str| {
        query.split('&').filter_map(|kv| kv.split_once('=')).find_map(|(k, v)| {
            k.eq_ignore_ascii_case(name).then(|| percent_decode(v))
        })
    };
    let secure = ["secure", "ssl", "tls"]
        .iter()
        .filter_map(|k| flag(k))
        .any(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"));
    let scheme = scheme.to_ascii_lowercase();
    let https = match scheme.as_str() {
        "https" | "clickhouses" => true,
        "http" | "clickhouse" | "ch" => secure,
        _ => return Err(bad()),
    };
    // Plain web URLs keep the web's default ports (a proxy / the public
    // playground on 443); clickhouse:// means the server's own 8123/8443.
    let web_port = match scheme.as_str() {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    };
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let (user, password) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((u, p)) => (percent_decode(u), percent_decode(p)),
            None => (percent_decode(u), String::new()),
        },
        None => (String::new(), String::new()),
    };
    let (host, port) = if let Some(close) = hostport.find(']') {
        let (h, t) = hostport.split_at(close + 1);
        (h.to_string(), t.strip_prefix(':').and_then(|p| p.parse().ok()))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.parse::<u16>().map_err(|_| bad())?)),
            None => (hostport.to_string(), None),
        }
    };
    if host.is_empty() {
        return Err(bad());
    }
    let port = port.or(web_port).unwrap_or(default_port(https));
    if let Some(e) = native_port_error(port) {
        return Err(e);
    }
    let database = match path.trim_matches('/') {
        "" => flag("database").unwrap_or_default(),
        db => percent_decode(db),
    };
    Ok(Endpoint {
        base: format!("{}://{host}:{port}/", if https { "https" } else { "http" }),
        user: if user.is_empty() { "default".into() } else { user },
        password,
        database: if database.is_empty() { "default".into() } else { database },
    })
}

fn endpoint(conn: &DbConnection) -> Result<Endpoint, DbError> {
    if let Some(url) = &conn.url {
        return parse_url(url);
    }
    let port = if conn.port == 0 { 8123 } else { conn.port };
    if let Some(e) = native_port_error(port) {
        return Err(e);
    }
    let https = matches!(port, 443 | 8443);
    let host = if conn.host.contains(':') && !conn.host.starts_with('[') {
        format!("[{}]", conn.host)
    } else {
        conn.host.clone()
    };
    Ok(Endpoint {
        base: format!("{}://{host}:{port}/", if https { "https" } else { "http" }),
        user: if conn.user.is_empty() { "default".into() } else { conn.user.clone() },
        password: conn.password.clone(),
        database: if conn.database.is_empty() { "default".into() } else { conn.database.clone() },
    })
}

/// `` `db`.`table` `` — both from introspection; backticks / backslashes escaped.
fn ident(name: &str) -> String {
    format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
}

fn table_ident(t: &TableRef) -> String {
    format!("{}.{}", ident(&t.schema), ident(&t.name))
}

/// Query-parameter text is read in ClickHouse's escaped (TSV) format.
fn escape_param(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
}

/// `Nullable(LowCardinality(String))` → `String`.
fn base_type(t: &str) -> &str {
    let mut t = t.trim();
    loop {
        let inner = ["Nullable(", "LowCardinality(", "SimpleAggregateFunction("]
            .iter()
            .find_map(|w| t.strip_prefix(w).and_then(|r| r.strip_suffix(')')));
        match inner {
            Some(i) => t = i.trim(),
            None => return t,
        }
    }
}

fn is_nullable(t: &str) -> bool {
    t.contains("Nullable(")
}

fn kind_of(t: &str) -> ColumnKind {
    let b = base_type(t);
    if b.starts_with("Interval") {
        ColumnKind::Text
    } else if b.starts_with("Int") || b.starts_with("UInt") {
        ColumnKind::Int
    } else if b.starts_with("Float") || b.starts_with("Decimal") || b.starts_with("BFloat") {
        ColumnKind::Float
    } else if b == "Bool" {
        ColumnKind::Bool
    } else if b.starts_with("Date") {
        ColumnKind::Date
    } else if b == "UUID" {
        ColumnKind::Uuid
    } else if ["Array", "Map", "Tuple", "JSON", "Object", "Nested", "Variant", "Dynamic"]
        .iter()
        .any(|w| b.starts_with(w))
    {
        ColumnKind::Json
    } else {
        ColumnKind::Text
    }
}

/// One JSON cell → `Value`, by the column's ClickHouse type (64-bit ints
/// arrive quoted; values beyond i64 stay text).
fn cell(v: &serde_json::Value, t: &str) -> Value {
    use serde_json::Value as J;
    let kind = kind_of(t);
    match v {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => match (n.as_i64(), kind) {
            (Some(i), ColumnKind::Int | ColumnKind::Bool) => Value::Int(i),
            (_, ColumnKind::Int) => Value::Str(n.to_string()),
            (Some(i), ColumnKind::Text) => Value::Int(i),
            _ => n.as_f64().map(Value::Float).unwrap_or_else(|| Value::Str(n.to_string())),
        },
        J::String(s) => match kind {
            ColumnKind::Int => s.parse::<i64>().map(Value::Int).unwrap_or_else(|_| Value::Str(s.clone())),
            ColumnKind::Float => s.parse::<f64>().ok().filter(|f| f.is_finite()).map(Value::Float)
                .unwrap_or_else(|| Value::Str(s.clone())),
            _ => Value::Str(s.clone()),
        },
        other => Value::Str(other.to_string()),
    }
}

fn column(name: String, t: String) -> ColumnMeta {
    ColumnMeta {
        nullable: is_nullable(&t),
        kind: kind_of(&t),
        name,
        type_: t,
        primary_key: false,
        auto_increment: false,
        has_default: false,
    }
}

/// `Code: 60. DB::Exception: Table x does not exist. (UNKNOWN_TABLE) (version 24.3.1.1)`
/// → without the version tail.
fn clean_error(text: &str) -> String {
    let t = text.trim();
    let t = match t.rfind(" (version ") {
        Some(ix) if t.ends_with(')') => &t[..ix],
        _ => t,
    };
    let t: String = t.chars().take(1000).collect();
    if t.is_empty() { "ClickHouse returned an error".into() } else { t }
}

/// A decoded response: header lines plus rows (or none for DDL / DML).
#[derive(Debug, Default)]
struct ResultSet {
    names: Vec<String>,
    types: Vec<String>,
    rows: Vec<Vec<Value>>,
    /// More rows existed than `max` (the read stopped).
    truncated: bool,
}

/// Incremental parser of the `…WithNamesAndTypes` stream (pure — tested).
#[derive(Default)]
struct Decoder {
    buf: Vec<u8>,
    set: ResultSet,
    line: usize,
    /// The body is not JSON lines (explicit `FORMAT …`): plain text rows.
    raw: bool,
}

impl Decoder {
    /// Feed bytes; `Ok(true)` = enough rows, stop reading.
    fn feed(&mut self, chunk: &[u8], max: usize) -> Result<bool, DbError> {
        self.buf.extend_from_slice(chunk);
        while let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            if self.take_line(&line[..line.len() - 1], max)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn finish(mut self, max: usize) -> Result<ResultSet, DbError> {
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            self.take_line(&rest, max)?;
        }
        Ok(self.set)
    }

    fn take_line(&mut self, line: &[u8], max: usize) -> Result<bool, DbError> {
        let text = String::from_utf8_lossy(line);
        let text = text.trim_end_matches('\r');
        if self.set.rows.len() >= max {
            if !text.is_empty() {
                self.set.truncated = true;
                return Ok(true);
            }
            return Ok(false);
        }
        if self.raw {
            self.set.rows.push(vec![Value::Str(text.to_string())]);
            return Ok(false);
        }
        let parsed: Option<Vec<serde_json::Value>> = serde_json::from_str(text).ok();
        match (self.line, parsed) {
            (0 | 1, Some(arr)) if arr.iter().all(|v| v.is_string()) => {
                let strs = arr.iter().map(|v| v.as_str().unwrap_or_default().to_string()).collect();
                if self.line == 0 { self.set.names = strs } else { self.set.types = strs }
            }
            (0, _) => {
                if text.is_empty() {
                    return Ok(false);
                }
                // Not our format: the user picked one with FORMAT.
                if text.contains("DB::Exception") {
                    return Err(DbError::new(clean_error(text)));
                }
                self.raw = true;
                self.set.names = vec!["result".into()];
                self.set.types = vec!["String".into()];
                self.set.rows.push(vec![Value::Str(text.to_string())]);
            }
            (_, Some(arr)) if self.line >= 2 => {
                let row = arr
                    .iter()
                    .enumerate()
                    .map(|(i, v)| cell(v, self.set.types.get(i).map(String::as_str).unwrap_or("")))
                    .collect();
                self.set.rows.push(row);
            }
            _ if text.is_empty() => {}
            // An exception after the headers (the server already sent 200).
            _ => return Err(DbError::new(clean_error(text))),
        }
        self.line += 1;
        Ok(false)
    }
}

pub struct ClickHouseSession {
    endpoint: Result<Endpoint, DbError>,
    client: OnceCell<reqwest::Client>,
}

impl ClickHouseSession {
    pub fn new(conn: &DbConnection) -> Self {
        Self { endpoint: endpoint(conn), client: OnceCell::new() }
    }

    fn endpoint(&self) -> Result<&Endpoint, DbError> {
        self.endpoint.as_ref().map_err(Clone::clone)
    }

    async fn client(&self) -> Result<&reqwest::Client, DbError> {
        self.client.get_or_try_init(|| async { crate::core::runtime::http_client() }).await
    }

    /// POST one statement; non-2xx answers become the server's message.
    /// `params` are query parameters (`{name:Type}` in the SQL).
    async fn post(
        &self,
        sql: &str,
        params: &[(String, String)],
        query_id: Option<&str>,
        body: Option<String>,
    ) -> Result<reqwest::Response, DbError> {
        let ep = self.endpoint()?;
        let client = self.client().await?;
        let mut q: Vec<(String, String)> = vec![
            ("database".into(), ep.database.clone()),
            ("default_format".into(), FORMAT.into()),
        ];
        if let Some(id) = query_id {
            q.push(("query_id".into(), id.into()));
        }
        q.extend(params.iter().map(|(k, v)| (format!("param_{k}"), escape_param(v))));
        // With a data body the statement goes in the URL (`query=`).
        let body = match body {
            Some(data) => {
                q.push(("query".into(), sql.into()));
                data
            }
            None => sql.to_string(),
        };
        let mut url = reqwest::Url::parse(&ep.base).map_err(|e| {
            DbError::with_source(format!("invalid ClickHouse address {}: {e}", ep.base), e)
        })?;
        url.query_pairs_mut().extend_pairs(&q);
        let resp = client
            .post(url)
            .header("X-ClickHouse-User", &ep.user)
            .header("X-ClickHouse-Key", &ep.password)
            .body(body)
            .send()
            .await
            .map_err(|e| {
                DbError::with_source(format!("cannot reach ClickHouse at {}: {e}", ep.base), e)
            })?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(DbError::new(if text.trim().is_empty() {
            format!("ClickHouse answered {status}")
        } else {
            clean_error(&text)
        }))
    }

    /// Run a statement and decode up to `max` rows.
    async fn query(
        &self,
        sql: &str,
        params: &[(String, String)],
        query_id: Option<&str>,
        max: usize,
    ) -> Result<(ResultSet, u64), DbError> {
        let mut resp = self.post(sql, params, query_id, None).await?;
        let written = resp
            .headers()
            .get("X-ClickHouse-Summary")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
            .and_then(|j| {
                let n = &j["written_rows"];
                n.as_str().and_then(|s| s.parse().ok()).or_else(|| n.as_u64())
            })
            .unwrap_or(0);
        let mut dec = Decoder::default();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| DbError::with_source(format!("ClickHouse connection lost: {e}"), e))?
        {
            if dec.feed(&chunk, max)? {
                break; // dropping the response closes the stream
            }
        }
        Ok((dec.finish(max)?, written))
    }

    async fn rows(&self, sql: &str, params: &[(String, String)]) -> Result<Vec<Vec<Value>>, DbError> {
        Ok(self.query(sql, params, None, usize::MAX).await?.0.rows)
    }
}

fn s(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        other => other.edit_string(),
    }
}

/// ` WHERE …` / ` ORDER BY …` plus the query parameters.
fn clauses(q: &RowQuery) -> (String, String, Vec<(String, String)>) {
    let mut conds = Vec::new();
    let mut params = Vec::new();
    for (i, f) in q.filters.iter().enumerate() {
        let col = ident(&f.column);
        let p = format!("f{i}");
        match f.op {
            FilterOp::IsNull | FilterOp::NotNull => conds.push(format!("{col} {}", f.op.sql())),
            FilterOp::Contains => {
                conds.push(format!("positionCaseInsensitiveUTF8(toString({col}), {{{p}:String}}) > 0"));
                params.push((p, f.value.display()));
            }
            op => {
                let t = match f.value {
                    Value::Int(_) => "Int64",
                    Value::Float(_) => "Float64",
                    Value::Bool(_) => "Bool",
                    _ => "String",
                };
                conds.push(format!("{col} {} {{{p}:{t}}}", op.sql()));
                params.push((p, s(&f.value)));
            }
        }
    }
    let where_ = if conds.is_empty() { String::new() } else { format!(" WHERE {}", conds.join(" AND ")) };
    let order = match &q.sort {
        Some((c, desc)) => format!(" ORDER BY {} {}", ident(c), if *desc { "DESC" } else { "ASC" }),
        None => String::new(),
    };
    (where_, order, params)
}

fn text(v: Option<&Value>) -> String {
    v.map(s).unwrap_or_default()
}

#[async_trait]
impl DbSession for ClickHouseSession {
    async fn ping(&self) -> Result<Ping, DbError> {
        let t0 = Instant::now();
        self.rows("SELECT 1", &[]).await?;
        Ok(Ping { status: ConnectionStatus::Connected, latency_ms: t0.elapsed().as_millis() as u32 })
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaMeta>, DbError> {
        let dbs_sql =
            format!("SELECT name FROM system.databases WHERE name NOT IN {HIDDEN_DBS} ORDER BY name");
        let dbs = self.rows(&dbs_sql, &[]);
        let tables_sql = format!(
            "SELECT database, name, engine, total_rows FROM system.tables \
             WHERE database NOT IN {HIDDEN_DBS} AND NOT is_temporary \
             AND NOT startsWith(name, '.inner') ORDER BY database, name"
        );
        let tables = self.rows(&tables_sql, &[]);
        let cols_sql = format!(
            "SELECT database, table, name, type, default_kind FROM system.columns \
             WHERE database NOT IN {HIDDEN_DBS} ORDER BY database, table, position"
        );
        let cols = self.rows(&cols_sql, &[]);
        let (dbs, tables, cols) = tokio::try_join!(dbs, tables, cols)?;

        let mut by_table: std::collections::HashMap<(String, String), Vec<ColumnMeta>> =
            std::collections::HashMap::new();
        for r in cols {
            let default_kind = text(r.get(4));
            // Not returned by SELECT * (see the module doc).
            if matches!(default_kind.as_str(), "MATERIALIZED" | "ALIAS" | "EPHEMERAL") {
                continue;
            }
            let mut c = column(text(r.get(2)), text(r.get(3)));
            c.has_default = !default_kind.is_empty();
            by_table.entry((text(r.first()), text(r.get(1)))).or_default().push(c);
        }

        let mut schemas: Vec<SchemaMeta> = dbs
            .iter()
            .map(|r| SchemaMeta { name: text(r.first()), tables: vec![], views: vec![], functions: vec![] })
            .collect();
        for r in tables {
            let (db, name, engine) = (text(r.first()), text(r.get(1)), text(r.get(2)));
            let Some(schema) = schemas.iter_mut().find(|s| s.name == db) else { continue };
            let is_view = matches!(engine.as_str(), "View" | "MaterializedView" | "LiveView" | "WindowView");
            let row_count = match r.get(3) {
                Some(Value::Int(n)) => (*n).max(0) as u64,
                Some(Value::Str(s)) => s.parse().unwrap_or(0),
                _ => 0,
            };
            let meta = std::sync::Arc::new(TableMeta {
                columns: by_table.remove(&(db.clone(), name.clone())).unwrap_or_default(),
                name,
                schema: db,
                kind: if is_view { NodeKind::View } else { NodeKind::Table },
                row_count,
            });
            if is_view { schema.views.push(meta) } else { schema.tables.push(meta) }
        }
        Ok(schemas)
    }

    async fn list_relations(&self, _schema: &str) -> Result<Vec<Relation>, DbError> {
        // ClickHouse has no foreign keys.
        Ok(Vec::new())
    }

    async fn fetch_rows(
        &self,
        table: TableRef,
        query: RowQuery,
        offset: u64,
        limit: u64,
    ) -> Result<Page, DbError> {
        let t0 = Instant::now();
        let ident = table_ident(&table);
        let (where_, order, params) = clauses(&query);
        let filtered = query.is_filtered();
        let page_sql = format!("SELECT * FROM {ident}{where_}{order} LIMIT {limit} OFFSET {offset}");
        let page = self.rows(&page_sql, &params);
        // MergeTree keeps an exact row count in system.tables.
        let stored = async {
            if filtered {
                return None;
            }
            let p = [("db".to_string(), table.schema.clone()), ("t".to_string(), table.name.clone())];
            let rows = self
                .rows("SELECT total_rows FROM system.tables WHERE database = {db:String} AND name = {t:String}", &p)
                .await
                .ok()?;
            match rows.first()?.first()? {
                Value::Int(n) => Some((*n).max(0) as u64),
                Value::Str(s) => s.parse().ok(),
                _ => None,
            }
        };
        let (rows, stored) = tokio::join!(page, stored);
        let rows = rows?;
        let (total, exact) = match stored {
            Some(n) => (n.max(offset + rows.len() as u64), true),
            None => match total_without_count(offset, limit, rows.len(), None) {
                Some(t) => t,
                None => {
                    let n = self.rows(&format!("SELECT count() FROM {ident}{where_}"), &params).await?;
                    let n = match n.first().and_then(|r| r.first()) {
                        Some(Value::Int(n)) => (*n).max(0) as u64,
                        Some(Value::Str(s)) => s.parse().unwrap_or(0),
                        _ => 0,
                    };
                    (n, true)
                }
            },
        };
        Ok(Page {
            rows,
            total,
            exact,
            truncated: total > offset + limit,
            elapsed_ms: t0.elapsed().as_millis() as u32,
            columns: None,
            documents: None,
        })
    }

    async fn execute(&self, sql: &str, limit: u64, cancel: &CancelToken) -> Result<QueryPage, DbError> {
        let query_id = uuid::Uuid::new_v4().to_string();
        let work = async {
            let t0 = Instant::now();
            let max = usize::try_from(limit).unwrap_or(usize::MAX);
            let (set, written) = self.query(sql, &[], Some(&query_id), max).await?;
            let columns = set
                .names
                .iter()
                .enumerate()
                .map(|(i, n)| column(n.clone(), set.types.get(i).cloned().unwrap_or_default()))
                .collect();
            Ok(QueryPage {
                columns,
                rows: set.rows,
                rows_affected: written,
                truncated: set.truncated,
                elapsed_ms: t0.elapsed().as_millis() as u32,
            })
        };
        let kill = async {
            let p = [("qid".to_string(), query_id.clone())];
            let _ = self.post("KILL QUERY WHERE query_id = {qid:String} ASYNC", &p, None, None).await;
        };
        cancellable(work, cancel, Some(kill)).await
    }

    async fn insert_row(&self, table: TableRef, values: Vec<CellChange>) -> Result<u64, DbError> {
        if values.is_empty() {
            return Ok(0);
        }
        let mut row = serde_json::Map::new();
        for v in &values {
            let j = match &v.value {
                Value::Null => serde_json::Value::Null,
                Value::Int(i) => (*i).into(),
                Value::Float(f) => serde_json::Number::from_f64(*f)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
                Value::Bool(b) => (*b).into(),
                Value::Str(s) if v.kind == ColumnKind::Json => {
                    serde_json::from_str(s).unwrap_or_else(|_| s.clone().into())
                }
                Value::Str(s) => s.clone().into(),
                Value::Bytes(_) => return Err(DbError::new(format!("{}: binary values are read-only", v.column))),
            };
            row.insert(v.column.clone(), j);
        }
        let cols = values.iter().map(|v| ident(&v.column)).collect::<Vec<_>>().join(", ");
        let sql = format!("INSERT INTO {} ({cols}) FORMAT JSONEachRow", table_ident(&table));
        let data = serde_json::Value::Object(row).to_string();
        self.post(&sql, &[], None, Some(data)).await?;
        Ok(1)
    }

    async fn drop_table(&self, table: TableRef) -> Result<(), DbError> {
        let Some(keyword) = table.kind.drop_keyword() else {
            return Err(DbError::new(format!("{} cannot be dropped from here", table.name)));
        };
        self.post(&format!("DROP {keyword} {}", table_ident(&table)), &[], None, None).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::Filter;

    #[test]
    fn urls_and_form_fields_resolve_to_the_http_endpoint() {
        let e = parse_url("clickhouse://bob:p%40ss@db.local/analytics").unwrap();
        assert_eq!(e.base, "http://db.local:8123/");
        assert_eq!((e.user.as_str(), e.password.as_str(), e.database.as_str()), ("bob", "p@ss", "analytics"));
        let e = parse_url("clickhouse://h:8443?secure=true").unwrap();
        assert_eq!((e.base.as_str(), e.user.as_str(), e.database.as_str()), ("https://h:8443/", "default", "default"));
        assert_eq!(parse_url("https://x.clickhouse.cloud").unwrap().base, "https://x.clickhouse.cloud:443/");
        assert_eq!(parse_url("clickhouses://x.clickhouse.cloud").unwrap().base, "https://x.clickhouse.cloud:8443/");
        assert_eq!(parse_url("http://[::1]:8123/db").unwrap().base, "http://[::1]:8123/");
        assert!(parse_url("clickhouse://h:9000/db").unwrap_err().message().contains("8123"));
        assert!(parse_url("postgres://h/db").is_err());
    }

    #[test]
    fn types_map_to_kinds_and_values() {
        assert_eq!(kind_of("Nullable(UInt64)"), ColumnKind::Int);
        assert_eq!(kind_of("LowCardinality(Nullable(String))"), ColumnKind::Text);
        assert_eq!(kind_of("DateTime64(3, 'UTC')"), ColumnKind::Date);
        assert_eq!(kind_of("Decimal(10, 2)"), ColumnKind::Float);
        assert_eq!(kind_of("Array(String)"), ColumnKind::Json);
        assert_eq!(kind_of("IntervalDay"), ColumnKind::Text);
        assert!(is_nullable("LowCardinality(Nullable(String))"));
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(cell(&j("\"42\""), "Int64"), Value::Int(42));
        assert_eq!(cell(&j("\"18446744073709551615\""), "UInt64"), Value::Str("18446744073709551615".into()));
        assert_eq!(cell(&j("1.5"), "Float32"), Value::Float(1.5));
        assert_eq!(cell(&j("\"inf\""), "Float64"), Value::Str("inf".into()));
        assert_eq!(cell(&j("null"), "Nullable(String)"), Value::Null);
        assert_eq!(cell(&j("[1,2]"), "Array(UInt8)"), Value::Str("[1,2]".into()));
        assert_eq!(cell(&j("true"), "Bool"), Value::Bool(true));
    }

    #[test]
    fn decoder_reads_split_chunks_and_stops_at_the_limit() {
        let body = "[\"id\",\"name\"]\n[\"UInt64\",\"String\"]\n[\"1\",\"a\"]\n[\"2\",\"b\"]\n[\"3\",\"c\"]\n";
        let mut d = Decoder::default();
        let mut stopped = false;
        for chunk in body.as_bytes().chunks(5) {
            if d.feed(chunk, 2).unwrap() {
                stopped = true;
                break;
            }
        }
        assert!(stopped);
        let set = d.finish(2).unwrap();
        assert_eq!(set.names, ["id", "name"]);
        assert_eq!(set.rows, vec![vec![Value::Int(1), Value::Str("a".into())], vec![Value::Int(2), Value::Str("b".into())]]);
        assert!(set.truncated);

        // DDL: empty body.
        let set = Decoder::default().finish(10).unwrap();
        assert!(set.names.is_empty() && set.rows.is_empty());

        // A user FORMAT → one text column.
        let mut d = Decoder::default();
        d.feed(b"1,a\n2,b\n", 10).unwrap();
        let set = d.finish(10).unwrap();
        assert_eq!(set.names, ["result"]);
        assert_eq!(set.rows.len(), 2);

        // An exception after the headers.
        let mut d = Decoder::default();
        let err = d
            .feed(b"[\"x\"]\n[\"UInt8\"]\n[1]\nCode: 395. DB::Exception: boom. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) (version 24.3.1)\n", 10)
            .unwrap_err();
        assert_eq!(err.message(), "Code: 395. DB::Exception: boom. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)");
    }

    /// Live server: `CLICKHOUSE_URL=https://explorer@play.clickhouse.com/default
    /// cargo test clickhouse_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn clickhouse_live() {
        let url = std::env::var("CLICKHOUSE_URL").expect("set CLICKHOUSE_URL");
        let conn = DbConnection {
            id: "t".into(),
            name: "t".into(),
            db_type: crate::core::model::DbType::ClickHouse,
            host: String::new(),
            port: 0,
            database: String::new(),
            user: String::new(),
            password: String::new(),
            url: Some(url),
            status: ConnectionStatus::Disconnected,
            latency_ms: 0,
            ssh: None,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let s = ClickHouseSession::new(&conn);
            println!("ping {} ms", s.ping().await.unwrap().latency_ms);
            let schemas = s.list_schemas().await.unwrap();
            let t = schemas
                .iter()
                .flat_map(|s| &s.tables)
                .find(|t| !t.columns.is_empty())
                .expect("a table with columns");
            println!("{} schemas; first table {}.{} ({} cols)", schemas.len(), t.schema, t.name, t.columns.len());
            let page = s.fetch_rows(TableRef::of(t), RowQuery::default(), 0, 5).await.unwrap();
            println!("page: {} rows, total {} exact {}", page.rows.len(), page.total, page.exact);
            assert_eq!(page.rows.first().map(|r| r.len()), Some(t.columns.len()));
            let c = &t.columns[0];
            let q = RowQuery {
                filters: vec![Filter {
                    column: c.name.clone(),
                    kind: c.kind,
                    type_: c.type_.clone(),
                    op: FilterOp::NotNull,
                    value: Value::Null,
                }],
                sort: Some((c.name.clone(), true)),
            };
            let page = s.fetch_rows(TableRef::of(t), q, 0, 3).await.unwrap();
            println!("filtered: {} rows, total {}", page.rows.len(), page.total);
            let r = s
                .execute("SELECT number, toString(number) AS s FROM numbers(100000000)", 10, &CancelToken::new())
                .await
                .unwrap();
            assert_eq!(r.rows.len(), 10);
            assert!(r.truncated);
            println!("numbers: {:?} … truncated", r.rows[0]);
            let p = [("x".to_string(), "it's\ta \\ test".to_string())];
            let rows = s.rows("SELECT {x:String}", &p).await.unwrap();
            assert_eq!(rows[0][0], Value::Str("it's\ta \\ test".into()));
            let err = s.rows("SELECT * FROM no_such_table_xyz", &[]).await.unwrap_err();
            println!("error: {}", err.message());
        });
    }

    #[test]
    fn filters_become_query_parameters() {
        let f = |column: &str, op, value| Filter { column: column.into(), kind: ColumnKind::Text, type_: String::new(), op, value };
        let q = RowQuery {
            filters: vec![
                f("id", FilterOp::Ge, Value::Int(5)),
                f("name", FilterOp::Contains, Value::Str("o'k\t".into())),
                f("note", FilterOp::IsNull, Value::Null),
            ],
            sort: Some(("we`ird".into(), true)),
        };
        let (w, o, p) = clauses(&q);
        assert_eq!(
            w,
            " WHERE `id` >= {f0:Int64} AND positionCaseInsensitiveUTF8(toString(`name`), {f1:String}) > 0 AND `note` IS NULL"
        );
        assert_eq!(o, " ORDER BY `we\\`ird` DESC");
        assert_eq!(p, vec![("f0".into(), "5".into()), ("f1".into(), "o'k\t".into())]);
        assert_eq!(escape_param("o'k\t\\"), "o'k\\t\\\\");
    }
}
