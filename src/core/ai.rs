//! AI assistant (Stage 6): a streaming chat client for **OpenAI-compatible**
//! servers only — Ollama, LM Studio, llama.cpp `server`, vLLM, LocalAI, or
//! any hosted endpoint speaking `/v1/chat/completions`. No vendor SDKs.
//!
//! Pure parts (SSE parsing, schema context, SQL block extraction) are unit
//! tested here; network calls run on the tokio sidecar and report back
//! through runtime-agnostic channels, like `SessionHandle`.
//!
//! Persistence: `ai.json` (base URL + model) in the platform config dir;
//! the optional API key lives in the OS keyring only (`storage`).
//! Privacy: only the schema (names and types) is sent — never row data.

use std::fmt::Write as _;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::core::error::DbError;
use crate::core::model::{DbType, QueryPage, SchemaMeta, first_keyword};
use crate::core::{runtime, storage};

const FILE: &str = "ai.json";
const HISTORY_FILE: &str = "ai_history.json";
/// Conversations kept in the history list.
const MAX_HISTORY: usize = 50;
/// Statements the assistant may run in a row in Auto mode before it must
/// hand control back to the user.
pub const MAX_AUTO_STEPS: usize = 6;
/// Result rows shown to the model per executed statement.
const RESULT_ROWS_FOR_MODEL: usize = 20;
const KEY_ID: &str = "ai";
const KEY_SUFFIX: &str = "::api_key";
/// Upper bound for the schema description in the system prompt (chars);
/// small local models have small context windows.
const SCHEMA_BUDGET: usize = 12_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AiConfig {
    /// e.g. `http://localhost:11434/v1` (Ollama) or `http://localhost:1234/v1` (LM Studio).
    pub base_url: String,
    pub model: String,
    #[serde(default)]
    pub mode: AiMode,
    /// Never serialized — keyring only.
    #[serde(skip)]
    pub api_key: String,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:11434/v1".into(),
            model: String::new(),
            mode: AiMode::Manual,
            api_key: String::new(),
        }
    }
}

impl AiConfig {
    /// `<base>/<path>` with exactly one slash; a bare host gets `/v1`.
    fn endpoint(&self, path: &str) -> String {
        let mut base = self.base_url.trim().trim_end_matches('/').to_string();
        let after_scheme = base.split_once("://").map(|(_, r)| r).unwrap_or(&base);
        if !after_scheme.contains('/') {
            base.push_str("/v1");
        }
        format!("{base}/{path}")
    }
}

pub fn load_config() -> AiConfig {
    let mut cfg = storage::config_dir()
        .and_then(|d| std::fs::read_to_string(d.join(FILE)).ok())
        .and_then(|s| serde_json::from_str::<AiConfig>(&s).ok())
        .unwrap_or_default();
    cfg.api_key = storage::load_secret(KEY_ID, KEY_SUFFIX).unwrap_or_default();
    cfg
}

pub fn save_config(cfg: &AiConfig) {
    if let Some(dir) = storage::config_dir() {
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(json) = serde_json::to_string_pretty(cfg) {
            if let Err(e) = std::fs::write(dir.join(FILE), json) {
                eprintln!("DataBridge: cannot write {FILE}: {e}");
            }
        }
    }
    if cfg.api_key.trim().is_empty() {
        storage::delete_secret(KEY_ID, KEY_SUFFIX);
    } else {
        storage::save_secret(KEY_ID, KEY_SUFFIX, cfg.api_key.trim());
    }
}

/// Manual: SQL in answers runs only on the user's Run click.
/// Auto: the assistant runs its statements itself and sees the results
/// (up to [`MAX_AUTO_STEPS`]); destructive ones still wait for a click.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AiMode {
    #[default]
    Manual,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

/// One streamed event. The stream always ends with `Done` or `Error`.
#[derive(Debug, Clone, PartialEq)]
pub enum AiEvent {
    Delta(String),
    Done,
    Error(String),
}

fn client() -> Result<reqwest::Client, DbError> {
    // reqwest is built without a bundled crypto provider; sqlx already
    // links rustls+ring, so reuse it. Ignored when already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| DbError::with_source("cannot create the HTTP client", e))
}

