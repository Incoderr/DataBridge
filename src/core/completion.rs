//! SQL autocompletion engine — pure and driver-agnostic, in the spirit of
//! JetBrains DataGrip: it reads the statement around the cursor and offers
//! what the word under the cursor can become.
//!
//! - after `FROM` / `JOIN` / `INTO` / `UPDATE` / `TABLE` (and commas in a
//!   `FROM` list): tables and views, other schemas qualified;
//! - after `alias.` / `table.`: that table's columns; after `schema.`: its
//!   tables;
//! - elsewhere: columns of the tables the statement references (aliases
//!   resolved, even when the `FROM` comes after the cursor), then tables,
//!   keywords and common functions;
//! - at a statement start: statement keywords;
//! - in `CREATE` / `ALTER`: column types and constraints, tables after
//!   `REFERENCES`, and the column list of `INTO t (` / `REFERENCES t (`.
//!
//! [`predict`] is the inline "next step" (grey ghost text, Tab accepts):
//! `FROM` after `SELECT *`, `BY` after `ORDER`, the column list of an
//! `INSERT`, `SET` after `UPDATE t`, a foreign-key `ON` for a `JOIN`, …
//!
//! Matching is case-insensitive: prefix first, then a word-part prefix
//! (`id` → `customer_id`), then a subsequence anchored at the first letter
//! (`cid` → `customer_id`). Nothing is offered inside strings or comments.
//! The editor adapter lives in `ui/completion.rs`.

use std::sync::Arc;

use crate::core::model::{DbType, SchemaMeta, TableMeta};

/// Cap on suggestions per request — the menu is a picker, not a browser.
pub const MAX_ITEMS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    Keyword,
    Function,
    Schema,
    Table,
    View,
    Column,
    Alias,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    /// Shown in the menu.
    pub label: String,
    /// Replaces the typed prefix (identifiers quoted when the dialect needs it).
    pub insert: String,
    pub kind: SuggestionKind,
    /// Description shown beside the selected item: column type and
    /// owner, "view", …
    pub detail: Option<String>,
    /// The label starts with the typed prefix (the menu highlights it).
    pub prefix_match: bool,
}

/// What to show: `start..end` is the byte range the chosen item replaces.
#[derive(Debug, Default)]
pub struct Completion {
    pub start: usize,
    pub end: usize,
    pub prefix: String,
    pub items: Vec<Suggestion>,
}

const STATEMENT_KEYWORDS: &[&str] = &[
    "SELECT", "INSERT INTO", "UPDATE", "DELETE FROM", "WITH", "CREATE TABLE",
    "CREATE INDEX", "CREATE VIEW", "ALTER TABLE", "DROP TABLE", "DROP VIEW", "EXPLAIN",
    "BEGIN", "COMMIT", "ROLLBACK",
];

const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "AND", "OR", "NOT", "NULL", "IS NULL", "IS NOT NULL", "IN",
    "LIKE", "BETWEEN", "EXISTS", "AS", "ON", "USING", "JOIN", "LEFT JOIN", "RIGHT JOIN",
    "INNER JOIN", "CROSS JOIN", "FULL JOIN", "GROUP BY", "ORDER BY", "HAVING", "LIMIT",
    "OFFSET", "DISTINCT", "UNION", "UNION ALL", "CASE", "WHEN", "THEN", "ELSE", "END",
    "ASC", "DESC", "SET", "VALUES", "RETURNING", "DEFAULT", "PRIMARY KEY", "FOREIGN KEY",
    "REFERENCES", "NOT NULL", "UNIQUE", "CHECK", "TRUE", "FALSE", "INTEGER", "TEXT",
    "VARCHAR", "BOOLEAN", "TIMESTAMP", "CURRENT_TIMESTAMP",
];

/// Column types and constraints offered inside `CREATE` / `ALTER`.
const DDL_KEYWORDS: &[&str] = &[
    "INTEGER", "INT", "BIGINT", "SMALLINT", "TEXT", "VARCHAR(", "CHAR(", "BOOLEAN", "REAL",
    "DOUBLE PRECISION", "NUMERIC(", "DECIMAL(", "DATE", "TIME", "TIMESTAMP", "BLOB", "JSON",
    "PRIMARY KEY", "NOT NULL", "NULL", "UNIQUE", "DEFAULT", "REFERENCES", "FOREIGN KEY",
    "CHECK", "ON DELETE CASCADE", "ON DELETE SET NULL", "AUTOINCREMENT", "AUTO_INCREMENT",
    "SERIAL", "CONSTRAINT", "ADD COLUMN", "DROP COLUMN", "RENAME TO", "IF NOT EXISTS",
];

