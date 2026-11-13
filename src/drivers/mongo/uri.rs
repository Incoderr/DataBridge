//! `mongodb://` / `mongodb+srv://` connection strings, and the same target
//! built from the wizard's structured fields. An SRV string is parsed
//! here and expanded by [`resolve`] at connect time (seed-list discovery
//! spec: `_mongodb._tcp.<host>` SRV → hosts, one TXT record → default
//! `authSource`/`replicaSet`, TLS on unless `tls=false`).

use crate::core::error::DbError;
use crate::core::model::DbConnection;

use super::scram::Mechanism;
use super::wire::Tls;

#[derive(Debug, Clone, Default)]
pub struct Target {
    pub hosts: Vec<(String, u16)>,
    pub user: Option<String>,
    pub password: String,
    /// Path database: the default for `use`, the fallback tree when the
    /// user may not run `listDatabases`, and the default `authSource`.
    pub database: Option<String>,
    pub auth_source: Option<String>,
    pub mechanism: Option<Mechanism>,
    pub tls: Tls,
    /// `directConnection=true`: talk to the given host even if it is a
    /// secondary.
    pub direct: bool,
    /// `mongodb+srv://` host still to expand via DNS (`hosts` is empty).
    pub srv: Option<String>,
}

impl Target {
    pub fn from_connection(conn: &DbConnection) -> Result<Self, DbError> {
        if let Some(url) = conn.url.as_deref().filter(|u| !u.trim().is_empty()) {
            return parse(url.trim());
        }
        let host = if conn.host.is_empty() { "localhost" } else { &conn.host };
        let port = if conn.port == 0 { 27017 } else { conn.port };
        Ok(Self {
            hosts: vec![(host.to_string(), port)],
            user: Some(conn.user.clone()).filter(|u| !u.is_empty()),
            password: conn.password.clone(),
            database: Some(conn.database.clone()).filter(|d| !d.is_empty()),
            ..Self::default()
        })
    }

    /// The database credentials are checked against.
    pub fn auth_db(&self) -> &str {
        self.auth_source
            .as_deref()
            .or(self.database.as_deref())
            .unwrap_or("admin")
    }
}

pub fn parse(url: &str) -> Result<Target, DbError> {
    let (rest, srv) = if let Some(r) = url.strip_prefix("mongodb://") {
        (r, false)
    } else if let Some(r) = url.strip_prefix("mongodb+srv://") {
        (r, true)
    } else {
        return Err(DbError::new(
            "a MongoDB URL must start with mongodb:// or mongodb+srv://",
        ));
    };
    let (authority, tail) = match rest.find(['/', '?']) {
        Some(ix) => (&rest[..ix], &rest[ix..]),
        None => (rest, ""),
    };
    let mut t = Target::default();
    let hosts = match authority.rsplit_once('@') {
        Some((creds, hosts)) => {
            let (u, p) = creds.split_once(':').unwrap_or((creds, ""));
            t.user = Some(decode(u)?).filter(|u| !u.is_empty());
            t.password = decode(p)?;
            hosts
        }
        None => authority,
    };
    for h in hosts.split(',').filter(|h| !h.is_empty()) {
        t.hosts.push(host_port(h)?);
    }
    if t.hosts.is_empty() {
        return Err(DbError::new("the MongoDB URL has no host"));
    }
    if srv {
        let host = match (t.hosts.as_slice(), hosts.contains(':')) {
            ([(h, _)], false) => h.to_ascii_lowercase(),
            _ => {
                return Err(DbError::new(
                    "mongodb+srv:// takes exactly one host name and no port",
                ));
            }
        };
        if host.split('.').count() < 3 {
            return Err(DbError::new(format!(
                "mongodb+srv:// needs a host like cluster0.xxxx.mongodb.net, got {host}"
            )));
        }
        t.hosts.clear();
        t.srv = Some(host);
        t.tls.enabled = true; // spec default; an explicit tls=false still wins
    }
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let db = decode(path.trim_start_matches('/'))?;
    t.database = Some(db).filter(|d| !d.is_empty());

    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = decode(v)?;
        let on = matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
        match k.to_ascii_lowercase().as_str() {
            "authsource" => t.auth_source = Some(v).filter(|s| !s.is_empty()),
            "authmechanism" => {
                t.mechanism = Some(match v.to_ascii_uppercase().as_str() {
                    "SCRAM-SHA-256" => Mechanism::Sha256,
                    "SCRAM-SHA-1" => Mechanism::Sha1,
                    other => {
                        return Err(DbError::new(format!(
                            "authMechanism {other} is not supported (SCRAM-SHA-256 / SCRAM-SHA-1 only)"
                        )));
                    }
                })
            }
            "tls" | "ssl" => t.tls.enabled = on,
            "tlsallowinvalidcertificates" | "tlsallowinvalidhostnames" | "tlsinsecure" => {
                t.tls.insecure |= on
            }
            "directconnection" => t.direct = on,
            // replicaSet, appName, timeouts, readPreference, …: harmless to ignore.
            _ => {}
        }
    }
    Ok(t)
}

