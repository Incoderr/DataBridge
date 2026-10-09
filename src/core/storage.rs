//! Stage 3 persistence: the recent-connections store.
//!
//! Connections live in `connections.json` inside the platform config dir.
//! Passwords never touch the disk: fields-mode passwords and the full
//! (credential-carrying) URLs of URL-mode connections go to the OS keyring
//! (service `DataBridge`, one entry per connection id). The JSON keeps a
//! password-stripped URL copy for display. Every operation is best-effort
//! — a missing config dir or an unavailable keyring degrades to "type the
//! password again", never a crash.

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use super::model::{ConnectionStatus, DbConnection, DbType};

/// File name inside the platform config dir.
const FILE: &str = "connections.json";
/// Keyring service name (shows up in the OS credential manager).
const SERVICE: &str = "DataBridge";
/// Keyring entry suffix for a URL-mode connection's full URL; the JSON
/// only stores the sanitized copy for display.
const URL_SUFFIX: &str = "::url";
/// How many recent connections to remember.
const MAX_RECENT: usize = 20;

/// The serializable mirror of [`DbConnection`]: everything but the
/// password (keyring-only) and the transient status/latency fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredConnection {
    id: String,
    name: String,
    db_type: DbType,
    host: String,
    port: u16,
    database: String,
    user: String,
    /// Password-stripped when it came from URL mode
    /// (see [`strip_url_password`]); the full URL lives in the keyring.
    url: Option<String>,
    /// Unix milliseconds of the last successful connect — recency order.
    last_used: u64,
}

/// Platform config dir (`…/DataBridge`): `%APPDATA%` on Windows,
/// `~/Library/Application Support` on macOS, `$XDG_CONFIG_HOME` or
/// `~/.config` elsewhere. `None` when the environment gives us nothing
/// (the app then runs memory-only, as before Stage 3).
pub fn config_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|base| base.join("DataBridge"))
}

fn connections_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(FILE))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------- keyring ----------

fn save_secret(id: &str, suffix: &str, value: &str) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, &format!("{id}{suffix}")) {
        if let Err(e) = entry.set_password(value) {
            eprintln!("DataBridge: keyring write failed for {id}{suffix}: {e}");
        }
    }
}

fn load_secret(id: &str, suffix: &str) -> Option<String> {
    keyring::Entry::new(SERVICE, &format!("{id}{suffix}"))
        .ok()?
        .get_password()
        .ok()
}

fn delete_secret(id: &str, suffix: &str) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, &format!("{id}{suffix}")) {
        if let Err(e) = entry.delete_credential() {
            // "not found" is the normal case when nothing was ever stored.
            eprintln!("DataBridge: keyring delete failed for {id}{suffix}: {e}");
        }
    }
}

// ---------- public API ----------

