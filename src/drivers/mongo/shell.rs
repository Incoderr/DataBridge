//! A small mongosh subset for the query editor (and the AI chat, which
//! writes shell syntax): one statement per run.
//!
//! ```text
//! use shop
//! { "ping": 1 }                                  // raw command
//! db.runCommand({ dbStats: 1 })   db.adminCommand({ … })
//! db.getCollectionNames()   db.createCollection("x")   db.dropDatabase()
//! db.orders.find({ total: { $gt: 10 } }, { _id: 0 }).sort({ total: -1 }).limit(5)
//! db.getSiblingDB("other").getCollection("my.coll").aggregate([ … ])
//! ```
//!
//! Literals are JavaScript-ish: unquoted / single-quoted keys, trailing
//! commas, comments, `ObjectId("…")`, `ISODate("…")` / `new Date(…)`,
//! `NumberLong`, `NumberInt`, `NumberDecimal`, `UUID("…")`, `/re/flags`,
//! and Extended JSON wrappers (`{"$oid": "…"}`, `{"$date": "…"}`, …).
//! Objects keep their key order (commands and sort specs depend on it).

use bson::oid::ObjectId;
use bson::spec::BinarySubtype;
use bson::{Binary, Bson, Document};

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `use <db>`: switch the session's default database.
    Use(String),
    /// A raw command document (`{…}`, `db.runCommand`, `db.adminCommand`).
    Command { db: Option<String>, doc: Document },
    /// A database-level helper: `db.getCollectionNames()`, …
    Db {
        db: Option<String>,
        method: String,
        args: Vec<Bson>,
    },
    /// A collection method plus chained cursor modifiers
    /// (`.sort(…)`, `.limit(n)`, `.skip(n)`, …).
    Collection {
        db: Option<String>,
        coll: String,
        method: String,
        args: Vec<Bson>,
        modifiers: Vec<(String, Vec<Bson>)>,
    },
}

pub fn parse(src: &str) -> Result<Statement, String> {
    let mut p = Parser {
        s: src.chars().collect(),
        pos: 0,
    };
    p.skip_ws();
    let stmt = p.statement()?;
    p.skip_ws();
    while p.eat(';') {
        p.skip_ws();
    }
    if p.pos < p.s.len() {
        return Err(format!(
            "unexpected `{}` — one statement per run",
            p.s[p.pos..].iter().take(20).collect::<String>()
        ));
    }
    Ok(stmt)
}

