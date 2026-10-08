//! DbType → session factory. The only place that knows which driver backs
//! which database type; the UI never imports drivers directly.

use std::sync::Arc;

use crate::core::error::DbError;
use crate::core::mock::MockSession;
use crate::core::model::{DbConnection, DbType};
use crate::core::session::{DbSession, SessionHandle};

/// Open a session for a connection. Cheap: real pools connect lazily on
/// the first session call (the wizard's Test Connection is that call).
pub fn open(conn: &DbConnection) -> Result<SessionHandle, DbError> {
    let session: Arc<dyn DbSession> = match conn.db_type {
        #[cfg(feature = "postgres")]
        DbType::PostgreSql => Arc::new(crate::drivers::postgres::PostgresSession::new(conn)),
        #[cfg(feature = "mysql")]
        DbType::MySql => Arc::new(crate::drivers::mysql::MySqlSession::new(conn)),
        #[cfg(feature = "sqlite")]
        DbType::Sqlite => Arc::new(crate::drivers::sqlite::SqliteSession::new(conn)),
        // MongoDB and Redis land in Stage 4; anything unhandled stays on
        // the deterministic mock.
        _ => Arc::new(MockSession::new(conn)),
    };
    Ok(SessionHandle::new(session))
}