const FUNCTIONS: &[&str] = &[
    "COUNT", "SUM", "AVG", "MIN", "MAX", "COALESCE", "NULLIF", "LOWER", "UPPER", "LENGTH",
    "TRIM", "SUBSTR", "REPLACE", "ROUND", "ABS", "CAST",
];

/// Words that end a table reference instead of naming its alias.
const NOT_ALIAS: &[&str] = &[
    "where", "join", "inner", "left", "right", "full", "cross", "outer", "natural", "on",
    "using", "set", "values", "group", "order", "limit", "offset", "having", "union",
    "returning", "select", "default", "window", "for", "except", "intersect", "lateral",
];

/// Compute suggestions for the cursor at byte `cursor` of `text`.
/// `default_schema` is the schema unqualified names resolve against.
pub fn complete(
    text: &str,
    cursor: usize,
    schemas: &[SchemaMeta],
    default_schema: Option<&str>,
    dialect: DbType,
) -> Completion {
    let cursor = floor_char_boundary(text, cursor);
    let mut out = Completion {
        start: cursor,
        end: cursor,
        ..Default::default()
    };
    let (before, open) = lex(&text[..cursor]);
    if open {
        return out; // inside a string, comment or quoted identifier
    }

    // The word being typed.
    let bytes = text.as_bytes();
    let mut start = cursor;
    while start > 0 && is_ident_byte(bytes[start - 1]) {
        start -= 1;
    }
    let prefix = &text[start..cursor];
    if prefix.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
        return out; // a number
    }
    out.start = start;
    out.prefix = prefix.to_string();

    // Tokens of the current statement before the word.
    let stmt_start = before
        .iter()
        .rev()
        .find(|t| t.tok == Tok::Punct(';'))
        .map_or(0, |t| t.end);
    let prev: Vec<&Token> = before
        .iter()
        .filter(|t| t.start >= stmt_start && t.end <= start)
        .collect();

    // The whole statement (FROM may follow the cursor) for alias scope.
    let (all, _) = lex(text);
    let stmt_end = all
        .iter()
        .find(|t| t.start >= cursor && t.tok == Tok::Punct(';'))
        .map_or(text.len(), |t| t.start);
    let stmt: Vec<&Token> = all
        .iter()
        .filter(|t| t.start >= stmt_start && t.end <= stmt_end)
        .collect();

    let default = default_schema
        .and_then(|d| schemas.iter().find(|s| s.name == d))
        .or_else(|| schemas.first());
    let scope = scope_of(&stmt, start, schemas, default);

    let mut b = Builder::new(prefix);

    // `qualifier.` before the word.
    if prev.last().is_some_and(|t| t.tok == Tok::Punct('.')) {
        let Some(Tok::Word(q, _)) = prev.len().checked_sub(2).map(|i| &prev[i].tok) else {
            return out;
        };
        if let Some(entry) = scope.iter().find(|e| {
            e.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q))
                || e.table.name.eq_ignore_ascii_case(q)
        }) {
            b.columns(&entry.table, None, dialect);
        } else if let Some(t) = find_table(schemas, default, None, q) {
            b.columns(&t, None, dialect);
        } else if let Some(s) = schemas.iter().find(|s| s.name.eq_ignore_ascii_case(q)) {
            b.tables(s, false, dialect);
        }
        out.items = b.finish();
        return out;
    }

    let last = prev.last().map(|t| &t.tok);
    let last_kw = last.and_then(keyword_of);
    let before_last_kw = prev
        .len()
        .checked_sub(2)
        .and_then(|i| keyword_of(&prev[i].tok));
    let clause = prev.iter().rev().find_map(|t| {
        keyword_of(&t.tok).filter(|k| {
            matches!(
                k.as_str(),
                "select" | "from" | "join" | "where" | "on" | "set" | "group" | "order"
                    | "having" | "into" | "values" | "update" | "limit" | "returning"
            )
        })
    });

    let ddl = prev
        .first()
        .and_then(|t| keyword_of(&t.tok))
        .is_some_and(|k| k == "create" || k == "alter");

    // `INSERT INTO t (|` / `REFERENCES t (|` (and commas in that list):
    // the target table's columns.
    if let Some(target) = column_list_target(&prev, schemas, default) {
        b.columns(&target, Some(&target.name), dialect);
        out.items = b.finish();
        return out;
    }

    let table_context = match last_kw.as_deref() {
        Some("from" | "join" | "into" | "update" | "references") => true,
        // `CREATE TABLE x` names a new table — nothing to suggest.
        Some("table") => before_last_kw.as_deref() != Some("create"),
        _ => last == Some(&Tok::Punct(',')) && clause.as_deref() == Some("from"),
    };

    if table_context {
        if let Some(d) = default {
            b.tables(d, false, dialect);
        }
        for s in schemas.iter().filter(|s| Some(s.name.as_str()) != default.map(|d| d.name.as_str())) {
            b.schema(s, dialect);
            b.tables(s, true, dialect);
        }
    } else if prefix.is_empty() || (last_kw.as_deref() == Some("table")) {
        // Elsewhere the menu waits for a letter; `CREATE TABLE` names are new.
    } else if prev.is_empty() {
        b.keywords(STATEMENT_KEYWORDS);
    } else if ddl {
        // Column definitions name new things: types and constraints only.
        b.keywords(DDL_KEYWORDS);
    } else {
        if scope.is_empty() {
            // No FROM yet: every column of the default schema, labelled by table.
            if let Some(d) = default {
                for t in d.tables.iter().chain(d.views.iter()) {
                    b.columns(t, Some(&t.name), dialect);
                }
            }
        } else {
            for e in &scope {
                let owner = e.alias.as_deref().unwrap_or(&e.table.name);
                b.columns(&e.table, Some(owner), dialect);
            }
            for e in &scope {
                if let Some(a) = &e.alias {
                    b.push(a.clone(), a.clone(), SuggestionKind::Alias, Some(e.table.name.clone()));
                }
            }
        }
        b.functions(FUNCTIONS);
        b.keywords(KEYWORDS);
        b.keywords(STATEMENT_KEYWORDS);
        if let Some(d) = default {
            b.tables(d, false, dialect);
        }
    }
    out.items = b.finish();
    out
}