fn authed(req: reqwest::RequestBuilder, cfg: &AiConfig) -> reqwest::RequestBuilder {
    if cfg.api_key.trim().is_empty() {
        req
    } else {
        req.bearer_auth(cfg.api_key.trim())
    }
}

fn http_error(status: reqwest::StatusCode, body: &str) -> String {
    // OpenAI-style `{"error":{"message":…}}` or Ollama's `{"error":"…"}`.
    let msg = serde_json::from_str::<serde_json::Value>(body).ok().and_then(|v| {
        let e = v.get("error")?;
        e.get("message")
            .and_then(|m| m.as_str())
            .or_else(|| e.as_str())
            .map(str::to_string)
    });
    format!("HTTP {status}: {}", msg.unwrap_or_else(|| body.chars().take(300).collect()))
}

/// `GET /models` → model ids (for the picker).
pub fn list_models(cfg: AiConfig) -> oneshot::Receiver<Result<Vec<String>, DbError>> {
    runtime::spawn(async move {
        let client = client()?;
        let resp = authed(client.get(cfg.endpoint("models")), &cfg)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| DbError::with_source(format!("cannot reach {}: {e}", cfg.base_url), e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(DbError::new(http_error(status, &body)));
        }
        Ok(parse_models(&body))
    })
}

fn parse_models(body: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let mut ids: Vec<String> = v
        .get("data")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}

/// Stream a chat completion. Dropping the receiver cancels the request
/// (the sidecar task notices the closed channel on the next chunk).
pub fn stream_chat(cfg: AiConfig, messages: Vec<ChatMessage>) -> mpsc::UnboundedReceiver<AiEvent> {
    let (tx, rx) = mpsc::unbounded_channel();
    runtime::handle().spawn(async move {
        let end = match run_stream(&cfg, messages, &tx).await {
            Ok(()) => AiEvent::Done,
            Err(e) => AiEvent::Error(e.message().to_string()),
        };
        let _ = tx.send(end);
    });
    rx
}

async fn run_stream(
    cfg: &AiConfig,
    messages: Vec<ChatMessage>,
    tx: &mpsc::UnboundedSender<AiEvent>,
) -> Result<(), DbError> {
    if cfg.model.trim().is_empty() {
        return Err(DbError::new("choose a model in the AI settings first"));
    }
    let body = serde_json::json!({
        "model": cfg.model.trim(),
        "messages": messages,
        "stream": true,
        "temperature": 0.2,
    });
    let mut resp = authed(client()?.post(cfg.endpoint("chat/completions")), cfg)
        .json(&body)
        .send()
        .await
        .map_err(|e| DbError::with_source(format!("cannot reach {}: {e}", cfg.base_url), e))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(DbError::new(http_error(status, &text)));
    }
    let mut sse = SseParser::default();
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| DbError::with_source(format!("stream interrupted: {e}"), e))?;
        let Some(chunk) = chunk else { return Ok(()) };
        for item in sse.feed(&chunk) {
            match item {
                SseItem::Delta(s) => {
                    if tx.send(AiEvent::Delta(s)).is_err() {
                        return Ok(()); // receiver dropped: cancelled
                    }
                }
                SseItem::Done => return Ok(()),
                SseItem::Error(e) => return Err(DbError::new(e)),
            }
        }
    }
}

// ---------------- SSE ----------------

#[derive(Debug, PartialEq)]
enum SseItem {
    Delta(String),
    Done,
    Error(String),
}

/// Incremental `text/event-stream` parser for chat-completion chunks.
/// Bytes are buffered until a full line arrives (UTF-8 may split mid-chunk).
#[derive(Default)]
struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    fn feed(&mut self, bytes: &[u8]) -> Vec<SseItem> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(nl) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data == "[DONE]" {
                out.push(SseItem::Done);
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { continue };
            if let Some(e) = v.get("error") {
                let msg = e
                    .get("message")
                    .and_then(|m| m.as_str())
                    .or_else(|| e.as_str())
                    .unwrap_or("server error");
                out.push(SseItem::Error(msg.to_string()));
                continue;
            }
            if let Some(s) = v
                .pointer("/choices/0/delta/content")
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty())
            {
                out.push(SseItem::Delta(s.to_string()));
            }
        }
        out
    }
}

