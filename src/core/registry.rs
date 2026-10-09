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

/// Open a session for a connection. Cheap: real pools connect lazily on
/// the first session call (the wizard's Test Connection is that call).
pub fn open(conn: &DbConnection) -> Result<SessionHandle, DbError> {
    let session: Arc<dyn DbSession> = if is_mock() {
        // Demo mode: the mock serves every type, regardless of features.
        Arc::new(MockSession::new(conn))
    } else {
        match conn.db_type {
            #[cfg(feature = "postgres")]
            DbType::PostgreSql => Arc::new(crate::drivers::postgres::PostgresSession::new(conn)),
            #[cfg(feature = "mysql")]
            DbType::MySql => Arc::new(crate::drivers::mysql::MySqlSession::new(conn)),
            #[cfg(feature = "sqlite")]
            DbType::Sqlite => Arc::new(crate::drivers::sqlite::SqliteSession::new(conn)),
            // MongoDB and Redis land in Stage 4; anything unhandled stays
            // on the deterministic mock.
            _ => Arc::new(MockSession::new(conn)),
        }
    };
    Ok(SessionHandle::new(session))
}