// ---------------- suggestions ----------------

struct Builder<'a> {
    prefix: &'a str,
    /// (score, insertion order, suggestion)
    items: Vec<(u8, usize, Suggestion)>,
}

impl<'a> Builder<'a> {
    fn new(prefix: &'a str) -> Self {
        Self {
            prefix,
            items: Vec::new(),
        }
    }

    fn push(&mut self, label: String, insert: String, kind: SuggestionKind, detail: Option<String>) {
        let Some(score) = match_score(&label, self.prefix) else {
            return;
        };
        if self.items.iter().any(|(_, _, s)| {
            s.label == label && s.kind == kind && s.detail == detail
        }) {
            return;
        }
        let order = self.items.len();
        self.items.push((
            score,
            order,
            Suggestion {
                label,
                insert,
                kind,
                detail,
                prefix_match: score == 0,
            },
        ));
    }

    fn keywords(&mut self, words: &[&str]) {
        for w in words {
            self.push(w.to_string(), w.to_string(), SuggestionKind::Keyword, None);
        }
    }

    fn functions(&mut self, names: &[&str]) {
        for f in names {
            self.push(f.to_string(), format!("{f}("), SuggestionKind::Function, Some("function".into()));
        }
    }

    fn schema(&mut self, s: &SchemaMeta, dialect: DbType) {
        self.push(s.name.clone(), quote_ident(&s.name, dialect), SuggestionKind::Schema, Some("schema".into()));
    }

    /// Tables and views of `s`; `qualified` inserts `schema.table`.
    fn tables(&mut self, s: &SchemaMeta, qualified: bool, dialect: DbType) {
        for (list, kind, what) in [
            (&s.tables, SuggestionKind::Table, "table"),
            (&s.views, SuggestionKind::View, "view"),
        ] {
            for t in list {
                let (label, insert, detail) = if qualified {
                    (
                        format!("{}.{}", s.name, t.name),
                        format!("{}.{}", quote_ident(&s.name, dialect), quote_ident(&t.name, dialect)),
                        what.to_string(),
                    )
                } else {
                    (t.name.clone(), quote_ident(&t.name, dialect), what.to_string())
                };
                self.push(label, insert, kind, Some(detail));
            }
        }
    }

