//! MongoDB driver: `DbSession` over our own minimal client — `OP_MSG`
//! over tokio TCP (+ rustls), SCRAM-SHA-256/SHA-1 auth, BSON via the
//! `bson` crate. No official `mongodb` crate: it pulls a DNS resolver,
//! compressors and a full SDAM stack (~200 crates) an explorer does not
//! need.
//!
//! Model mapping: databases → schemas, collections → tables, views →
//! views. Collections have no fixed columns: the tree lists names only
//! (fast — no per-collection round trips), and `fetch_rows` infers the
//! columns from the documents it returns — the union of top-level keys
//! (`_id` first, most common BSON type as the column type), widened as
//! later pages bring new keys, cached per collection and sent back in
//! `Page::columns`. Rows are documents projected onto those columns;
//! nested objects/arrays render as relaxed Extended JSON and parse back
//! on edit. The query editor speaks a mongosh subset (`shell.rs`).
//!
//! A small pool ([`POOL`] connections, opened lazily); a connection is
//! dropped on transport errors and reopened on the next call (reads retry
//! once). Seed lists connect to the first reachable host and follow its
//! `primary` hint; `mongodb+srv://` (Atlas) expands its seed list via
//! `dns.rs` once per session. Not supported: x.509 / LDAP / Kerberos
//! auth, compression, sharded-cluster specifics.

mod dns;
mod scram;
mod shell;
mod uri;
mod wire;

use std::collections::HashMap;
use std::time::Instant;

use async_trait::async_trait;
use bson::oid::ObjectId;
use bson::spec::BinarySubtype;
use bson::{Binary, Bson, Document, doc};
use std::sync::Arc;

use crate::core::error::DbError;
use crate::core::model::{
    CellChange, ColumnKind, ColumnMeta, ConnectionStatus, DbConnection, DocNode, NodeKind, Page, Ping,
    QueryPage, Relation, Row, SchemaMeta, TableMeta, TableRef, Value,
};
use crate::core::session::DbSession;

use scram::Mechanism;
use shell::Statement;
use uri::Target;
use wire::{Conn, check_ok};

const MAX_COLUMNS: usize = 200;
/// Databases hidden from the tree (unless named in the URL).
const SYSTEM_DBS: &[&str] = &["admin", "config", "local"];

struct Live {
    conn: Conn,
    /// Connected to a secondary (`directConnection`): reads carry a
    /// `primaryPreferred` read preference, or the server refuses them.
    secondary: bool,
}

/// Connections per session: the grid's count + page and the tree's
/// per-database listings run side by side instead of queueing.
const POOL: usize = 3;

pub struct MongoSession {
    target: Result<Target, DbError>,
    /// `mongodb+srv://` expanded once per session (DNS is not free).
    resolved: tokio::sync::OnceCell<Target>,
    conns: Vec<tokio::sync::Mutex<Option<Live>>>,
    next: std::sync::atomic::AtomicUsize,
    /// `use <db>` target of the query editor.
    current_db: std::sync::Mutex<Option<String>>,
    /// Inferred columns per (database, collection) — rows are projected
    /// onto exactly what the tree shows.
    columns: std::sync::Mutex<HashMap<(String, String), Vec<ColumnMeta>>>,
}

impl MongoSession {
    pub fn new(conn: &DbConnection) -> Self {
        let target = Target::from_connection(conn);
        let current = target.as_ref().ok().and_then(|t| t.database.clone());
        Self {
            target,
            resolved: tokio::sync::OnceCell::new(),
            conns: (0..POOL).map(|_| tokio::sync::Mutex::new(None)).collect(),
            next: std::sync::atomic::AtomicUsize::new(0),
            current_db: std::sync::Mutex::new(current),
            columns: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn target(&self) -> Result<&Target, DbError> {
        self.target.as_ref().map_err(Clone::clone)
    }

    fn current_db(&self) -> String {
        self.current_db
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "test".into())
    }

    // ---------------- connection ----------------

    async fn open(&self) -> Result<Live, DbError> {
        // `mongodb+srv://`: the seed list is looked up once per session
        // (a failed lookup is retried on the next connect).
        let t = self
            .resolved
            .get_or_try_init(|| async { uri::resolve(self.target()?).await })
            .await?;
        let mut last = None;
        for (host, port) in &t.hosts {
            let (mut conn, hello) = match connect_host(t, host, *port).await {
                Ok(x) => x,
                Err(e) => {
                    last = Some(e);
                    continue;
                }
            };
            let mut primary = is_primary(&hello);
            let mut hello = hello;
            // A seed list may start at a secondary: hop to the primary it
            // names (one hop; SDAM-style monitoring is out of scope).
            if !primary
                && !t.direct
                && t.hosts.len() > 1
                && let Ok(p) = hello.get_str("primary")
                && let Some((h, port)) = p.rsplit_once(':').and_then(|(h, p)| Some((h, p.parse().ok()?)))
            {
                let (c, h2) = connect_host(t, h, port).await?;
                conn = c;
                hello = h2;
                primary = is_primary(&hello);
            }
            authenticate(&mut conn, t, &hello).await?;
            return Ok(Live {
                conn,
                secondary: !primary && hello.get_bool("secondary").unwrap_or(false),
            });
        }
        Err(last.unwrap_or_else(|| DbError::new("no MongoDB host to connect to")))
    }

    /// Run one command; server errors come back as `DbError` with the
    /// server's message. Transport failures drop the connection; reads
    /// retry once on a fresh one (an idle connection the server closed).
    /// A pool slot: an idle open connection if there is one, else an idle
    /// empty slot (opened by the caller), else wait for a busy one.
    async fn slot(&self) -> tokio::sync::MutexGuard<'_, Option<Live>> {
        let mut empty = None;
        for s in &self.conns {
            if let Ok(g) = s.try_lock() {
                if g.is_some() {
                    return g;
                }
                if empty.is_none() {
                    empty = Some(g);
                }
            }
        }
        if let Some(g) = empty {
            return g;
        }
        let i = self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % self.conns.len();
        self.conns[i].lock().await
    }