/// Parse one literal value (the record editor's JSON cells).
pub fn parse_value(src: &str) -> Result<Bson, String> {
    let mut p = Parser {
        s: src.chars().collect(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.value()?;
    p.skip_ws();
    if p.pos < p.s.len() {
        return Err("unexpected text after the value".into());
    }
    Ok(v)
}

struct Parser {
    s: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.pos).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        self.skip_ws();
        if self.eat(c) {
            Ok(())
        } else {
            Err(match self.peek() {
                Some(got) => format!("expected `{c}`, found `{got}`"),
                None => format!("expected `{c}` before the end"),
            })
        }
    }

    /// Whitespace plus `// …` and `/* … */` comments.
    fn skip_ws(&mut self) {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.pos += 1;
            }
            let next = self.s.get(self.pos + 1).copied();
            match (self.peek(), next) {
                (Some('/'), Some('/')) => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.pos += 1;
                    }
                }
                (Some('/'), Some('*')) => {
                    self.pos += 2;
                    while self.pos < self.s.len()
                        && !(self.s[self.pos] == '*' && self.s.get(self.pos + 1) == Some(&'/'))
                    {
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(self.s.len());
                }
                _ => return,
            }
        }
    }

    fn ident(&mut self) -> Option<String> {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
        {
            self.pos += 1;
        }
        (self.pos > start).then(|| self.s[start..self.pos].iter().collect())
    }

    fn statement(&mut self) -> Result<Statement, String> {
        if self.peek() == Some('{') {
            return match self.value()? {
                Bson::Document(doc) => Ok(Statement::Command { db: None, doc }),
                _ => unreachable!(),
            };
        }
        let start = self.pos;
        match self.ident().as_deref() {
            Some("use") => {
                self.skip_ws();
                let name = match self.peek() {
                    Some('"' | '\'') => self.string()?,
                    _ => {
                        let s = self.pos;
                        while self.peek().is_some_and(|c| !c.is_whitespace() && c != ';') {
                            self.pos += 1;
                        }
                        self.s[s..self.pos].iter().collect()
                    }
                };
                if name.is_empty() {
                    return Err("use: database name expected".into());
                }
                Ok(Statement::Use(name))
            }
            Some("db") => self.db_chain(),
            _ => {
                self.pos = start;
                Err("expected `db.<collection>.<method>(…)`, `use <db>` or a `{…}` command".into())
            }
        }
    }

    fn db_chain(&mut self) -> Result<Statement, String> {
        let mut db: Option<String> = None;
        let mut segments: Vec<String> = Vec::new();
        loop {
            self.skip_ws();
            if self.eat('[') {
                self.skip_ws();
                segments.push(self.string()?);
                self.expect(']')?;
                continue;
            }
            if !self.eat('.') {
                return Err("expected a method call like `.find()`".into());
            }
            self.skip_ws();
            let name = self.ident().ok_or("name expected after `.`")?;
            self.skip_ws();
            if self.peek() != Some('(') {
                segments.push(name);
                continue;
            }
            let args = self.args()?;
            match name.as_str() {
                "getSiblingDB" if segments.is_empty() => {
                    db = Some(string_arg(&args, "getSiblingDB")?);
                }
                "getCollection" if segments.is_empty() => {
                    segments.push(string_arg(&args, "getCollection")?);
                }
                "runCommand" | "adminCommand" if segments.is_empty() => {
                    let doc = match args.into_iter().next() {
                        Some(Bson::Document(d)) => d,
                        Some(Bson::String(cmd)) => bson::doc! { cmd: 1 },
                        _ => return Err(format!("{name} expects a command document")),
                    };
                    let db = if name == "adminCommand" { Some("admin".into()) } else { db };
                    return Ok(Statement::Command { db, doc });
                }
                _ if segments.is_empty() => {
                    return Ok(Statement::Db {
                        db,
                        method: name,
                        args,
                    });
                }
                _ => {
                    let mut modifiers = Vec::new();
                    loop {
                        self.skip_ws();
                        if self.peek() != Some('.') {
                            break;
                        }
                        self.pos += 1;
                        self.skip_ws();
                        let m = self.ident().ok_or("method name expected after `.`")?;
                        let a = self.args()?;
                        modifiers.push((m, a));
                    }
                    return Ok(Statement::Collection {
                        db,
                        coll: segments.join("."),
                        method: name,
                        args,
                        modifiers,
                    });
                }
            }
        }
    }

    /// `( value, value, … )`
    fn args(&mut self) -> Result<Vec<Bson>, String> {
        self.expect('(')?;
        let mut out = Vec::new();
        loop {
            self.skip_ws();
            if self.eat(')') {
                return Ok(out);
            }
            out.push(self.value()?);
            self.skip_ws();
            if !self.eat(',') {
                self.expect(')')?;
                return Ok(out);
            }
        }
    }

    fn value(&mut self) -> Result<Bson, String> {
        self.skip_ws();
        match self.peek() {
            None => Err("value expected".into()),
            Some('{') => {
                self.pos += 1;
                let mut doc = Document::new();
                loop {
                    self.skip_ws();
                    if self.eat('}') {
                        return Ok(ext_json(doc));
                    }
                    let key = match self.peek() {
                        Some('"' | '\'') => self.string()?,
                        _ => self.ident().ok_or_else(|| self.unexpected("a key"))?,
                    };
                    self.expect(':')?;
                    let v = self.value()?;
                    doc.insert(key, v);
                    self.skip_ws();
                    if !self.eat(',') {
                        self.expect('}')?;
                        return Ok(ext_json(doc));
                    }
                }
            }
            Some('[') => {
                self.pos += 1;
                let mut arr = Vec::new();
                loop {
                    self.skip_ws();
                    if self.eat(']') {
                        return Ok(Bson::Array(arr));
                    }
                    arr.push(self.value()?);
                    self.skip_ws();
                    if !self.eat(',') {
                        self.expect(']')?;
                        return Ok(Bson::Array(arr));
                    }
                }
            }
            Some('"' | '\'') => Ok(Bson::String(self.string()?)),
            Some('/') => self.regex(),
            Some(c) if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => self.number(),
            Some(_) => {
                let word = self.ident().ok_or_else(|| self.unexpected("a value"))?;
                match word.as_str() {
                    "true" => Ok(Bson::Boolean(true)),
                    "false" => Ok(Bson::Boolean(false)),
                    "null" | "undefined" => Ok(Bson::Null),
                    "new" => {
                        self.skip_ws();
                        let ctor = self.ident().ok_or("constructor expected after `new`")?;
                        self.constructor(&ctor)
                    }
                    ctor => self.constructor(ctor),
                }
            }
        }
    }

    fn unexpected(&self, what: &str) -> String {
        match self.peek() {
            Some(c) => format!("expected {what}, found `{c}`"),
            None => format!("expected {what} before the end"),
        }
    }

    fn constructor(&mut self, name: &str) -> Result<Bson, String> {
        self.skip_ws();
        if self.peek() != Some('(') {
            return Err(format!("unknown identifier `{name}`"));
        }
        let args = self.args()?;
        let first = args.first();
        match name {
            "ObjectId" => match first {
                None => Ok(Bson::ObjectId(ObjectId::new())),
                Some(Bson::String(s)) => ObjectId::parse_str(s)
                    .map(Bson::ObjectId)
                    .map_err(|_| format!("invalid ObjectId \"{s}\"")),
                _ => Err("ObjectId expects a hex string".into()),
            },
            "ISODate" | "Date" => match first {
                None => Ok(Bson::DateTime(bson::DateTime::now())),
                Some(Bson::String(s)) => parse_date(s).map(Bson::DateTime),
                Some(n) => as_i64(n)
                    .map(|ms| Bson::DateTime(bson::DateTime::from_millis(ms)))
                    .ok_or_else(|| "Date expects a string or milliseconds".into()),
            },
            "NumberLong" => first
                .and_then(as_i64)
                .map(Bson::Int64)
                .ok_or_else(|| "NumberLong expects an integer".into()),
            "NumberInt" => first
                .and_then(as_i64)
                .and_then(|v| i32::try_from(v).ok())
                .map(Bson::Int32)
                .ok_or_else(|| "NumberInt expects a 32-bit integer".into()),
            "NumberDecimal" => {
                let s = match first {
                    Some(Bson::String(s)) => s.clone(),
                    Some(Bson::Int32(v)) => v.to_string(),
                    Some(Bson::Int64(v)) => v.to_string(),
                    Some(Bson::Double(v)) => v.to_string(),
                    _ => return Err("NumberDecimal expects a number".into()),
                };
                Bson::try_from(serde_json::json!({ "$numberDecimal": s }))
                    .map_err(|_| format!("invalid decimal \"{s}\""))
            }
            "UUID" => match first {
                Some(Bson::String(s)) => uuid::Uuid::parse_str(s)
                    .map(|u| {
                        Bson::Binary(Binary {
                            subtype: BinarySubtype::Uuid,
                            bytes: u.as_bytes().to_vec(),
                        })
                    })
                    .map_err(|_| format!("invalid UUID \"{s}\"")),
                None => Ok(Bson::Binary(Binary {
                    subtype: BinarySubtype::Uuid,
                    bytes: uuid::Uuid::new_v4().as_bytes().to_vec(),
                })),
                _ => Err("UUID expects a string".into()),
            },
            other => Err(format!("unknown constructor `{other}`")),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let quote = self.peek().ok_or("string expected")?;
        self.pos += 1;
        let mut out = String::new();
        loop {
            let c = self.peek().ok_or("unterminated string")?;
            self.pos += 1;
            if c == quote {
                return Ok(out);
            }
            if c != '\\' {
                out.push(c);
                continue;
            }
            let e = self.peek().ok_or("unterminated string")?;
            self.pos += 1;
            out.push(match e {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                'b' => '\u{8}',
                'f' => '\u{c}',
                '0' => '\0',
                'u' => {
                    let hex: String = self.s.get(self.pos..self.pos + 4).ok_or("bad \\u escape")?.iter().collect();
                    self.pos += 4;
                    u32::from_str_radix(&hex, 16)
                        .ok()
                        .and_then(char::from_u32)
                        .ok_or("bad \\u escape")?
                }
                other => other,
            });
        }
    }

    fn number(&mut self) -> Result<Bson, String> {
        let start = self.pos;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'))
        {
            self.pos += 1;
        }
        let text: String = self.s[start..self.pos].iter().collect();
        let text = text.trim_start_matches('+');
        if let Ok(i) = text.parse::<i64>() {
            return Ok(i32::try_from(i).map(Bson::Int32).unwrap_or(Bson::Int64(i)));
        }
        text.parse::<f64>()
            .map(Bson::Double)
            .map_err(|_| format!("invalid number `{text}`"))
    }

    fn regex(&mut self) -> Result<Bson, String> {
        self.pos += 1;
        let mut pattern = String::new();
        loop {
            let c = self.peek().ok_or("unterminated regex")?;
            self.pos += 1;
            match c {
                '/' => break,
                '\\' => {
                    let next = self.peek().ok_or("unterminated regex")?;
                    self.pos += 1;
                    // `\/` only exists to keep the literal open: the
                    // pattern itself holds a bare `/`.
                    if next != '/' {
                        pattern.push(c);
                    }
                    pattern.push(next);
                }
                _ => pattern.push(c),
            }
        }
        let mut options: Vec<char> = Vec::new();
        while let Some(c) = self.peek().filter(char::is_ascii_alphabetic) {
            options.push(c);
            self.pos += 1;
        }
        options.sort_unstable();
        Ok(Bson::RegularExpression(bson::Regex {
            pattern,
            options: options.into_iter().collect(),
        }))
    }
}