    /// Columns of `t`; `owner` names the table / alias in the hint.
    fn columns(&mut self, t: &TableMeta, owner: Option<&str>, dialect: DbType) {
        for c in &t.columns {
            let mut detail = match owner {
                Some(o) => format!("{o}.{}: {}", c.name, c.type_),
                None => format!("{}: {}", c.name, c.type_),
            };
            if c.primary_key {
                detail.push_str(" · primary key");
            }
            if !c.nullable && !c.primary_key {
                detail.push_str(" · not null");
            }
            self.push(c.name.clone(), quote_ident(&c.name, dialect), SuggestionKind::Column, Some(detail));
        }
    }

    fn finish(mut self) -> Vec<Suggestion> {
        // Stable by score, then by the context's own order of groups.
        self.items.sort_by_key(|(score, order, _)| (*score, *order));
        self.items.into_iter().take(MAX_ITEMS).map(|(_, _, s)| s).collect()
    }
}

/// 0: prefix, 1: a word part starts with it (`_`/camel boundary),
/// 2: subsequence anchored at the first letter; `None`: no match.
/// An empty prefix matches everything at 0.
fn match_score(label: &str, prefix: &str) -> Option<u8> {
    if prefix.is_empty() {
        return Some(0);
    }
    let l = label.to_lowercase();
    let p = prefix.to_lowercase();
    if l.starts_with(&p) {
        return Some(0);
    }
    let chars: Vec<char> = label.chars().collect();
    for i in 1..chars.len() {
        let boundary = matches!(chars[i - 1], '_' | '.' | ' ')
            || (chars[i - 1].is_lowercase() && chars[i].is_uppercase());
        if boundary {
            let rest: String = chars[i..].iter().collect::<String>().to_lowercase();
            if rest.starts_with(&p) {
                return Some(1);
            }
        }
    }
    let mut it = l.chars();
    let mut pc = p.chars();
    let first = pc.next()?;
    if it.next() != Some(first) {
        return None;
    }
    pc.all(|c| it.any(|x| x == c)).then_some(2)
}