// ---------------- prompt context ----------------

/// System prompt: role, dialect, and a compact schema listing
/// (`schema.table(col type PK, …)`), truncated to `SCHEMA_BUDGET`.
pub fn system_prompt(dialect: Option<DbType>, schemas: &[SchemaMeta]) -> String {
    let mut s = String::from(
        "You are the SQL assistant inside DataBridge, a desktop database client. \
         Reply in the language of the last user message. Answer concisely. Put every SQL statement in a ```sql fenced block. \
         Use only tables and columns that exist in the schema below; if something \
         is missing, say so instead of inventing it. Prefer read-only queries and \
         warn before any statement that modifies or drops data. Write exactly \
         one statement per ```sql block, valid for the dialect below. Messages \
         starting with [ok] or [error] are results of statements you proposed; \
         use them to continue or to fix the statement.\n",
    );
    match dialect {
        Some(t) => {
            let _ = writeln!(s, "Database dialect: {}.", t.label());
        }
        None => s.push_str("No database is connected.\n"),
    }
    if schemas.is_empty() {
        return s;
    }
    s.push_str("\nSchema:\n");
    let mut truncated = false;
    'outer: for sch in schemas {
        for t in sch.tables.iter().chain(sch.views.iter()) {
            let mut line = format!("{}.{}(", sch.name, t.name);
            for (i, c) in t.columns.iter().enumerate() {
                if i > 0 {
                    line.push_str(", ");
                }
                let _ = write!(line, "{} {}", c.name, c.type_);
                if c.primary_key {
                    line.push_str(" PK");
                }
            }
            line.push_str(")\n");
            if s.len() + line.len() > SCHEMA_BUDGET {
                truncated = true;
                break 'outer;
            }
            s.push_str(&line);
        }
    }
    if truncated {
        s.push_str("… (schema truncated)\n");
    }
    s
}

/// Split a message into `(is_code, text)` segments: prose vs fenced
/// code blocks. An unterminated trailing fence (still streaming) counts
/// as code.
pub fn segments(text: &str) -> Vec<(bool, String)> {
    let mut out: Vec<(bool, String)> = Vec::new();
    let mut code = false;
    let mut cur = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if !cur.trim().is_empty() {
                out.push((code, cur.trim_end().to_string()));
            }
            cur.clear();
            code = !code;
            continue;
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        out.push((code, cur.trim_end().to_string()));
    }
    out
}

// ---------------- conversations ----------------

/// One transcript entry. `Exec` records a statement the user or the
/// assistant ran and its outcome; it reaches the model as a user message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Entry {
    User { text: String },
    Assistant { text: String },
    Exec { sql: String, ok: bool, outcome: String },
}