/// Load the remembered connections, most recently used first. Passwords
/// (and full URLs) are re-attached from the keyring; a missing keyring
/// entry just means the user types it again.
pub fn load_connections() -> Vec<DbConnection> {
    let Some(path) = connections_path() else {
        return Vec::new();
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(mut list) = serde_json::from_str::<Vec<StoredConnection>>(&text) else {
        return Vec::new();
    };
    list.sort_by(|a, b| b.last_used.cmp(&a.last_used));
    list.into_iter()
        .map(|c| {
            // URL mode: the keyring holds the full URL (credentials and
            // all); the sanitized copy is only the fallback for display.
            let url = match &c.url {
                Some(_) => load_secret(&c.id, URL_SUFFIX).or_else(|| c.url.clone()),
                None => None,
            };
            let password = load_secret(&c.id, "").unwrap_or_default();
            DbConnection {
                id: c.id,
                name: c.name,
                db_type: c.db_type,
                host: c.host,
                port: c.port,
                database: c.database,
                user: c.user,
                password,
                url,
                status: ConnectionStatus::Disconnected,
                latency_ms: 0,
            }
        })
        .collect()
}

/// Remember a connection that just connected: upsert by id, stamp
/// `last_used`, keep the list newest-first and capped at [`MAX_RECENT`].
/// The password / full URL go to the keyring only — never the JSON.
pub fn remember_connection(conn: &DbConnection) {
    let Some(path) = connections_path() else {
        return;
    };
    let mut list: Vec<StoredConnection> = read_list(&path);
    // This connection goes first; everyone else keeps its own timestamp.
    list.retain(|c| c.id != conn.id);
    list.insert(
        0,
        StoredConnection {
            id: conn.id.clone(),
            name: conn.name.clone(),
            db_type: conn.db_type,
            host: conn.host.clone(),
            port: conn.port,
            database: conn.database.clone(),
            user: conn.user.clone(),
            url: conn.url.as_ref().map(|u| strip_url_password(u)),
            last_used: now_ms(),
        },
    );
    list.truncate(MAX_RECENT);
    if let Err(e) = write_list(&path, &list) {
        eprintln!("DataBridge: cannot save connections: {e}");
    }
    // Keyring: fields mode → the password; URL mode → the full URL.
    if let Some(url) = &conn.url {
        save_secret(&conn.id, URL_SUFFIX, url);
    } else if !conn.password.is_empty() {
        save_secret(&conn.id, "", &conn.password);
    }
}

/// Drop a remembered connection: list entry + both keyring secrets.
pub fn forget_connection(id: &str) {
    delete_secret(id, "");
    delete_secret(id, URL_SUFFIX);
    let Some(path) = connections_path() else {
        return;
    };
    let mut list = read_list(&path);
    list.retain(|c| c.id != id);
    if let Err(e) = write_list(&path, &list) {
        eprintln!("DataBridge: cannot save connections: {e}");
    }
}

// ---------- helpers ----------

/// `postgres://u:s3cret@h/db?sslmode=…` → `postgres://u@h/db?sslmode=…` —
/// the display copy kept in the JSON; the original stays in the keyring.
/// URLs without a password round-trip unchanged.
pub fn strip_url_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    // The authority ends at the first path / query / fragment separator.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return url.to_string();
    };
    let user = userinfo.split_once(':').map(|(u, _)| u).unwrap_or(userinfo);
    format!("{scheme}://{user}@{host}{tail}")
}

fn read_list(path: &PathBuf) -> Vec<StoredConnection> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn write_list(path: &PathBuf, list: &[StoredConnection]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(list)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    // Write-then-rename so a crash never leaves a half-written file behind.
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_password_but_keeps_user_and_query() {
        assert_eq!(
            strip_url_password("postgres://alice:s3cret@host.io:5432/db?sslmode=require"),
            "postgres://alice@host.io:5432/db?sslmode=require"
        );
    }

    #[test]
    fn keeps_url_without_password_intact() {
        let url = "mysql://bob@localhost:3306/warehouse";
        assert_eq!(strip_url_password(url), url);
        // IPv6 literals keep their brackets and colons.
        let url = "postgres://u:pw@[::1]:5432/db";
        assert_eq!(strip_url_password(url), "postgres://u@[::1]:5432/db");
    }

    #[test]
    fn leaves_bare_and_relative_strings_alone() {
        assert_eq!(strip_url_password("not-a-url"), "not-a-url");
        assert_eq!(strip_url_password("sqlite:///data/db.sqlite"), "sqlite:///data/db.sqlite");
    }

    #[test]
    fn stored_connection_round_trips() {
        let c = StoredConnection {
            id: "pg-prod".into(),
            name: "production_postgres".into(),
            db_type: DbType::PostgreSql,
            host: "db.internal.acme.io".into(),
            port: 5432,
            database: "acme_core".into(),
            user: "app_reader".into(),
            url: None,
            last_used: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: StoredConnection = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, c.id);
        assert_eq!(back.db_type, DbType::PostgreSql);
        assert_eq!(back.last_used, c.last_used);
    }
}