/// Quote an identifier when the dialect would not read it back verbatim:
/// Postgres folds unquoted names to lower case, the others only need
/// quoting for unusual characters.
pub fn quote_ident(name: &str, dialect: DbType) -> String {
    let plain = !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name.chars().all(|c| match dialect {
            DbType::PostgreSql => c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_',
            _ => c.is_ascii_alphanumeric() || c == '_',
        });
    if plain {
        return name.to_string();
    }
    match dialect {
        DbType::MySql | DbType::MariaDb => format!("`{}`", name.replace('`', "``")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

/// `… INTO t (a, |` / `REFERENCES t (|`: the table whose column list the
/// cursor is in.
fn column_list_target(
    prev: &[&Token],
    schemas: &[SchemaMeta],
    default: Option<&SchemaMeta>,
) -> Option<Arc<TableMeta>> {
    // Walk back over `ident ,` pairs to the opening parenthesis.
    let mut i = prev.len();
    loop {
        let t = &prev.get(i.checked_sub(1)?)?.tok;
        match t {
            Tok::Punct('(') => break,
            Tok::Punct(',') | Tok::Word(..) => i -= 1,
            _ => return None,
        }
    }
    let open = i - 1;
    let Tok::Word(name, _) = &prev.get(open.checked_sub(1)?)?.tok else {
        return None;
    };
    // [schema .] name, preceded by INTO / REFERENCES
    let (schema, kw_ix) = if open >= 3 && prev[open - 2].tok == Tok::Punct('.') {
        match &prev[open - 3].tok {
            Tok::Word(s, _) => (Some(s.as_str()), open.checked_sub(4)?),
            _ => return None,
        }
    } else {
        (None, open.checked_sub(2)?)
    };
    let kw = keyword_of(&prev[kw_ix].tok)?;
    if kw != "into" && kw != "references" {
        return None;
    }
    find_table(schemas, default, schema, name)
}

/// The inline "next step" at `cursor`: text to show as grey ghost text
/// and insert on Tab. Only right after whitespace — while a word is being
/// typed the completion menu is in charge. Keywords follow the case of the
/// statement's previous keyword.
pub fn predict(
    text: &str,
    cursor: usize,
    schemas: &[SchemaMeta],
    default_schema: Option<&str>,
    dialect: DbType,
) -> Option<String> {
    let cursor = floor_char_boundary(text, cursor);
    let before = &text[..cursor];
    if !before.ends_with([' ', '\t']) {
        return None;
    }
    let (toks, open) = lex(before);
    if open {
        return None;
    }
    let stmt_start = toks
        .iter()
        .rev()
        .find(|t| t.tok == Tok::Punct(';'))
        .map_or(0, |t| t.end);
    let prev: Vec<&Token> = toks.iter().filter(|t| t.start >= stmt_start).collect();
    let last = prev.last()?;
    let last_kw = keyword_of(&last.tok);
    let lower = prev
        .iter()
        .rev()
        .find_map(|t| match &t.tok {
            Tok::Word(w, false) if is_keyword(w) => Some(w),
            _ => None,
        })
        .is_some_and(|w| !w.chars().any(|c| c.is_ascii_uppercase()));
    let kw = |s: &str| if lower { s.to_lowercase() } else { s.to_string() };

    let default = default_schema
        .and_then(|d| schemas.iter().find(|s| s.name == d))
        .or_else(|| schemas.first());
    let first_kw = prev.first().and_then(|t| keyword_of(&t.tok));
    let has = |k: &str| prev.iter().any(|t| keyword_of(&t.tok).is_some_and(|w| w == k));

    match last_kw.as_deref() {
        Some("order" | "group") => return Some(kw("BY ")),
        Some("insert") => return Some(kw("INTO ")),
        Some("delete") => return Some(kw("FROM ")),
        Some("values") => return Some("(".into()),
        _ => {}
    }
    if last.tok == Tok::Punct('*') && first_kw.as_deref() == Some("select") && !has("from") {
        return Some(kw("FROM "));
    }

    // Right after a table reference: what usually follows it.
    let scope = scope_of(&prev, usize::MAX, schemas, default);
    let entry = scope.last()?;
    let ref_end = last_table_ref_end(&prev)?;
    if ref_end != prev.len() {
        return None; // something already follows the table reference
    }
    let ref_kw = prev
        .iter()
        .rev()
        .find_map(|t| keyword_of(&t.tok).filter(|k| matches!(k.as_str(), "from" | "join" | "update" | "into")))?;
    match ref_kw.as_str() {
        "update" => Some(kw("SET ")),
        "into" => {
            let cols: Vec<String> = entry
                .table
                .columns
                .iter()
                .filter(|c| !c.auto_increment)
                .map(|c| quote_ident(&c.name, dialect))
                .collect();
            if cols.is_empty() {
                return None;
            }
            Some(format!("({}) {}(", cols.join(", "), kw("VALUES ")))
        }
        "join" => {
            let joined = entry;
            let on = scope[..scope.len() - 1]
                .iter()
                .rev()
                .find_map(|other| fk_guess(joined, other).or_else(|| fk_guess(other, joined)))
                .map(|(a, b)| format!("{}{a} = {b}", kw("ON ")));
            Some(on.unwrap_or_else(|| kw("ON ")))
        }
        _ if first_kw.as_deref() == Some("select") || first_kw.as_deref() == Some("delete") => {
            (!has("where")).then(|| kw("WHERE "))
        }
        _ => None,
    }
}

/// A word the keyword lists know (case-insensitive) — table and column
/// names don't decide the case of predicted keywords.
fn is_keyword(word: &str) -> bool {
    STATEMENT_KEYWORDS
        .iter()
        .chain(KEYWORDS)
        .flat_map(|k| k.split(' '))
        .any(|k| k.eq_ignore_ascii_case(word))
}

/// Token count up to the end of the last `FROM/JOIN/UPDATE/INTO
/// [schema.]table [[AS] alias]` reference, when it ends the token list.
fn last_table_ref_end(prev: &[&Token]) -> Option<usize> {
    let kw_ix = prev.iter().rposition(|t| {
        keyword_of(&t.tok).is_some_and(|k| matches!(k.as_str(), "from" | "join" | "update" | "into"))
    })?;
    let mut i = kw_ix + 1;
    if !matches!(prev.get(i)?.tok, Tok::Word(..)) {
        return None;
    }
    i += 1;
    if prev.get(i).is_some_and(|t| t.tok == Tok::Punct('.')) {
        if !matches!(prev.get(i + 1)?.tok, Tok::Word(..)) {
            return None;
        }
        i += 2;
    }
    if prev.get(i).and_then(|t| keyword_of(&t.tok)).as_deref() == Some("as") {
        i += 1;
    }
    if let Some(Tok::Word(a, quoted)) = prev.get(i).map(|t| &t.tok) {
        if *quoted || !NOT_ALIAS.contains(&a.to_ascii_lowercase().as_str()) {
            i += 1;
        }
    }
    Some(i)
}

/// `from` holds a column naming `to` (`category_id`, `CategoryID` for
/// `categories`) and `to` has a primary key: `(from.col, to.pk)`.
fn fk_guess(from: &ScopeEntry, to: &ScopeEntry) -> Option<(String, String)> {
    let pk = to.table.columns.iter().find(|c| c.primary_key)?;
    let norm = |s: &str| s.to_lowercase().replace('_', "");
    let target = norm(&to.table.name);
    let singular = if let Some(stem) = target.strip_suffix("ies") {
        format!("{stem}y")
    } else {
        target.strip_suffix('s').unwrap_or(&target).to_string()
    };
    let pk_name = norm(&pk.name);
    let col = from.table.columns.iter().find(|c| {
        let n = norm(&c.name);
        [&target, &singular].iter().any(|t| n == format!("{t}id") || n == format!("{t}{pk_name}"))
    })?;
    let name = |e: &ScopeEntry| e.alias.clone().unwrap_or_else(|| e.table.name.clone());
    Some((format!("{}.{}", name(from), col.name), format!("{}.{}", name(to), pk.name)))
}

// ---------------- scope: tables the statement references ----------------

struct ScopeEntry {
    table: Arc<TableMeta>,
    alias: Option<String>,
}

/// `FROM a x, b AS y JOIN c ON …`, `UPDATE t`, `INSERT INTO t`: resolved
/// tables with their aliases. The word under the cursor (`typing`) is
/// never taken for an alias.
fn scope_of(
    stmt: &[&Token],
    typing: usize,
    schemas: &[SchemaMeta],
    default: Option<&SchemaMeta>,
) -> Vec<ScopeEntry> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < stmt.len() {
        let kw = keyword_of(&stmt[i].tok);
        i += 1;
        if !matches!(kw.as_deref(), Some("from" | "join" | "update" | "into")) {
            continue;
        }
        let in_from = kw.as_deref() == Some("from");
        loop {
            // [schema .] table
            let Some(Tok::Word(first, _)) = stmt.get(i).map(|t| &t.tok) else {
                break;
            };
            if stmt[i].start == typing || keyword_of(&stmt[i].tok).is_some_and(|k| k == "select") {
                break;
            }
            let (schema, name) = if stmt.get(i + 1).is_some_and(|t| t.tok == Tok::Punct('.')) {
                match stmt.get(i + 2).map(|t| &t.tok) {
                    Some(Tok::Word(n, _)) => {
                        i += 3;
                        (Some(first.as_str()), n.as_str())
                    }
                    _ => break,
                }
            } else {
                i += 1;
                (None, first.as_str())
            };
            // [AS] alias
            let mut alias = None;
            let mut j = i;
            if stmt.get(j).and_then(|t| keyword_of(&t.tok)).as_deref() == Some("as") {
                j += 1;
            }
            if let Some(t) = stmt.get(j) {
                if let Tok::Word(a, quoted) = &t.tok {
                    let reserved = !*quoted && NOT_ALIAS.contains(&a.to_ascii_lowercase().as_str());
                    if !reserved && t.start != typing {
                        alias = Some(a.clone());
                        i = j + 1;
                    }
                }
            }
            if let Some(table) = find_table(schemas, default, schema, name) {
                out.push(ScopeEntry { table, alias });
            }
            if in_from && stmt.get(i).is_some_and(|t| t.tok == Tok::Punct(',')) {
                i += 1;
                continue;
            }
            break;
        }
    }
    out
}

/// Look a table / view up: exact name first, then case-insensitive;
/// unqualified names search the default schema, then every schema.
fn find_table(
    schemas: &[SchemaMeta],
    default: Option<&SchemaMeta>,
    schema: Option<&str>,
    name: &str,
) -> Option<Arc<TableMeta>> {
    let candidates: Vec<&SchemaMeta> = match schema {
        Some(s) => schemas.iter().filter(|m| m.name.eq_ignore_ascii_case(s)).collect(),
        None => default.into_iter().chain(schemas.iter()).collect(),
    };
    for exact in [true, false] {
        for s in &candidates {
            if let Some(t) = s.tables.iter().chain(s.views.iter()).find(|t| {
                if exact { t.name == name } else { t.name.eq_ignore_ascii_case(name) }
            }) {
                return Some(t.clone());
            }
        }
    }
    None
}

// ---------------- lexer ----------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// Identifier or keyword (quotes stripped) and whether it was quoted.
    Word(String, bool),
    Punct(char),
    Str,
}

#[derive(Debug)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
}