impl Entry {
    pub fn to_wire(&self) -> ChatMessage {
        match self {
            Entry::User { text } => ChatMessage { role: Role::User, content: text.clone() },
            Entry::Assistant { text } => ChatMessage { role: Role::Assistant, content: text.clone() },
            Entry::Exec { sql, ok, outcome } => ChatMessage {
                role: Role::User,
                content: format!(
                    "[{}] executed:\n```sql\n{sql}\n```\n{outcome}",
                    if *ok { "ok" } else { "error" }
                ),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    /// Unix milliseconds of the last change — newest first.
    pub updated: u64,
    pub entries: Vec<Entry>,
}

impl Conversation {
    /// The first user message, shortened — the history list label.
    pub fn derive_title(entries: &[Entry]) -> String {
        let first = entries.iter().find_map(|e| match e {
            Entry::User { text } => Some(text.as_str()),
            _ => None,
        });
        let t = first.unwrap_or("New chat").split_whitespace().collect::<Vec<_>>().join(" ");
        if t.chars().count() > 60 {
            format!("{}…", t.chars().take(60).collect::<String>())
        } else {
            t
        }
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn load_history() -> Vec<Conversation> {
    let mut list: Vec<Conversation> = storage::config_dir()
        .and_then(|d| std::fs::read_to_string(d.join(HISTORY_FILE)).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    list.sort_by(|a, b| b.updated.cmp(&a.updated));
    list
}

pub fn save_history(list: &[Conversation]) {
    let Some(dir) = storage::config_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let keep = &list[..list.len().min(MAX_HISTORY)];
    match serde_json::to_string(keep) {
        Ok(json) => {
            if let Err(e) = std::fs::write(dir.join(HISTORY_FILE), json) {
                eprintln!("DataBridge: cannot write {HISTORY_FILE}: {e}");
            }
        }
        Err(e) => eprintln!("DataBridge: cannot encode the AI history: {e}"),
    }
}

/// Insert or replace `conv` (by id) at the front of `list`.
pub fn upsert(list: &mut Vec<Conversation>, conv: Conversation) {
    list.retain(|c| c.id != conv.id);
    list.insert(0, conv);
    list.truncate(MAX_HISTORY);
}

// ---------------- execution ----------------

/// Split a script on `;` outside quotes ('…', "…", `…`) and `--` / `/* */`
/// comments; empty pieces are dropped.
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = sql.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        cur.push(c);
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '-' if chars.peek() == Some(&'-') => {
                    for n in chars.by_ref() {
                        cur.push(n);
                        if n == '\n' {
                            break;
                        }
                    }
                }
                '/' if chars.peek() == Some(&'*') => {
                    let mut prev = '\0';
                    for n in chars.by_ref() {
                        cur.push(n);
                        if prev == '*' && n == '/' {
                            break;
                        }
                        prev = n;
                    }
                }
                ';' => {
                    cur.pop();
                    if !cur.trim().is_empty() {
                        out.push(cur.trim().to_string());
                    }
                    cur.clear();
                }
                _ => {}
            },
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Statements Auto mode never runs on its own: dropping/truncating
/// objects, `ALTER … DROP`, and UPDATE/DELETE without a WHERE clause.
/// Every statement of a multi-statement block is checked — a trailing
/// `DROP` must not hide behind a harmless first keyword.
pub fn needs_confirmation(sql: &str) -> bool {
    split_statements(sql).iter().any(|stmt| {
        let lower = stmt.to_ascii_lowercase();
        let has_word = |w: &str| {
            lower
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|x| x == w)
        };
        match first_keyword(stmt).as_str() {
            "drop" | "truncate" => true,
            "delete" | "update" => !has_word("where"),
            "alter" => has_word("drop"),
            _ => false,
        }
    })
}

/// Small models ignore "reply in the user's language" in the system
/// prompt; a short note on the latest user turn works far better.
/// Only Cyrillic is detected for now (Latin is the models' default
/// language anyway).
pub fn language_hint(text: &str) -> Option<&'static str> {
    let cyr = text.chars().filter(|c| ('\u{0400}'..='\u{04FF}').contains(c)).count();
    // A few Cyrillic letters are enough: questions often embed SQL.
    (cyr >= 3).then_some("(Отвечай на русском языке.)")
}

/// Normalized statement text for comparing attempts (whitespace, case,
/// trailing `;` ignored).
fn norm_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .trim()
        .to_lowercase()
}

/// Auto-mode loop guard: `sql` already failed earlier in this transcript,
/// so running it again cannot make progress.
pub fn repeats_failure(entries: &[Entry], sql: &str) -> bool {
    let n = norm_sql(sql);
    entries
        .iter()
        .any(|e| matches!(e, Entry::Exec { sql, ok: false, .. } if norm_sql(sql) == n))
}

/// The fenced code blocks of an answer, in order.
pub fn sql_of(text: &str) -> Vec<String> {
    segments(text).into_iter().filter(|(c, _)| *c).map(|(_, s)| s).collect()
}

/// A statement outcome as `(ok, text)`: a compact TSV preview for result
/// sets (capped for small context windows), the affected-row count for
/// everything else, the server message for errors.
pub fn describe_outcome(res: &Result<QueryPage, DbError>) -> (bool, String) {
    match res {
        Err(e) => (false, e.message().to_string()),
        Ok(p) if p.columns.is_empty() => (
            true,
            format!("{} row(s) affected · {} ms", p.rows_affected, p.elapsed_ms),
        ),
        Ok(p) => {
            let mut out = String::new();
            let _ = writeln!(
                out,
                "{} row(s){} · {} ms",
                p.rows.len(),
                if p.truncated { " (truncated)" } else { "" },
                p.elapsed_ms
            );
            let head: Vec<&str> = p.columns.iter().map(|c| c.name.as_str()).collect();
            out.push_str(&head.join("\t"));
            out.push('\n');
            for r in p.rows.iter().take(RESULT_ROWS_FOR_MODEL) {
                let cells: Vec<String> = r
                    .iter()
                    .map(|v| {
                        let s = v.display();
                        if s.chars().count() > 80 {
                            format!("{}…", s.chars().take(80).collect::<String>())
                        } else {
                            s
                        }
                    })
                    .collect();
                out.push_str(&cells.join("\t"));
                out.push('\n');
            }
            if p.rows.len() > RESULT_ROWS_FOR_MODEL {
                let _ = writeln!(out, "… {} more row(s)", p.rows.len() - RESULT_ROWS_FOR_MODEL);
            }
            (true, out.trim_end().to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_normalizes_slashes_and_v1() {
        let mut c = AiConfig::default();
        c.base_url = "http://localhost:1234/v1/".into();
        assert_eq!(c.endpoint("models"), "http://localhost:1234/v1/models");
        c.base_url = "http://localhost:11434".into();
        assert_eq!(c.endpoint("chat/completions"), "http://localhost:11434/v1/chat/completions");
        c.base_url = "https://example.com/openai/v1".into();
        assert_eq!(c.endpoint("models"), "https://example.com/openai/v1/models");
    }

    #[test]
    fn sse_parses_split_chunks_and_done() {
        let mut p = SseParser::default();
        let a = p.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel");
        assert!(a.is_empty());
        let b = p.feed(b"lo\"}}]}\n\ndata: {\"choices\":[{\"delta\":{}}]}\n\ndata: [DONE]\n\n");
        assert_eq!(b, vec![SseItem::Delta("Hello".into()), SseItem::Done]);
    }

    #[test]
    fn sse_handles_utf8_split_and_errors() {
        let mut p = SseParser::default();
        let s = "data: {\"choices\":[{\"delta\":{\"content\":\"привет\"}}]}\n".as_bytes();
        let (x, y) = s.split_at(48);
        assert!(p.feed(x).is_empty());
        assert_eq!(p.feed(y), vec![SseItem::Delta("привет".into())]);
        assert_eq!(
            p.feed(b"data: {\"error\":{\"message\":\"model not found\"}}\n"),
            vec![SseItem::Error("model not found".into())]
        );
    }

    #[test]
    fn models_are_sorted_ids() {
        let body = r#"{"object":"list","data":[{"id":"qwen2.5-coder:7b"},{"id":"llama3.2"}]}"#;
        assert_eq!(parse_models(body), vec!["llama3.2", "qwen2.5-coder:7b"]);
    }

    #[test]
    fn splits_prose_and_code() {
        let s = segments("Here:
```sql
SELECT 1;
```
Done.
```
SELECT 2");
        assert_eq!(
            s,
            vec![
                (false, "Here:".into()),
                (true, "SELECT 1;".into()),
                (false, "Done.".into()),
                (true, "SELECT 2".into()),
            ]
        );
    }

    #[test]
    fn http_error_reads_both_shapes() {
        let s = reqwest::StatusCode::NOT_FOUND;
        assert!(http_error(s, r#"{"error":{"message":"no model"}}"#).ends_with("no model"));
        assert!(http_error(s, r#"{"error":"boom"}"#).ends_with("boom"));
    }

    #[test]
    fn confirmation_rules() {
        assert!(needs_confirmation("DROP TABLE t"));
        assert!(needs_confirmation("delete from orders"));
        assert!(!needs_confirmation("DELETE FROM orders WHERE id = 1"));
        assert!(needs_confirmation("UPDATE t SET a = 1"));
        assert!(needs_confirmation("ALTER TABLE t DROP COLUMN a"));
        assert!(!needs_confirmation("INSERT INTO t VALUES (1)"));
        assert!(!needs_confirmation("SELECT * FROM where_log"));
    }

    #[test]
    fn exec_entry_reaches_the_model_as_user_text() {
        let e = Entry::Exec { sql: "SELECT 1".into(), ok: false, outcome: "no such table: dual".into() };
        let w = e.to_wire();
        assert_eq!(w.role, Role::User);
        assert!(w.content.starts_with("[error]") && w.content.contains("no such table"));
    }

    #[test]
    fn history_round_trips_and_titles() {
        let entries = vec![
            Entry::User { text: "create   20\norders please".into() },
            Entry::Assistant { text: "```sql\nSELECT 1\n```".into() },
            Entry::Exec { sql: "SELECT 1".into(), ok: true, outcome: "1 row(s)".into() },
        ];
        let c = Conversation { id: "a".into(), title: Conversation::derive_title(&entries), updated: 1, entries };
        assert_eq!(c.title, "create 20 orders please");
        let json = serde_json::to_string(&vec![c.clone()]).unwrap();
        let back: Vec<Conversation> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, vec![c.clone()]);
        let mut list = vec![c.clone()];
        upsert(&mut list, Conversation { updated: 2, ..c });
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].updated, 2);
    }

    #[test]
    fn describes_result_sets_and_errors() {
        use crate::core::model::{ColumnKind, ColumnMeta, Value};
        let col = |n: &str| ColumnMeta {
            name: n.into(),
            type_: "int".into(),
            nullable: true,
            primary_key: false,
            auto_increment: false,
            has_default: false,
            kind: ColumnKind::Text,
        };
        let page = QueryPage {
            columns: vec![col("id"), col("n")],
            rows: (0..25).map(|i| vec![Value::Int(i), Value::Null]).collect(),
            rows_affected: 0,
            truncated: false,
            elapsed_ms: 3,
        };
        let (ok, t) = describe_outcome(&Ok(page));
        assert!(ok);
        assert!(t.starts_with("25 row(s)"));
        assert!(t.contains("id\tn\n0\tnull"));
        assert!(t.ends_with("… 5 more row(s)"));
        let (ok, t) = describe_outcome(&Err(DbError::new("query failed: boom")));
        assert!(!ok && t.ends_with("boom"));
    }

    #[test]
    fn detects_repeated_failures() {
        let entries = vec![Entry::Exec {
            sql: "SELECT rnd()
FROM t;".into(),
            ok: false,
            outcome: "no such function: rnd".into(),
        }];
        assert!(repeats_failure(&entries, "select  rnd() from t"));
        assert!(!repeats_failure(&entries, "SELECT random() FROM t"));
    }

    #[test]
    fn trailing_drop_in_a_block_needs_confirmation() {
        let block = "CREATE TEMPORARY TABLE temp_dual AS SELECT 1;\n\nINSERT INTO orders SELECT * FROM temp_dual;\nDROP TABLE temp_dual;";
        assert!(needs_confirmation(block));
        assert_eq!(split_statements(block).len(), 3);
        assert_eq!(
            split_statements("INSERT INTO t VALUES ('a;b'); -- x;\nSELECT 1"),
            vec!["INSERT INTO t VALUES ('a;b')", "-- x;\nSELECT 1"]
        );
    }

    #[test]
    fn repeat_guard_matches_the_logged_session() {
        let sql = "CREATE TEMPORARY TABLE temp_dual AS SELECT employee_id FROM dual;\n\nDROP TABLE temp_dual;";
        let entries = vec![Entry::Exec { sql: sql.into(), ok: false, outcome: "no such table: dual".into() }];
        assert!(repeats_failure(&entries, sql));
    }

    #[test]
    fn russian_gets_a_language_hint() {
        assert!(language_hint("создай 20 заказов").is_some());
        assert!(language_hint("create 20 orders").is_none());
        assert!(language_hint("SELECT * FROM orders — почему пусто?").is_some());
    }
}