fn as_i64(b: &Bson) -> Option<i64> {
    match b {
        Bson::Int32(v) => Some(*v as i64),
        Bson::Int64(v) => Some(*v),
        Bson::Double(v) if v.fract() == 0.0 => Some(*v as i64),
        Bson::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn string_arg(args: &[Bson], method: &str) -> Result<String, String> {
    match args.first() {
        Some(Bson::String(s)) => Ok(s.clone()),
        _ => Err(format!("{method} expects a name string")),
    }
}

/// ISO-8601 the forgiving way: a bare date gets midnight, a missing
/// zone means UTC.
pub fn parse_date(s: &str) -> Result<bson::DateTime, String> {
    let s = s.trim();
    let mut norm = s.replace(' ', "T");
    if norm.len() == 10 {
        norm.push_str("T00:00:00Z");
    } else if let Some((_, time)) = norm.split_once('T')
        && !time.ends_with('Z')
        && !time.contains('+')
        && !time.contains('-')
    {
        norm.push('Z');
    }
    bson::DateTime::parse_rfc3339_str(&norm).map_err(|_| format!("invalid date \"{s}\""))
}

/// A single-key Extended JSON wrapper (`{"$oid": …}`, `{"$date": …}`,
/// `{"$numberLong": …}`, …) becomes its BSON value — this is how the grid
/// shows nested values, so edits round-trip. Anything else stays a
/// document.
fn ext_json(doc: Document) -> Bson {
    const WRAPPERS: &[&str] = &[
        "$oid",
        "$date",
        "$numberLong",
        "$numberInt",
        "$numberDouble",
        "$numberDecimal",
        "$binary",
        "$uuid",
        "$timestamp",
        "$symbol",
        "$minKey",
        "$maxKey",
    ];
    let wrapper = doc.len() == 1 && doc.keys().next().is_some_and(|k| WRAPPERS.contains(&k.as_str()));
    if !wrapper {
        return Bson::Document(doc);
    }
    let json = Bson::Document(doc.clone()).into_relaxed_extjson();
    Bson::try_from(json).unwrap_or(Bson::Document(doc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn find_with_modifiers_keeps_key_order() {
        let s = parse(
            "db.orders.find({ status: 'paid', total: { $gt: 10.5 } }) // recent\n  .sort({ z: -1, a: 1 }).limit(5);",
        )
        .unwrap();
        let Statement::Collection { coll, method, args, modifiers, db } = s else { panic!() };
        assert_eq!((coll.as_str(), method.as_str(), db), ("orders", "find", None));
        assert_eq!(args[0], Bson::Document(doc! { "status": "paid", "total": { "$gt": 10.5 } }));
        assert_eq!(modifiers[0].0, "sort");
        let Bson::Document(sort) = &modifiers[0].1[0] else { panic!() };
        assert_eq!(sort.keys().collect::<Vec<_>>(), vec!["z", "a"]);
        assert_eq!(modifiers[1], ("limit".into(), vec![Bson::Int32(5)]));
    }

    #[test]
    fn sibling_db_dotted_collection_and_commands() {
        let s = parse(r#"db.getSiblingDB("logs").getCollection("a.b").countDocuments()"#).unwrap();
        assert!(matches!(s, Statement::Collection { ref db, ref coll, .. }
            if db.as_deref() == Some("logs") && coll == "a.b"));
        assert!(matches!(parse("db.system.users.find()").unwrap(),
            Statement::Collection { ref coll, .. } if coll == "system.users"));
        assert_eq!(
            parse("db.adminCommand({ listDatabases: 1 })").unwrap(),
            Statement::Command { db: Some("admin".into()), doc: doc! { "listDatabases": 1 } }
        );
        assert_eq!(parse("use shop;").unwrap(), Statement::Use("shop".into()));
        assert!(matches!(parse("db.getCollectionNames()").unwrap(), Statement::Db { .. }));
        assert!(parse("db.a.find() db.b.find()").is_err());
    }

    #[test]
    fn constructors_and_ext_json() {
        let v = parse_value(
            r#"{ _id: ObjectId("65a1b2c3d4e5f60718293a4b"), at: ISODate("2024-01-02"),
                n: NumberLong(7), big: 3000000000, re: /ab+c/mi, e: {"$oid": "65a1b2c3d4e5f60718293a4b"} }"#,
        )
        .unwrap();
        let Bson::Document(d) = v else { panic!() };
        assert!(matches!(d.get("_id"), Some(Bson::ObjectId(_))));
        assert_eq!(d.get("_id"), d.get("e"));
        assert_eq!(
            d.get_datetime("at").unwrap().try_to_rfc3339_string().unwrap(),
            "2024-01-02T00:00:00Z"
        );
        assert_eq!(d.get("n"), Some(&Bson::Int64(7)));
        assert_eq!(d.get("big"), Some(&Bson::Int64(3_000_000_000)));
        assert!(matches!(d.get("re"), Some(Bson::RegularExpression(r)) if r.options == "im"));
    }
}