    async fn run(&self, db: &str, cmd: Document) -> Result<Document, DbError> {
        let read = is_read(&cmd);
        let mut guard = self.slot().await;
        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(self.open().await?);
            }
            let live = guard.as_mut().unwrap();
            let mut cmd = cmd.clone();
            if live.secondary && read {
                cmd.insert("$readPreference", doc! { "mode": "primaryPreferred" });
            }
            match live.conn.command(db, cmd).await {
                Ok(reply) => return check_ok(reply),
                Err(e) => {
                    *guard = None;
                    if attempt == 1 || !read {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!()
    }

    /// Documents of a cursor-returning command (`find`, `aggregate`,
    /// `listCollections`, …), following `getMore` up to `limit`. Returns
    /// the documents and whether more were left.
    async fn cursor_docs(
        &self,
        db: &str,
        cmd: Document,
        limit: usize,
    ) -> Result<(Vec<Document>, bool), DbError> {
        let reply = self.run(db, cmd).await?;
        self.drain(db, reply, limit).await
    }

    async fn drain(
        &self,
        db: &str,
        reply: Document,
        limit: usize,
    ) -> Result<(Vec<Document>, bool), DbError> {
        let cursor = reply
            .get_document("cursor")
            .map_err(|_| DbError::new("MongoDB reply has no cursor"))?;
        let ns = cursor.get_str("ns").unwrap_or_default().to_string();
        let coll = ns.split_once('.').map(|(_, c)| c.to_string()).unwrap_or_default();
        let mut id = cursor_id(cursor);
        let mut docs = docs_of(cursor.get_array("firstBatch").ok());
        while id != 0 && docs.len() <= limit {
            let more = self
                .run(
                    db,
                    doc! {
                        "getMore": id,
                        "collection": coll.as_str(),
                        "batchSize": (limit + 1 - docs.len()).min(i32::MAX as usize) as i32,
                    },
                )
                .await?;
            let cursor = more
                .get_document("cursor")
                .map_err(|_| DbError::new("MongoDB reply has no cursor"))?;
            id = cursor_id(cursor);
            docs.extend(docs_of(cursor.get_array("nextBatch").ok()));
        }
        if id != 0 {
            let _ = self
                .run(db, doc! { "killCursors": coll.as_str(), "cursors": [id] })
                .await;
        }
        let more = docs.len() > limit || id != 0;
        docs.truncate(limit);
        Ok((docs, more))
    }

    /// The column metadata for a field when editing (type label decides
    /// how text converts back to BSON).
    fn column(&self, table: &TableRef, name: &str) -> Option<ColumnMeta> {
        self.columns
            .lock()
            .unwrap()
            .get(&(table.schema.clone(), table.name.clone()))?
            .iter()
            .find(|c| c.name == name)
            .cloned()
    }

    fn cell(&self, table: &TableRef, ch: &CellChange) -> Result<Bson, DbError> {
        let label = self.column(table, &ch.column).map(|c| c.type_).unwrap_or_default();
        cell_to_bson(&ch.value, ch.kind, &label)
            .map_err(|e| DbError::new(format!("{}: {e}", ch.column)))
    }

    /// `{ pk: value, … }`; a 24-hex `_id` matches both an ObjectId and
    /// the plain string (the grid shows both the same way).
    fn pk_filter(&self, table: &TableRef, pk: &[CellChange]) -> Result<Document, DbError> {
        if pk.is_empty() {
            return Err(DbError::new("the document has no _id"));
        }
        let mut q = Document::new();
        for key in pk {
            let v = match (&key.value, key.column.as_str()) {
                (Value::Str(s), "_id") => match ObjectId::parse_str(s) {
                    Ok(oid) => Bson::Document(doc! { "$in": [Bson::ObjectId(oid), s.as_str()] }),
                    Err(_) => self.cell(table, key)?,
                },
                _ => self.cell(table, key)?,
            };
            q.insert(key.column.clone(), v);
        }
        Ok(q)
    }

    // ---------------- query editor ----------------

    async fn statement(&self, stmt: Statement, limit: usize) -> Result<Outcome, DbError> {
        let default_db = self.current_db();
        match stmt {
            Statement::Use(db) => {
                *self.current_db.lock().unwrap() = Some(db);
                Ok(Outcome::Affected(0))
            }
            Statement::Command { db, doc } => {
                let db = db.unwrap_or(default_db);
                let reply = self.run(&db, doc).await?;
                if reply.get_document("cursor").is_ok() {
                    let (docs, more) = self.drain(&db, reply, limit).await?;
                    Ok(Outcome::Docs(docs, more))
                } else {
                    Ok(Outcome::Docs(vec![strip_reply(reply)], false))
                }
            }
            Statement::Db { db, method, args } => {
                let db = db.unwrap_or(default_db);
                self.db_method(&db, &method, args, limit).await
            }
            Statement::Collection {
                db,
                coll,
                method,
                args,
                modifiers,
            } => {
                let db = db.unwrap_or(default_db);
                self.collection_method(&db, &coll, &method, args, modifiers, limit)
                    .await
            }
        }
    }

    async fn db_method(
        &self,
        db: &str,
        method: &str,
        args: Vec<Bson>,
        limit: usize,
    ) -> Result<Outcome, DbError> {
        match method {
            "getCollectionNames" | "getCollectionInfos" => {
                let name_only = method == "getCollectionNames";
                let mut cmd = doc! { "listCollections": 1, "nameOnly": name_only };
                if let Some(Bson::Document(f)) = args.first() {
                    cmd.insert("filter", f.clone());
                }
                let (docs, more) = self.cursor_docs(db, cmd, limit).await?;
                let docs = if name_only {
                    docs.into_iter()
                        .map(|d| doc! { "name": d.get("name").cloned().unwrap_or(Bson::Null) })
                        .collect()
                } else {
                    docs
                };
                Ok(Outcome::Docs(docs, more))
            }
            "createCollection" | "createView" => {
                let name = str_arg(&args, 0, method)?;
                let mut cmd = doc! { "create": name };
                if method == "createView" {
                    cmd.insert("viewOn", str_arg(&args, 1, method)?);
                    cmd.insert("pipeline", args.get(2).cloned().unwrap_or(Bson::Array(vec![])));
                } else {
                    cmd.extend(doc_arg(&args, 1)?);
                }
                self.run(db, cmd).await?;
                Ok(Outcome::Affected(0))
            }
            "dropDatabase" => {
                self.run(db, doc! { "dropDatabase": 1 }).await?;
                Ok(Outcome::Affected(0))
            }
            "stats" => Ok(Outcome::Docs(
                vec![strip_reply(self.run(db, doc! { "dbStats": 1 }).await?)],
                false,
            )),
            "serverStatus" | "hostInfo" | "version" | "buildInfo" => {
                let cmd = if method == "version" { "buildInfo" } else { method };
                let reply = strip_reply(self.run("admin", doc! { cmd: 1 }).await?);
                Ok(Outcome::Docs(vec![reply], false))
            }
            other => Err(DbError::new(format!("db.{other}() is not supported"))),
        }
    }

    async fn collection_method(
        &self,
        db: &str,
        coll: &str,
        method: &str,
        args: Vec<Bson>,
        modifiers: Vec<(String, Vec<Bson>)>,
        limit: usize,
    ) -> Result<Outcome, DbError> {
        if !modifiers.is_empty() && !matches!(method, "find" | "aggregate") {
            return Err(DbError::new(format!(
                ".{}() cannot follow {method}()",
                modifiers[0].0
            )));
        }
        match method {
            "find" | "findOne" => {
                let mut cmd = doc! { "find": coll, "filter": doc_arg(&args, 0)? };
                let projection = doc_arg(&args, 1)?;
                if !projection.is_empty() {
                    cmd.insert("projection", projection);
                }
                let mut user_limit = (method == "findOne").then_some(1usize);
                for (m, a) in modifiers {
                    match m.as_str() {
                        "sort" => {
                            cmd.insert("sort", doc_arg(&a, 0)?);
                        }
                        "projection" => {
                            cmd.insert("projection", doc_arg(&a, 0)?);
                        }
                        "skip" => {
                            cmd.insert("skip", int_arg(&a, &m)?);
                        }
                        "limit" => {
                            let n = int_arg(&a, &m)?.unsigned_abs() as usize;
                            user_limit = (n > 0).then_some(n);
                        }
                        "hint" => {
                            cmd.insert("hint", a.first().cloned().unwrap_or(Bson::Null));
                        }
                        "count" | "itcount" | "size" => {
                            let filter = cmd.get_document("filter").cloned().unwrap_or_default();
                            return self.count_docs(db, coll, filter).await;
                        }
                        "toArray" | "pretty" | "batchSize" => {}
                        other => {
                            return Err(DbError::new(format!(".{other}() is not supported")));
                        }
                    }
                }
                // Ask for one extra document to tell "exactly limit" from
                // "truncated".
                let (cap, exact) = match user_limit {
                    Some(n) if n <= limit => (n, true),
                    _ => (limit, false),
                };
                cmd.insert("limit", (cap + usize::from(!exact)) as i64);
                let (docs, more) = self.cursor_docs(db, cmd, cap).await?;
                Ok(Outcome::Docs(docs, more && !exact))
            }
            "aggregate" => {
                let pipeline = match args.first() {
                    Some(Bson::Array(stages)) => stages.clone(),
                    // aggregate(stage1, stage2, …) — legacy shell form.
                    _ => args.iter().filter(|a| matches!(a, Bson::Document(_))).cloned().collect(),
                };
                let mut cmd = doc! {
                    "aggregate": coll,
                    "pipeline": pipeline,
                    "cursor": { "batchSize": (limit + 1) as i32 },
                };
                if let Some(Bson::Document(opts)) = args.get(1) {
                    cmd.extend(opts.clone());
                }
                for (m, _) in &modifiers {
                    if !matches!(m.as_str(), "toArray" | "pretty") {
                        return Err(DbError::new(format!(".{m}() is not supported after aggregate()")));
                    }
                }
                let (docs, more) = self.cursor_docs(db, cmd, limit).await?;
                Ok(Outcome::Docs(docs, more))
            }
            "countDocuments" => self.count_docs(db, coll, doc_arg(&args, 0)?).await,
            "count" | "estimatedDocumentCount" => {
                let mut cmd = doc! { "count": coll };
                let q = doc_arg(&args, 0)?;
                if !q.is_empty() {
                    cmd.insert("query", q);
                }
                let n = number(self.run(db, cmd).await?.get("n"));
                Ok(Outcome::Docs(vec![doc! { "count": n as i64 }], false))
            }
            "distinct" => {
                let reply = self
                    .run(
                        db,
                        doc! { "distinct": coll, "key": str_arg(&args, 0, method)?, "query": doc_arg(&args, 1)? },
                    )
                    .await?;
                let values = reply.get_array("values").cloned().unwrap_or_default();
                let more = values.len() > limit;
                let docs = values.into_iter().take(limit).map(|v| doc! { "value": v }).collect();
                Ok(Outcome::Docs(docs, more))
            }
            "insertOne" | "insert" | "insertMany" => {
                let documents: Vec<Bson> = match args.first() {
                    Some(Bson::Array(a)) => a.clone(),
                    Some(d @ Bson::Document(_)) => vec![d.clone()],
                    _ => return Err(DbError::new(format!("{method} expects a document"))),
                };
                let reply = self.run(db, doc! { "insert": coll, "documents": documents }).await?;
                Ok(Outcome::Affected(write_count(&reply, false)?))
            }
            "updateOne" | "updateMany" | "replaceOne" | "update" => {
                let opts = doc_arg(&args, 2)?;
                let update = match args.get(1) {
                    Some(u @ (Bson::Document(_) | Bson::Array(_))) => u.clone(),
                    _ => return Err(DbError::new(format!("{method} expects an update document"))),
                };
                let multi = method == "updateMany" || opts.get_bool("multi").unwrap_or(false);
                let mut stmt = doc! { "q": doc_arg(&args, 0)?, "u": update, "multi": multi };
                if let Ok(upsert) = opts.get_bool("upsert") {
                    stmt.insert("upsert", upsert);
                }
                let reply = self.run(db, doc! { "update": coll, "updates": [stmt] }).await?;
                Ok(Outcome::Affected(write_count(&reply, true)?))
            }
            "deleteOne" | "deleteMany" | "remove" => {
                let one = method == "deleteOne";
                let reply = self
                    .run(
                        db,
                        doc! { "delete": coll, "deletes": [{ "q": doc_arg(&args, 0)?, "limit": i32::from(one) }] },
                    )
                    .await?;
                Ok(Outcome::Affected(write_count(&reply, false)?))
            }
            "drop" => {
                self.run(db, doc! { "drop": coll }).await?;
                Ok(Outcome::Affected(0))
            }
            "createIndex" => {
                let keys = doc_arg(&args, 0)?;
                let mut index = doc_arg(&args, 1)?;
                if !index.contains_key("name") {
                    let name = keys
                        .iter()
                        .map(|(k, v)| format!("{k}_{}", bson_text(v)))
                        .collect::<Vec<_>>()
                        .join("_");
                    index.insert("name", name);
                }
                index.insert("key", keys);
                self.run(db, doc! { "createIndexes": coll, "indexes": [index] }).await?;
                Ok(Outcome::Affected(0))
            }
            "dropIndex" => {
                let index = args
                    .first()
                    .cloned()
                    .ok_or_else(|| DbError::new("dropIndex expects an index name or key"))?;
                self.run(db, doc! { "dropIndexes": coll, "index": index }).await?;
                Ok(Outcome::Affected(0))
            }
            "getIndexes" => {
                let (docs, more) = self.cursor_docs(db, doc! { "listIndexes": coll }, limit).await?;
                Ok(Outcome::Docs(docs, more))
            }
            "renameCollection" => {
                let to = str_arg(&args, 0, method)?;
                self.run(
                    "admin",
                    doc! { "renameCollection": format!("{db}.{coll}"), "to": format!("{db}.{to}") },
                )
                .await?;
                Ok(Outcome::Affected(0))
            }
            "stats" => Ok(Outcome::Docs(
                vec![strip_reply(self.run(db, doc! { "collStats": coll }).await?)],
                false,
            )),
            other => Err(DbError::new(format!("db.{coll}.{other}() is not supported"))),
        }
    }

    async fn count_docs(&self, db: &str, coll: &str, filter: Document) -> Result<Outcome, DbError> {
        let cmd = doc! {
            "aggregate": coll,
            "pipeline": [{ "$match": filter }, { "$group": { "_id": 1, "n": { "$sum": 1 } } }],
            "cursor": {},
        };
        let (docs, _) = self.cursor_docs(db, cmd, 1).await?;
        let n = docs.first().map(|d| number(d.get("n"))).unwrap_or(0);
        Ok(Outcome::Docs(vec![doc! { "count": n as i64 }], false))
    }
}

enum Outcome {
    Docs(Vec<Document>, bool),
    Affected(u64),
}

async fn connect_host(t: &Target, host: &str, port: u16) -> Result<(Conn, Document), DbError> {
    let mut conn = Conn::connect(host, port, t.tls).await?;
    let mechs = t.user.as_ref().map(|u| format!("{}.{u}", t.auth_db()));
    let mut hello = doc! {
        "hello": 1,
        "helloOk": true,
        "client": {
            "application": { "name": "DataBridge" },
            "driver": { "name": "databridge", "version": env!("CARGO_PKG_VERSION") },
            "os": { "type": std::env::consts::OS },
        },
    };
    if let Some(m) = &mechs {
        hello.insert("saslSupportedMechs", m.as_str());
    }
    let reply = match check_ok(conn.command("admin", hello).await?) {
        Ok(r) => r,
        // Pre-4.4.2 servers: the legacy name of the same handshake.
        Err(_) => {
            let mut legacy = doc! { "isMaster": 1 };
            if let Some(m) = &mechs {
                legacy.insert("saslSupportedMechs", m.as_str());
            }
            check_ok(conn.command("admin", legacy).await?)?
        }
    };
    let wire = number(reply.get("maxWireVersion"));
    if wire < 6 {
        return Err(DbError::new("MongoDB 3.6 or newer is required"));
    }
    Ok((conn, reply))
}

async fn authenticate(conn: &mut Conn, t: &Target, hello: &Document) -> Result<(), DbError> {
    let Some(user) = &t.user else { return Ok(()) };
    let mech = t.mechanism.unwrap_or_else(|| {
        let offered = hello.get_array("saslSupportedMechs").cloned().unwrap_or_default();
        let has = |m: &str| offered.iter().any(|x| x.as_str() == Some(m));
        if !has("SCRAM-SHA-256") && has("SCRAM-SHA-1") {
            Mechanism::Sha1
        } else {
            Mechanism::Sha256
        }
    });
    scram::authenticate(conn, t.auth_db(), user, &t.password, mech).await
}

fn is_primary(hello: &Document) -> bool {
    hello
        .get_bool("isWritablePrimary")
        .or_else(|_| hello.get_bool("ismaster"))
        .unwrap_or(false)
}

/// Commands safe to resend after a transport failure.
fn is_read(cmd: &Document) -> bool {
    let first = cmd.keys().next().map(String::as_str).unwrap_or_default();
    match first {
        "aggregate" => !cmd
            .get_array("pipeline")
            .map(|p| {
                p.iter().any(|s| {
                    s.as_document()
                        .is_some_and(|d| d.contains_key("$out") || d.contains_key("$merge"))
                })
            })
            .unwrap_or(false),
        "find" | "count" | "distinct" | "listDatabases" | "listCollections" | "listIndexes"
        | "ping" | "dbStats" | "collStats" | "hello" | "isMaster" | "buildInfo"
        | "serverStatus" | "hostInfo" => true,
        _ => false,
    }
}

fn cursor_id(cursor: &Document) -> i64 {
    match cursor.get("id") {
        Some(Bson::Int64(v)) => *v,
        Some(Bson::Int32(v)) => *v as i64,
        _ => 0,
    }
}

fn docs_of(batch: Option<&bson::Array>) -> Vec<Document> {
    batch
        .map(|a| a.iter().filter_map(|d| d.as_document().cloned()).collect())
        .unwrap_or_default()
}

fn number(v: Option<&Bson>) -> i64 {
    match v {
        Some(Bson::Int32(n)) => *n as i64,
        Some(Bson::Int64(n)) => *n,
        Some(Bson::Double(n)) => *n as i64,
        _ => 0,
    }
}

/// Reply noise that is not part of a command's answer.
fn strip_reply(mut d: Document) -> Document {
    for k in ["ok", "$clusterTime", "operationTime", "$db"] {
        d.remove(k);
    }
    d
}

/// Rows touched by an insert/update/delete reply; write errors (which
/// arrive with `ok: 1`) become errors.
fn write_count(reply: &Document, update: bool) -> Result<u64, DbError> {
    if let Ok(errors) = reply.get_array("writeErrors")
        && let Some(first) = errors.first().and_then(Bson::as_document)
    {
        return Err(DbError::new(format!(
            "query failed: {}",
            first.get_str("errmsg").unwrap_or("write error")
        )));
    }
    if let Ok(wc) = reply.get_document("writeConcernError") {
        return Err(DbError::new(format!(
            "write concern failed: {}",
            wc.get_str("errmsg").unwrap_or_default()
        )));
    }
    // `n` counts matched (not modified) documents for updates — an edit
    // that keeps the value still found its document — plus upserts.
    let _ = update;
    Ok(number(reply.get("n")).max(0) as u64)
}

fn doc_arg(args: &[Bson], i: usize) -> Result<Document, DbError> {
    match args.get(i) {
        None | Some(Bson::Null) => Ok(Document::new()),
        Some(Bson::Document(d)) => Ok(d.clone()),
        Some(other) => Err(DbError::new(format!(
            "argument {} must be a document, got {}",
            i + 1,
            bson_text(other)
        ))),
    }
}

fn str_arg(args: &[Bson], i: usize, method: &str) -> Result<String, DbError> {
    match args.get(i) {
        Some(Bson::String(s)) => Ok(s.clone()),
        _ => Err(DbError::new(format!("{method} expects a name string"))),
    }
}

fn int_arg(args: &[Bson], method: &str) -> Result<i64, DbError> {
    match args.first() {
        Some(Bson::Int32(v)) => Ok(*v as i64),
        Some(Bson::Int64(v)) => Ok(*v),
        Some(Bson::Double(v)) if v.fract() == 0.0 => Ok(*v as i64),
        _ => Err(DbError::new(format!(".{method}() expects a number"))),
    }
}

// ---------------- BSON ↔ grid ----------------

fn type_label(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid => "uuid",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null | Bson::Undefined => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::Int32(_) => "int",
        Bson::Timestamp(_) => "timestamp",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        _ => "other",
    }
}

fn label_kind(label: &str) -> ColumnKind {
    match label {
        "double" | "decimal" => ColumnKind::Float,
        "int" | "long" => ColumnKind::Int,
        "bool" => ColumnKind::Bool,
        "date" => ColumnKind::Date,
        "object" | "array" => ColumnKind::Json,
        "uuid" => ColumnKind::Uuid,
        "binData" => ColumnKind::Binary,
        _ => ColumnKind::Text,
    }
}

/// Union of the documents' top-level keys (`_id` first, then first-seen
/// order); the most common non-null type names the column.
fn infer_columns(docs: &[Document]) -> Vec<ColumnMeta> {
    struct Acc {
        present: usize,
        null: bool,
        types: Vec<(&'static str, usize)>,
    }
    let mut order: Vec<String> = vec!["_id".into()];
    let mut acc: HashMap<String, Acc> = HashMap::new();
    for d in docs {
        for (k, v) in d {
            let a = acc.entry(k.clone()).or_insert_with(|| {
                if k != "_id" && order.len() < MAX_COLUMNS {
                    order.push(k.clone());
                }
                Acc { present: 0, null: false, types: Vec::new() }
            });
            a.present += 1;
            let t = type_label(v);
            if t == "null" {
                a.null = true;
                continue;
            }
            match a.types.iter_mut().find(|(n, _)| *n == t) {
                Some((_, c)) => *c += 1,
                None => a.types.push((t, 1)),
            }
        }
    }
    order
        .into_iter()
        .filter_map(|name| {
            let is_id = name == "_id";
            let a = acc.get(&name);
            if a.is_none() && !(is_id && docs.is_empty()) {
                return None; // documents without `_id` (projections)
            }
            let label = a
                .and_then(|a| a.types.iter().max_by_key(|(_, c)| *c).map(|(t, _)| *t))
                .unwrap_or(if is_id { "objectId" } else { "null" });
            let generated = is_id && label == "objectId";
            Some(ColumnMeta {
                nullable: !is_id && a.is_none_or(|a| a.null || a.present < docs.len()),
                primary_key: is_id,
                auto_increment: generated,
                has_default: generated,
                kind: label_kind(label),
                type_: label.to_string(),
                name,
            })
        })
        .collect()
}

/// Text for labels: strings bare, everything else as relaxed JSON.
fn bson_text(v: &Bson) -> String {
    match v {
        Bson::String(s) => s.clone(),
        other => other.clone().into_relaxed_extjson().to_string(),
    }
}

fn to_value(v: &Bson) -> Value {
    match v {
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Int32(n) => Value::Int(*n as i64),
        Bson::Int64(n) => Value::Int(*n),
        Bson::Double(n) => Value::Float(*n),
        Bson::Boolean(b) => Value::Bool(*b),
        Bson::String(s) => Value::Str(s.clone()),
        Bson::ObjectId(oid) => Value::Str(oid.to_hex()),
        Bson::DateTime(dt) => Value::Str(
            dt.try_to_rfc3339_string()
                .unwrap_or_else(|_| dt.timestamp_millis().to_string()),
        ),
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid => match uuid::Uuid::from_slice(&b.bytes) {
            Ok(u) => Value::Str(u.to_string()),
            Err(_) => Value::Bytes(b.bytes.clone()),
        },
        Bson::Binary(b) => Value::Bytes(b.bytes.clone()),
        Bson::Decimal128(_) => {
            let json = v.clone().into_relaxed_extjson();
            match json.get("$numberDecimal").and_then(|s| s.as_str()) {
                Some(s) => Value::Str(s.to_string()),
                None => Value::Str(json.to_string()),
            }
        }
        other => Value::Str(other.clone().into_relaxed_extjson().to_string()),
    }
}

/// BSON → the UI's document tree; leaves as mongosh literals that
/// `shell::parse_value` reads back to the same BSON (int vs long, double,
/// ObjectId, dates, decimals, UUIDs survive the editor).
fn to_node(v: &Bson) -> DocNode {
    let leaf = |text: String, kind: ColumnKind| DocNode::Scalar { text, kind: Some(kind) };
    let ext = |v: &Bson| v.clone().into_relaxed_extjson().to_string();
    match v {
        Bson::Document(d) => DocNode::Object(d.iter().map(|(k, v)| (k.clone(), to_node(v))).collect()),
        Bson::Array(a) => DocNode::Array(a.iter().map(to_node).collect()),
        Bson::Null | Bson::Undefined => DocNode::Scalar { text: "null".into(), kind: None },
        Bson::String(s) => leaf(serde_json::to_string(s).unwrap_or_default(), ColumnKind::Text),
        Bson::Boolean(b) => leaf(b.to_string(), ColumnKind::Bool),
        Bson::Int32(n) => leaf(n.to_string(), ColumnKind::Int),
        // Small longs need the wrapper, or they would come back as int.
        Bson::Int64(n) if i32::try_from(*n).is_ok() => leaf(format!("NumberLong({n})"), ColumnKind::Int),
        Bson::Int64(n) => leaf(n.to_string(), ColumnKind::Int),
        Bson::Double(f) if f.is_finite() => {
            // Keep a decimal point so it parses back as a double.
            let s = format!("{f}");
            let s = if s.contains(['.', 'e', 'E']) { s } else { format!("{s}.0") };
            leaf(s, ColumnKind::Float)
        }
        Bson::Double(_) => leaf(ext(v), ColumnKind::Float),
        Bson::ObjectId(oid) => leaf(format!("ObjectId(\"{}\")", oid.to_hex()), ColumnKind::Uuid),
        Bson::DateTime(dt) => match dt.try_to_rfc3339_string() {
            Ok(s) => leaf(format!("ISODate(\"{s}\")"), ColumnKind::Date),
            Err(_) => leaf(ext(v), ColumnKind::Date),
        },
        Bson::Decimal128(_) => match to_value(v) {
            Value::Str(s) => leaf(format!("NumberDecimal(\"{s}\")"), ColumnKind::Float),
            _ => leaf(ext(v), ColumnKind::Float),
        },
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid => match uuid::Uuid::from_slice(&b.bytes) {
            Ok(u) => leaf(format!("UUID(\"{u}\")"), ColumnKind::Uuid),
            Err(_) => leaf(ext(v), ColumnKind::Binary),
        },
        Bson::Binary(_) => leaf(ext(v), ColumnKind::Binary),
        Bson::RegularExpression(r) => {
            // Escape bare slashes: the literal ends at the first one.
            let mut pattern = String::new();
            let mut escaped = false;
            for c in r.pattern.chars() {
                if c == '/' && !escaped {
                    pattern.push('\\');
                }
                escaped = c == '\\' && !escaped;
                pattern.push(c);
            }
            leaf(format!("/{pattern}/{}", r.options), ColumnKind::Text)
        }
        other => leaf(ext(other), ColumnKind::Json),
    }
}

/// Add the columns of `fresh` that `known` lacks (an empty `known` takes
/// `fresh` as is). Existing columns keep their position and type.
fn merge_columns(known: &mut Vec<ColumnMeta>, fresh: Vec<ColumnMeta>) {
    if known.is_empty() {
        *known = fresh;
        return;
    }
    for c in fresh {
        if known.len() >= MAX_COLUMNS {
            break;
        }
        if !known.iter().any(|k| k.name == c.name) {
            // Seen only on a later page: absent from earlier documents.
            known.push(ColumnMeta { nullable: true, ..c });
        }
    }
}

/// Await all futures concurrently, results in input order (the tiny
/// `futures::join_all` this crate otherwise lacks).
async fn join_all<F: std::future::Future>(futs: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    use std::task::Poll;
    let mut futs: Vec<std::pin::Pin<Box<F>>> = futs.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = futs.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (f, slot) in futs.iter_mut().zip(out.iter_mut()) {
            if slot.is_none() {
                match f.as_mut().poll(cx) {
                    Poll::Ready(v) => *slot = Some(v),
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending { Poll::Pending } else { Poll::Ready(()) }
    })
    .await;
    out.into_iter().map(|v| v.expect("every future completed")).collect()
}

fn project(doc: &Document, columns: &[ColumnMeta]) -> Row {
    columns
        .iter()
        .map(|c| doc.get(&c.name).map(to_value).unwrap_or(Value::Null))
        .collect()
}

/// Editor text/typed value → BSON, guided by the column's type label so
/// an ObjectId stays an ObjectId and a `long` stays a `long`.
fn cell_to_bson(v: &Value, kind: ColumnKind, label: &str) -> Result<Bson, String> {
    Ok(match v {
        Value::Null => Bson::Null,
        Value::Bool(b) => Bson::Boolean(*b),
        Value::Int(n) => match label {
            "long" => Bson::Int64(*n),
            "double" => Bson::Double(*n as f64),
            "decimal" => decimal(&n.to_string())?,
            _ => i32::try_from(*n).map(Bson::Int32).unwrap_or(Bson::Int64(*n)),
        },
        Value::Float(f) => match label {
            "decimal" => decimal(&f.to_string())?,
            _ => Bson::Double(*f),
        },
        Value::Bytes(b) => Bson::Binary(Binary {
            subtype: BinarySubtype::Generic,
            bytes: b.clone(),
        }),
        Value::Str(s) => match (label, kind) {
            ("objectId", _) => Bson::ObjectId(
                ObjectId::parse_str(s.trim()).map_err(|_| format!("invalid ObjectId \"{s}\""))?,
            ),
            ("decimal", _) => decimal(s.trim())?,
            (_, ColumnKind::Date) => Bson::DateTime(shell::parse_date(s)?),
            (_, ColumnKind::Json) => shell::parse_value(s)?,
            (_, ColumnKind::Uuid) => Bson::Binary(Binary {
                subtype: BinarySubtype::Uuid,
                bytes: uuid::Uuid::parse_str(s.trim())
                    .map_err(|_| format!("invalid UUID \"{s}\""))?
                    .as_bytes()
                    .to_vec(),
            }),
            _ => Bson::String(s.clone()),
        },
    })
}

fn decimal(s: &str) -> Result<Bson, String> {
    Bson::try_from(serde_json::json!({ "$numberDecimal": s })).map_err(|_| format!("invalid decimal \"{s}\""))
}

fn query_page(outcome: Outcome, t0: Instant) -> QueryPage {
    let elapsed_ms = t0.elapsed().as_millis() as u32;
    match outcome {
        Outcome::Affected(n) => QueryPage {
            columns: Vec::new(),
            rows: Vec::new(),
            rows_affected: n,
            truncated: false,
            elapsed_ms,
        },
        Outcome::Docs(docs, truncated) => {
            let mut columns = infer_columns(&docs);
            if docs.is_empty() {
                columns.clear();
            }
            let rows = docs.iter().map(|d| project(d, &columns)).collect();
            QueryPage {
                columns,
                rows,
                rows_affected: 0,
                truncated,
                elapsed_ms,
            }
        }
    }
}

#[async_trait]
impl DbSession for MongoSession {
    async fn ping(&self) -> Result<Ping, DbError> {
        let t0 = Instant::now();
        self.run("admin", doc! { "ping": 1 }).await?;
        Ok(Ping {
            status: ConnectionStatus::Connected,
            latency_ms: t0.elapsed().as_millis() as u32,
        })
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaMeta>, DbError> {
        let t = self.target()?;
        let listed = self
            .run(
                "admin",
                doc! { "listDatabases": 1, "nameOnly": true, "authorizedDatabases": true },
            )
            .await;
        let mut names: Vec<String> = match listed {
            Ok(reply) => reply
                .get_array("databases")
                .map(|a| {
                    a.iter()
                        .filter_map(|d| d.as_document()?.get_str("name").ok().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
            // Users scoped to one database may not list the others.
            Err(e) => match &t.database {
                Some(db) => vec![db.clone()],
                None => return Err(e),
            },
        };
        names.retain(|n| !SYSTEM_DBS.contains(&n.as_str()) || t.database.as_deref() == Some(n));
        // A URL database that has no collections yet does not exist on the
        // server; show it anyway so it is reachable.
        if let Some(db) = &t.database
            && !names.contains(db)
        {
            names.push(db.clone());
        }
        names.sort();
        {
            let mut cur = self.current_db.lock().unwrap();
            if cur.is_none() {
                *cur = names.first().cloned();
            }
        }

        // Names only, every database at once (pool slots): no per-collection
        // sampling here — columns are inferred when a collection is opened
        // (`fetch_rows`) and remembered for later tree reloads.
        let listings = join_all(names.iter().map(|db| {
            self.cursor_docs(
                db,
                doc! { "listCollections": 1, "authorizedCollections": true, "nameOnly": true },
                100_000,
            )
        }))
        .await;
        let mut out = Vec::with_capacity(names.len());
        for (db, listed) in names.into_iter().zip(listings) {
            let colls = listed.map(|(docs, _)| docs).unwrap_or_default();
            let mut colls: Vec<(String, bool)> = colls
                .iter()
                .filter_map(|c| {
                    let name = c.get_str("name").ok()?;
                    (!name.starts_with("system.")).then(|| {
                        (name.to_string(), c.get_str("type").unwrap_or("collection") == "view")
                    })
                })
                .collect();
            colls.sort();
            let mut tables = Vec::new();
            let mut views = Vec::new();
            for (name, is_view) in colls {
                let columns = self
                    .columns
                    .lock()
                    .unwrap()
                    .get(&(db.clone(), name.clone()))
                    .cloned()
                    .unwrap_or_default();
                let meta = Arc::new(TableMeta {
                    name,
                    schema: db.clone(),
                    kind: if is_view { NodeKind::View } else { NodeKind::Table },
                    columns,
                    row_count: 0,
                });
                if is_view {
                    views.push(meta);
                } else {
                    tables.push(meta);
                }
            }
            out.push(SchemaMeta {
                name: db,
                tables,
                views,
                functions: Vec::new(),
            });
        }
        Ok(out)
    }

    /// Collections declare no references.
    async fn list_relations(&self, _schema: &str) -> Result<Vec<Relation>, DbError> {
        Ok(Vec::new())
    }

    async fn fetch_rows(&self, table: TableRef, offset: u64, limit: u64) -> Result<Page, DbError> {
        let t0 = Instant::now();
        let db = table.schema.as_str();
        // Count (collection metadata, no scan without a filter) and the page
        // side by side on two pool connections.
        let count = self.run(db, doc! { "count": table.name.as_str() });
        let cmd = doc! {
            "find": table.name.as_str(),
            "skip": offset as i64,
            "limit": limit as i64,
            "batchSize": limit.min(i32::MAX as u64) as i32,
        };
        let page = self.cursor_docs(db, cmd, limit as usize);
        let (count, page) = tokio::join!(count, page);
        let (docs, _) = page
            .map_err(|e| DbError::new(format!("failed to read {}: {}", table.name, e.message())))?;
        let total = number(
            count
                .map_err(|e| DbError::new(format!("failed to count {}: {}", table.name, e.message())))?
                .get("n"),
        )
        .max(0) as u64;
        // Columns: the union so far, widened by keys this page brings in.
        let key = (table.schema.clone(), table.name.clone());
        let columns = {
            let mut cache = self.columns.lock().unwrap();
            let known = cache.entry(key).or_default();
            merge_columns(known, infer_columns(&docs));
            known.clone()
        };
        Ok(Page {
            rows: docs.iter().map(|d| project(d, &columns)).collect(),
            total,
            exact: true,
            truncated: total > offset + limit,
            elapsed_ms: t0.elapsed().as_millis() as u32,
            // Always sent: the tree's TableMeta of a collection carries no
            // columns until its first page.
            columns: Some(columns),
            documents: Some(docs.iter().map(|d| to_node(&Bson::Document(d.clone()))).collect()),
        })
    }

    async fn execute(&self, sql: &str, limit: u64) -> Result<QueryPage, DbError> {
        let t0 = Instant::now();
        let stmt = shell::parse(sql).map_err(|e| DbError::new(format!("query failed: {e}")))?;
        let outcome = self
            .statement(stmt, limit as usize)
            .await
            .map_err(|e| {
                if e.message().starts_with("query failed") {
                    e
                } else {
                    DbError::new(format!("query failed: {}", e.message()))
                }
            })?;
        Ok(query_page(outcome, t0))
    }

    async fn update_row(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
        changes: Vec<CellChange>,
    ) -> Result<u64, DbError> {
        if changes.is_empty() {
            return Ok(0);
        }
        let q = self.pk_filter(&table, &pk)?;
        let mut set = Document::new();
        for ch in &changes {
            set.insert(ch.column.clone(), self.cell(&table, ch)?);
        }
        let reply = self
            .run(
                &table.schema,
                doc! { "update": table.name.as_str(), "updates": [{ "q": q, "u": { "$set": set }, "multi": false }] },
            )
            .await
            .map_err(|e| DbError::new(format!("failed to update the document: {}", e.message())))?;
        write_count(&reply, true)
    }

    async fn insert_row(&self, table: TableRef, values: Vec<CellChange>) -> Result<u64, DbError> {
        let mut d = Document::new();
        for v in &values {
            d.insert(v.column.clone(), self.cell(&table, v)?);
        }
        let reply = self
            .run(&table.schema, doc! { "insert": table.name.as_str(), "documents": [d] })
            .await
            .map_err(|e| DbError::new(format!("failed to insert the document: {}", e.message())))?;
        write_count(&reply, false)
    }

    async fn delete_row(&self, table: TableRef, pk: Vec<CellChange>) -> Result<u64, DbError> {
        let q = self.pk_filter(&table, &pk)?;
        let reply = self
            .run(
                &table.schema,
                doc! { "delete": table.name.as_str(), "deletes": [{ "q": q, "limit": 1 }] },
            )
            .await
            .map_err(|e| DbError::new(format!("failed to delete the document: {}", e.message())))?;
        write_count(&reply, false)
    }

    async fn save_document(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
        text: String,
    ) -> Result<u64, DbError> {
        let doc = match shell::parse_value(&text) {
            Ok(Bson::Document(d)) => d,
            Ok(_) => return Err(DbError::new("the document must be an object { … }")),
            Err(e) => return Err(DbError::new(format!("invalid document: {e}"))),
        };
        let fail = |what: &str, e: DbError| DbError::new(format!("failed to {what} the document: {}", e.message()));
        if pk.is_empty() {
            let reply = self
                .run(&table.schema, doc! { "insert": table.name.as_str(), "documents": [doc] })
                .await
                .map_err(|e| fail("insert", e))?;
            return write_count(&reply, false);
        }
        let q = self.pk_filter(&table, &pk)?;
        // A replacement document must not change `_id`; a missing one
        // keeps the stored id.
        let reply = self
            .run(
                &table.schema,
                doc! { "update": table.name.as_str(), "updates": [{ "q": q, "u": doc, "multi": false }] },
            )
            .await
            .map_err(|e| fail("replace", e))?;
        let n = write_count(&reply, true)?;
        if n == 0 {
            return Err(DbError::new("the document no longer exists (deleted meanwhile?)"));
        }
        Ok(n)
    }

    async fn drop_table(&self, table: TableRef) -> Result<(), DbError> {
        if table.kind.drop_keyword().is_none() {
            return Err(DbError::new(format!("{} cannot be dropped from here", table.name)));
        }
        self.run(&table.schema, doc! { "drop": table.name.as_str() })
            .await
            .map_err(|e| DbError::new(format!("failed to drop {}: {}", table.name, e.message())))?;
        self.columns
            .lock()
            .unwrap()
            .remove(&(table.schema.clone(), table.name.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_union_with_id_first_and_majority_type() {
        let oid = ObjectId::new();
        let docs = vec![
            doc! { "name": "a", "_id": oid, "n": 1 },
            doc! { "_id": ObjectId::new(), "n": 2, "tags": ["x"] },
            doc! { "_id": ObjectId::new(), "n": 3.5, "name": Bson::Null },
            doc! { "_id": ObjectId::new(), "n": 4 },
        ];
        let cols = infer_columns(&docs);
        let names: Vec<_> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["_id", "name", "n", "tags"]);
        assert!(cols[0].primary_key && cols[0].auto_increment && !cols[0].nullable);
        assert_eq!((cols[2].type_.as_str(), cols[2].kind), ("int", ColumnKind::Int));
        assert!(cols[1].nullable);
        assert_eq!(cols[3].kind, ColumnKind::Json);
        let row = project(&docs[1], &cols);
        assert_eq!(row[1], Value::Null);
        assert_eq!(row[3], Value::Str(r#"["x"]"#.into()));
    }

    #[test]
    fn edited_text_converts_back_by_label() {
        let hex = "65a1b2c3d4e5f60718293a4b";
        assert!(matches!(
            cell_to_bson(&Value::Str(hex.into()), ColumnKind::Text, "objectId"),
            Ok(Bson::ObjectId(_))
        ));
        assert_eq!(
            cell_to_bson(&Value::Int(5), ColumnKind::Int, "long").unwrap(),
            Bson::Int64(5)
        );
        assert_eq!(
            cell_to_bson(&Value::Str(r#"{"a": [1, 2]}"#.into()), ColumnKind::Json, "object").unwrap(),
            Bson::Document(doc! { "a": [1, 2] })
        );
        assert!(matches!(
            cell_to_bson(&Value::Str("2024-05-01T10:00:00Z".into()), ColumnKind::Date, "date"),
            Ok(Bson::DateTime(_))
        ));
    }

    #[test]
    fn nested_values_round_trip_through_the_grid() {
        let original = Bson::Document(doc! { "at": bson::DateTime::from_millis(0), "id": ObjectId::new() });
        let Value::Str(text) = to_value(&original) else { panic!() };
        assert_eq!(cell_to_bson(&Value::Str(text), ColumnKind::Json, "object").unwrap(), original);
    }

    /// Live round trip: `MONGO_URL=mongodb://… cargo test -- --ignored mongo_live`.
    #[test]
    #[ignore = "needs a live MongoDB (MONGO_URL)"]
    fn mongo_live() {
        let url = std::env::var("MONGO_URL").unwrap_or("mongodb://localhost:27017/databridge_test".into());
        let conn = DbConnection {
            id: "t".into(),
            name: "t".into(),
            db_type: crate::core::model::DbType::MongoDb,
            host: String::new(),
            port: 0,
            database: String::new(),
            user: String::new(),
            password: String::new(),
            url: Some(url),
            status: ConnectionStatus::Disconnected,
            latency_ms: 0,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let s = MongoSession::new(&conn);
            s.ping().await.unwrap();
            s.execute("db.dbx_live.drop()", 10).await.ok();
            let ins = s.execute("db.dbx_live.insertMany([{ a: 1, b: 'x' }, { a: 2 }])", 10).await.unwrap();
            assert_eq!(ins.rows_affected, 2);
            let page = s.execute("db.dbx_live.find({ a: { $gte: 1 } }).sort({ a: -1 })", 10).await.unwrap();
            assert_eq!(page.rows.len(), 2);
            let schemas = s.list_schemas().await.unwrap();
            let t = schemas
                .iter()
                .flat_map(|sc| sc.tables.iter())
                .find(|t| t.name == "dbx_live")
                .unwrap()
                .clone();
            let rows = s.fetch_rows(TableRef::of(&t), 0, 50).await.unwrap();
            assert_eq!(rows.total, 2);
            let id = rows.rows[0][0].clone();
            let pk = vec![CellChange::new("_id", ColumnKind::Text, id)];
            let upd = vec![CellChange::new("a", ColumnKind::Int, Value::Int(9))];
            assert_eq!(s.update_row(TableRef::of(&t), pk.clone(), upd).await.unwrap(), 1);
            assert_eq!(s.delete_row(TableRef::of(&t), pk).await.unwrap(), 1);
            s.drop_table(TableRef::of(&t)).await.unwrap();
        });
    }

    /// Grid document → editor text → parsed back: every type survives.
    #[test]
    fn documents_round_trip_through_the_editor() {
        let original = doc! {
            "_id": ObjectId::new(),
            "n32": 7,
            "n64": 7i64,
            "big": 3_000_000_000i64,
            "f": 2.0,
            "at": bson::DateTime::from_millis(1_700_000_000_123),
            "s": "quote \" and \\ and\nnewline",
            "nested": { "arr": [1, "x", { "deep": Bson::Null }], "empty": {} },
            // (A stored `\/` would come back as `/`: the same regex, and a
            // regex literal cannot tell them apart.)
            "re": bson::Regex { pattern: "a/b\\d".into(), options: "i".into() },
            "u": Bson::Binary(Binary { subtype: BinarySubtype::Uuid, bytes: uuid::Uuid::new_v4().as_bytes().to_vec() }),
            "dec": decimal("12.50").unwrap(),
        };
        let text = crate::core::model::format_document(&to_node(&Bson::Document(original.clone())));
        assert_eq!(shell::parse_value(&text).unwrap(), Bson::Document(original), "{text}");
    }

    #[test]
    fn columns_widen_with_later_pages() {
        let mut known = Vec::new();
        merge_columns(&mut known, infer_columns(&[doc! { "_id": 1, "a": 1 }]));
        merge_columns(&mut known, infer_columns(&[doc! { "_id": 2, "b": "x", "a": 2 }]));
        let names: Vec<_> = known.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["_id", "a", "b"]);
        assert!(known[2].nullable);
    }

    #[test]
    fn join_all_keeps_order() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let out = rt.block_on(join_all((0..3u64).map(|i| async move {
            tokio::time::sleep(std::time::Duration::from_millis(30 - i * 10)).await;
            i
        })));
        assert_eq!(out, vec![0, 1, 2]);
    }

    #[test]
    fn retry_only_reads() {
        assert!(is_read(&doc! { "find": "a" }));
        assert!(!is_read(&doc! { "insert": "a" }));
        assert!(!is_read(&doc! { "aggregate": "a", "pipeline": [{ "$out": "b" }] }));
    }
}
