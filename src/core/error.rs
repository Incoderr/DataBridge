//! Driver-agnostic error: a humanized, user-facing message plus an optional
//! underlying cause kept for logs and diagnostics.

use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct DbError {
    message: String,
    source: Option<Arc<dyn StdError + Send + Sync + 'static>>,
}

impl DbError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    /// Attach the underlying driver error (sqlx/mongodb/…); the message
    /// itself must already be presentable to the user. Unused until the
    /// real drivers land (Stage 2).
    #[allow(dead_code)]
    pub fn with_source(
        message: impl Into<String>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Arc::new(source)),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for DbError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_ref()
            .map(|s| s.as_ref() as &(dyn StdError + 'static))
    }
}
