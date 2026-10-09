//! The async driver contract every database must implement, plus the
//! cloneable handle the UI uses to talk to a session through the tokio
//! sidecar.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::core::error::DbError;
use crate::core::model::{
    CellChange, Page, Ping, QueryPage, Relation, SchemaMeta, TableRef,
};

#[async_trait]
pub trait DbSession: Send + Sync {
    /// Liveness probe; refreshes the connection's status and latency.
    /// Unused by the UI until the wizard's Test Connection wires to it
    /// (Stage 2).
    #[allow(dead_code)]
    async fn ping(&self) -> Result<Ping, DbError>;

    /// The schema tree of the connection: schemas with their tables, views
    /// and routines. Stage 2 may split this into lazy per-schema loading
    /// (`list_tables`) once real databases make eager trees expensive.
    async fn list_schemas(&self) -> Result<Vec<SchemaMeta>, DbError>;

    /// Foreign-key edges of one schema — feeds the relationships diagram.
    async fn list_relations(&self, schema: &str) -> Result<Vec<Relation>, DbError>;

    /// One page of rows of a table/view/routine, ordered by the table's
    /// columns.
    async fn fetch_rows(&self, table: TableRef, offset: u64, limit: u64) -> Result<Page, DbError>;

    /// Stage 5: run arbitrary SQL. Result sets come back as columns+rows
    /// (capped by `limit`); mutations report `rows_affected`.
    async fn execute(&self, sql: &str, limit: u64) -> Result<QueryPage, DbError> {
        let _ = (sql, limit);
        Err(DbError::new("this driver does not support arbitrary SQL"))
    }

    /// Simplified editor: update one row identified by its primary key
    /// cells. `changes` carries only the columns that differ.
    async fn update_row(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
        changes: Vec<CellChange>,
    ) -> Result<u64, DbError> {
        let _ = (table, pk, changes);
        Err(DbError::new("this driver does not support row editing"))
    }

    /// Simplified editor: insert one row (`values` may cover a subset of
    /// the table's columns).
    async fn insert_row(&self, table: TableRef, values: Vec<CellChange>)
        -> Result<u64, DbError> {
        let _ = (table, values);
        Err(DbError::new("this driver does not support row insertion"))
    }

    /// Simplified editor: delete one row identified by its primary key.
    async fn delete_row(&self, table: TableRef, pk: Vec<CellChange>) -> Result<u64, DbError> {
        let _ = (table, pk);
        Err(DbError::new("this driver does not support row deletion"))
    }
}

/// Cloneable UI-side handle. Every call is spawned onto the sidecar runtime
/// and returns a runtime-agnostic receiver for a GPUI task to await. The
/// caller owns the stale-guard (`load_id`) — see `RecordsDelegate::load`.
#[derive(Clone)]
pub struct SessionHandle(Arc<dyn DbSession>);

impl SessionHandle {
    pub fn new(session: Arc<dyn DbSession>) -> Self {
        Self(session)
    }

    /// Stage 2: the wizard's Test Connection button awaits this.
    #[allow(dead_code)]
    pub fn ping(&self) -> oneshot::Receiver<Result<Ping, DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.ping().await })
    }

    pub fn list_schemas(&self) -> oneshot::Receiver<Result<Vec<SchemaMeta>, DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.list_schemas().await })
    }

    pub fn list_relations(
        &self,
        schema: &str,
    ) -> oneshot::Receiver<Result<Vec<Relation>, DbError>> {
        let session = self.0.clone();
        let schema = schema.to_string();
        crate::core::runtime::spawn(async move { session.list_relations(&schema).await })
    }

    pub fn fetch_rows(
        &self,
        table: TableRef,
        offset: u64,
        limit: u64,
    ) -> oneshot::Receiver<Result<Page, DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.fetch_rows(table, offset, limit).await })
    }

    /// Stage 5: run arbitrary SQL (the query editor).
    pub fn execute(
        &self,
        sql: String,
        limit: u64,
    ) -> oneshot::Receiver<Result<QueryPage, DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.execute(&sql, limit).await })
    }

    pub fn update_row(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
        changes: Vec<CellChange>,
    ) -> oneshot::Receiver<Result<u64, DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.update_row(table, pk, changes).await })
    }

    pub fn insert_row(
        &self,
        table: TableRef,
        values: Vec<CellChange>,
    ) -> oneshot::Receiver<Result<u64, DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.insert_row(table, values).await })
    }

    pub fn delete_row(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
    ) -> oneshot::Receiver<Result<u64, DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.delete_row(table, pk).await })
    }
}