/// Expand an SRV target into its seed list (no-op for `mongodb://`).
/// Every SRV host must live under the SRV name's parent domain — the
/// spec's guard against a DNS answer redirecting credentials elsewhere.
pub async fn resolve(t: &Target) -> Result<Target, DbError> {
    let Some(host) = &t.srv else { return Ok(t.clone()) };
    let parent = host.split_once('.').map(|(_, p)| p).unwrap_or(host);
    let hosts = super::dns::lookup_srv(&format!("_mongodb._tcp.{host}")).await?;
    let mut out = t.clone();
    out.srv = None;
    for (h, port) in hosts {
        if !h.ends_with(&format!(".{parent}")) {
            return Err(DbError::new(format!(
                "DNS SRV host {h} is outside {parent} — refusing to connect"
            )));
        }
        out.hosts.push((h, port));
    }
    let txt = super::dns::lookup_txt(host).await?;
    if txt.len() > 1 {
        return Err(DbError::new(format!("{host} has more than one TXT record")));
    }
    if let Some(opts) = txt.first() {
        apply_txt(&mut out, opts)?;
    }
    Ok(out)
}

/// TXT options (`authSource=admin&replicaSet=…`): only `authSource` and
/// `replicaSet` / `loadBalanced` are allowed; options already in the URL win.
fn apply_txt(t: &mut Target, opts: &str) -> Result<(), DbError> {
    for pair in opts.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k.to_ascii_lowercase().as_str() {
            "authsource" => {
                if t.auth_source.is_none() {
                    t.auth_source = Some(decode(v)?).filter(|s| !s.is_empty());
                }
            }
            "replicaset" | "loadbalanced" => {}
            other => {
                return Err(DbError::new(format!("TXT record option {other} is not allowed")));
            }
        }
    }
    Ok(())
}

fn host_port(h: &str) -> Result<(String, u16), DbError> {
    let bad = || DbError::new(format!("invalid host in MongoDB URL: {h}"));
    if let Some(close) = h.find(']') {
        let (host, tail) = h.split_at(close + 1);
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| bad())?,
            None if tail.is_empty() => 27017,
            None => return Err(bad()),
        };
        return Ok((host.to_string(), port));
    }
    match h.rsplit_once(':') {
        Some((host, p)) => Ok((host.to_string(), p.parse().map_err(|_| bad())?)),
        None => Ok((h.to_string(), 27017)),
    }
}

/// Percent-decoding (user, password, database).
fn decode(s: &str) -> Result<String, DbError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| DbError::new("invalid percent-encoding in MongoDB URL"))?;
            out.push(hex);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| DbError::new("MongoDB URL is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seed_list_credentials_and_options() {
        let t = parse(
            "mongodb://ad%40min:p%3Ass@h1:27018,h2,[::1]:27019/shop?authSource=admin&tls=true&authMechanism=SCRAM-SHA-1",
        )
        .unwrap();
        assert_eq!(
            t.hosts,
            vec![("h1".into(), 27018), ("h2".into(), 27017), ("[::1]".into(), 27019)]
        );
        assert_eq!(t.user.as_deref(), Some("ad@min"));
        assert_eq!(t.password, "p:ss");
        assert_eq!(t.database.as_deref(), Some("shop"));
        assert_eq!(t.auth_db(), "admin");
        assert!(t.tls.enabled);
        assert_eq!(t.mechanism, Some(Mechanism::Sha1));
    }

    #[test]
    fn auth_db_defaults() {
        assert_eq!(parse("mongodb://localhost").unwrap().auth_db(), "admin");
        assert_eq!(parse("mongodb://u:p@localhost/app").unwrap().auth_db(), "app");
    }

    #[test]
    fn srv_strings() {
        let t = parse("mongodb+srv://u:p@Cluster0.ab12c.mongodb.net/shop?retryWrites=true&w=majority")
            .unwrap();
        assert_eq!(t.srv.as_deref(), Some("cluster0.ab12c.mongodb.net"));
        assert!(t.hosts.is_empty() && t.tls.enabled);
        assert!(!parse("mongodb+srv://c.ab.mongodb.net/?tls=false").unwrap().tls.enabled);
        assert!(parse("mongodb+srv://c.ab.mongodb.net:27017").is_err());
        assert!(parse("mongodb+srv://a.net,b.net").is_err());
        assert!(parse("mongodb+srv://localhost").is_err());

        let mut t = t;
        apply_txt(&mut t, "authSource=admin&replicaSet=atlas-x").unwrap();
        assert_eq!(t.auth_db(), "admin");
        assert!(apply_txt(&mut t, "tls=false").is_err());
    }
}