/// Lower-cased unquoted word — the keyword reading of a token.
fn keyword_of(t: &Tok) -> Option<String> {
    match t {
        Tok::Word(w, false) => Some(w.to_ascii_lowercase()),
        _ => None,
    }
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Tokenize SQL; the flag reports text ending inside a string, comment or
/// quoted identifier.
fn lex(s: &str) -> (Vec<Token>, bool) {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'-' && b.get(i + 1) == Some(&b'-') {
            match s[i..].find('\n') {
                Some(n) => i += n + 1,
                None => return (out, true),
            }
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            match s[i + 2..].find("*/") {
                Some(n) => i += n + 4,
                None => return (out, true),
            }
        } else if c == b'\'' {
            let mut j = i + 1;
            loop {
                match b.get(j) {
                    None => return (out, true),
                    Some(b'\'') if b.get(j + 1) == Some(&b'\'') => j += 2,
                    Some(b'\'') => break,
                    Some(_) => j += 1,
                }
            }
            out.push(Token { tok: Tok::Str, start: i, end: j + 1 });
            i = j + 1;
        } else if matches!(c, b'"' | b'`' | b'[') {
            let close = if c == b'[' { ']' } else { c as char };
            match s[i + 1..].find(close) {
                Some(n) => {
                    out.push(Token {
                        tok: Tok::Word(s[i + 1..i + 1 + n].to_string(), true),
                        start: i,
                        end: i + n + 2,
                    });
                    i += n + 2;
                }
                None => return (out, true),
            }
        } else if is_ident_byte(c) {
            let mut j = i;
            while j < b.len() && is_ident_byte(b[j]) {
                j += 1;
            }
            out.push(Token { tok: Tok::Word(s[i..j].to_string(), false), start: i, end: j });
            i = j;
        } else {
            out.push(Token { tok: Tok::Punct(c as char), start: i, end: i + 1 });
            i += 1;
        }
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::{ColumnKind, ColumnMeta, NodeKind};

    fn table(schema: &str, name: &str, cols: &[&str]) -> Arc<TableMeta> {
        Arc::new(TableMeta {
            name: name.into(),
            schema: schema.into(),
            kind: NodeKind::Table,
            columns: cols
                .iter()
                .map(|c| ColumnMeta {
                    name: c.to_string(),
                    type_: "int".into(),
                    nullable: true,
                    primary_key: *c == "id",
                    auto_increment: false,
                    has_default: false,
                    kind: ColumnKind::Int,
                })
                .collect(),
            row_count: 0,
        })
    }

    fn schemas() -> Vec<SchemaMeta> {
        vec![
            SchemaMeta {
                name: "public".into(),
                tables: vec![
                    table("public", "customers", &["id", "name", "country_id"]),
                    table("public", "orders", &["id", "customer_id", "total"]),
                ],
                views: vec![],
                functions: vec![],
            },
            SchemaMeta {
                name: "audit".into(),
                tables: vec![table("audit", "log", &["at", "message"])],
                views: vec![],
                functions: vec![],
            },
        ]
    }

    /// Complete at the `|` marker.
    fn labels(sql: &str) -> Vec<String> {
        let cursor = sql.find('|').unwrap();
        let text = sql.replace('|', "");
        complete(&text, cursor, &schemas(), Some("public"), DbType::PostgreSql)
            .items
            .into_iter()
            .map(|s| s.label)
            .collect()
    }

    #[test]
    fn tables_after_from() {
        let l = labels("SELECT * FROM |");
        assert_eq!(&l[..2], ["customers", "orders"]);
        assert!(l.contains(&"audit.log".to_string()));
        assert_eq!(labels("select * from or|"), ["orders"]);
        assert_eq!(labels("SELECT * FROM customers c JOIN o|")[0], "orders");
        assert!(labels("CREATE TABLE |").is_empty());
    }

    #[test]
    fn alias_columns_even_before_from() {
        assert_eq!(labels("SELECT c.| FROM customers c"), ["id", "name", "country_id"]);
        assert_eq!(labels("SELECT o.cu| FROM customers AS c, orders o"), ["customer_id"]);
        assert_eq!(labels("SELECT * FROM audit.|"), ["log"]);
    }

    #[test]
    fn scope_columns_then_keywords() {
        let l = labels("SELECT * FROM orders WHERE to|");
        assert_eq!(l[0], "total");
        let l = labels("SELECT * FROM orders o WHERE o.total > 1 OR|");
        assert!(l.contains(&"ORDER BY".to_string()) && l.contains(&"OR".to_string()));
        assert_eq!(labels("SEL|"), ["SELECT"]);
    }

    #[test]
    fn fuzzy_and_word_part_matching() {
        assert_eq!(match_score("customer_id", "cust"), Some(0));
        assert_eq!(match_score("customer_id", "id"), Some(1));
        assert_eq!(match_score("customer_id", "cid"), Some(2));
        assert_eq!(match_score("customer_id", "xid"), None);
        assert!(labels("SELECT * FROM orders WHERE cid|").contains(&"customer_id".to_string()));
    }

    #[test]
    fn nothing_inside_strings_and_comments() {
        assert!(labels("SELECT * FROM orders WHERE name = 'cu|").is_empty());
        assert!(labels("-- from |").is_empty());
    }

    #[test]
    fn ddl_context_offers_types_not_columns() {
        let l = labels("CREATE TABLE test (\n    ca|");
        assert!(!l.iter().any(|x| x == "customer_id"));
        let l = labels("CREATE TABLE t (id INTEGER PRIMARY KEY, c INT REFERENCES |");
        assert_eq!(&l[..2], ["customers", "orders"]);
        assert_eq!(labels("CREATE TABLE t (c INT REFERENCES customers(|"), ["id", "name", "country_id"]);
        assert_eq!(labels("INSERT INTO orders (id, |"), ["id", "customer_id", "total"]);
    }

    fn ghost(sql: &str) -> Option<String> {
        predict(sql, sql.len(), &schemas(), Some("public"), DbType::PostgreSql)
    }

    #[test]
    fn predicts_the_next_step() {
        assert_eq!(ghost("SELECT * ").as_deref(), Some("FROM "));
        assert_eq!(ghost("select * ").as_deref(), Some("from "));
        assert_eq!(ghost("SELECT * FROM orders ").as_deref(), Some("WHERE "));
        assert_eq!(ghost("SELECT * FROM orders o ").as_deref(), Some("WHERE "));
        assert_eq!(ghost("SELECT * FROM orders WHERE ").as_deref(), None);
        assert_eq!(ghost("SELECT id FROM orders ORDER ").as_deref(), Some("BY "));
        assert_eq!(ghost("UPDATE orders ").as_deref(), Some("SET "));
        assert_eq!(
            ghost("INSERT INTO customers ").as_deref(),
            Some("(id, name, country_id) VALUES (")
        );
        assert_eq!(
            ghost("SELECT * FROM orders o JOIN customers c ").as_deref(),
            Some("ON o.customer_id = c.id")
        );
        assert_eq!(ghost("SELECT * FROM orders").as_deref(), None);
    }

    #[test]
    fn quoting_per_dialect() {
        assert_eq!(quote_ident("orders", DbType::PostgreSql), "orders");
        assert_eq!(quote_ident("Orders", DbType::PostgreSql), "\"Orders\"");
        assert_eq!(quote_ident("Orders", DbType::Sqlite), "Orders");
        assert_eq!(quote_ident("order items", DbType::MySql), "`order items`");
    }
}
