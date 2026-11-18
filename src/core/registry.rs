//! DbType → session factory. The only place that knows which driver backs
//! which database type; the UI never imports drivers directly.

use std::sync::{Arc, OnceLock};

use crate::core::error::DbError;
use crate::core::mock::MockSession;
use crate::core::model::{DbConnection, DbType};
use crate::core::session::{DbSession, SessionHandle};

/// Process-wide demo mode (`--mock`): every connection — seeded or newly
/// typed — is served by the deterministic mock driver, so the app runs
/// without a live database. Set once from `main`, before the UI exists.
static MOCK_MODE: OnceLock<bool> = OnceLock::new();

/// Set the demo mode from the CLI (`--mock`). Called once from `main`.
pub fn init(mock: bool) {
    let _ = MOCK_MODE.set(mock);
}

/// Whether `--mock` was passed (demo mode).
pub fn is_mock() -> bool {
    MOCK_MODE.get().copied().unwrap_or(false)
}

/// Whether `open` can serve this type: always in `--mock`, otherwise only
/// when its driver is compiled in. The wizard dims the rest.
pub fn is_supported(t: DbType) -> bool {
    if is_mock() {
        return true;
    }
    match t {
        DbType::PostgreSql => cfg!(feature = "postgres"),
        DbType::MySql | DbType::MariaDb => cfg!(feature = "mysql"),
        DbType::Sqlite => cfg!(feature = "sqlite"),
        DbType::MongoDb | DbType::ClickHouse => true,
        DbType::Redis
        | DbType::Cassandra
        | DbType::ScyllaDb => false,
    }
}

/// Open a session for a connection. Cheap: real pools connect lazily on
/// the first session call (the wizard's Test Connection is that call).
/// A connection with an SSH tunnel gets a `TunneledSession` that opens the
/// tunnel first and points the driver at its local end (SQLite files and
/// `--mock` ignore the tunnel).
pub fn open(conn: &DbConnection) -> Result<SessionHandle, DbError> {
    if is_mock() {
        // Demo mode: the mock serves every type, regardless of features.
        return Ok(SessionHandle::new(Arc::new(MockSession::new(conn))));
    }
    let session = match &conn.ssh {
        Some(ssh) if conn.db_type != DbType::Sqlite && is_supported(conn.db_type) => {
            Arc::new(crate::drivers::ssh::TunneledSession::new(conn, ssh.clone(), driver)?)
        }
        _ => driver(conn)?,
    };
    Ok(SessionHandle::new(session))
}

/// The real driver for a connection (no tunnel, no mock).
fn driver(conn: &DbConnection) -> Result<Arc<dyn DbSession>, DbError> {
    Ok(match conn.db_type {
        #[cfg(feature = "postgres")]
        DbType::PostgreSql => Arc::new(crate::drivers::postgres::PostgresSession::new(conn)),
        // MariaDB speaks the MySQL wire protocol (and sqlx accepts
        // `mariadb://` URLs), so the MySQL driver serves both.
        #[cfg(feature = "mysql")]
        DbType::MySql | DbType::MariaDb => Arc::new(crate::drivers::mysql::MySqlSession::new(conn)),
        #[cfg(feature = "sqlite")]
        DbType::Sqlite => Arc::new(crate::drivers::sqlite::SqliteSession::new(conn)),
        DbType::MongoDb => Arc::new(crate::drivers::mongo::MongoSession::new(conn)),
        DbType::ClickHouse => Arc::new(crate::drivers::clickhouse::ClickHouseSession::new(conn)),
        // No driver for this type (Stage 4 types, or a build without
        // the feature): fail loudly. Falling back to the mock would show
        // fake demo tables as if they were the user's database.
        t => {
            return Err(DbError::new(format!(
                "{} is not supported in this build yet — run with --mock for a demo.",
                t.label()
            )));
        }
    })
}
