//! The async driver contract every database must implement, plus the
//! cloneable handle the UI uses to talk to a session through the tokio
//! sidecar.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{oneshot, watch};

use crate::core::error::DbError;
use crate::core::model::{
    CellChange, Page, Ping, QueryPage, Relation, RowQuery, SchemaMeta, TableRef,
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
    /// columns. `query` narrows (AND-ed filters, values bound as
    /// parameters) and orders the rows; the total counts filtered rows.
    async fn fetch_rows(
        &self,
        table: TableRef,
        query: RowQuery,
        offset: u64,
        limit: u64,
    ) -> Result<Page, DbError>;

    /// Stage 5: run arbitrary SQL. Result sets come back as columns+rows
    /// (capped by `limit`); mutations report `rows_affected`. `cancel`
    /// stops the statement early (see [`cancellable`]): drivers that can
    /// ask the server to stop it do so, the rest abandon the call.
    async fn execute(
        &self,
        sql: &str,
        limit: u64,
        cancel: &CancelToken,
    ) -> Result<QueryPage, DbError> {
        let _ = (sql, limit, cancel);
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

    /// Document editor (document stores): replace the document identified
    /// by `pk` with `text` (mongosh syntax, as [`format_document`] writes
    /// it), or insert it when `pk` is empty.
    ///
    /// [`format_document`]: crate::core::model::format_document
    async fn save_document(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
        text: String,
    ) -> Result<u64, DbError> {
        let _ = (table, pk, text);
        Err(DbError::new("this driver does not store documents"))
    }

    /// Tree context menu: drop a table or view (`table.kind` picks the
    /// statement, see `NodeKind::drop_keyword`). `table` comes from
    /// introspection; drivers quote it per dialect.
    async fn drop_table(&self, table: TableRef) -> Result<(), DbError> {
        let _ = table;
        Err(DbError::new("this driver does not support dropping tables"))
    }
}

/// Cooperative cancellation of one running statement. Cloneable: the UI
/// keeps one copy (Stop button), the session future awaits another.
#[derive(Clone)]
pub struct CancelToken(Arc<watch::Sender<bool>>);

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    /// Request cancellation (idempotent; safe from any thread).
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }

    /// Resolves once [`cancel`](Self::cancel) was called — immediately if
    /// it already was. Sidecar side only (a watch receiver is
    /// runtime-agnostic, but callers race it against tokio work).
    pub async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        // The sender lives as long as `self`, so `wait_for` cannot fail.
        let _ = rx.wait_for(|c| *c).await;
    }
}

/// How long a cancelled statement may take to wind down after the server
/// was asked to stop it (so its connection returns to the pool in a clean
/// state) before the call is abandoned outright.
pub const CANCEL_GRACE: Duration = Duration::from_secs(3);

/// The error a cancelled statement reports.
pub fn cancelled_error() -> DbError {
    DbError::new("query cancelled")
}

/// Run `work` until it finishes or `cancel` fires. On cancel, `kill` asks
/// the server to stop the statement (`pg_cancel_backend`, `KILL QUERY`;
/// `None` when the driver cannot), then `work` gets [`CANCEL_GRACE`] to
/// fail on its own before it is dropped. Either way the caller gets
/// [`cancelled_error`]. Runs on the sidecar (uses tokio timers).
pub async fn cancellable<T, W, K>(
    work: W,
    cancel: &CancelToken,
    kill: Option<K>,
) -> Result<T, DbError>
where
    W: Future<Output = Result<T, DbError>>,
    K: Future<Output = ()>,
{
    tokio::pin!(work);
    tokio::select! {
        biased;
        res = &mut work => return res,
        _ = cancel.cancelled() => {}
    }
    if let Some(kill) = kill {
        kill.await;
        // The statement normally fails at once ("canceling statement due
        // to user request"); a result that won the race is still dropped —
        // the user asked to stop.
        let _ = tokio::time::timeout(CANCEL_GRACE, &mut work).await;
    }
    Err(cancelled_error())
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
        query: RowQuery,
        offset: u64,
        limit: u64,
    ) -> oneshot::Receiver<Result<Page, DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.fetch_rows(table, query, offset, limit).await })
    }

    /// Stage 5: run arbitrary SQL (the query editor, AI chat). Keep a
    /// clone of `cancel` to stop the statement.
    pub fn execute(
        &self,
        sql: String,
        limit: u64,
        cancel: CancelToken,
    ) -> oneshot::Receiver<Result<QueryPage, DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.execute(&sql, limit, &cancel).await })
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

    pub fn save_document(
        &self,
        table: TableRef,
        pk: Vec<CellChange>,
        text: String,
    ) -> oneshot::Receiver<Result<u64, DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.save_document(table, pk, text).await })
    }

    pub fn drop_table(&self, table: TableRef) -> oneshot::Receiver<Result<(), DbError>> {
        let session = self.0.clone();
        crate::core::runtime::spawn(async move { session.drop_table(table).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn finished_work_is_returned() {
        let cancel = CancelToken::new();
        let res = rt().block_on(cancellable(
            async { Ok::<_, DbError>(7) },
            &cancel,
            None::<std::future::Ready<()>>,
        ));
        assert_eq!(res.unwrap(), 7);
    }

    #[test]
    fn cancel_without_kill_abandons_at_once() {
        let cancel = CancelToken::new();
        let rt = rt();
        let res = rt.block_on(async {
            let c = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                c.cancel();
            });
            let t0 = std::time::Instant::now();
            let res = cancellable(
                async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    Ok::<_, DbError>(())
                },
                &cancel,
                None::<std::future::Ready<()>>,
            )
            .await;
            assert!(t0.elapsed() < Duration::from_secs(2));
            res
        });
        assert_eq!(res.unwrap_err().message(), "query cancelled");
    }

    #[test]
    fn kill_runs_and_work_gets_to_wind_down() {
        let cancel = CancelToken::new();
        cancel.cancel(); // already cancelled before the call
        let killed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let k = killed.clone();
        let res = rt().block_on(cancellable(
            async move {
                // Ends as soon as the "server" got the kill.
                let _ = rx.await;
                Err::<(), _>(DbError::new("canceling statement due to user request"))
            },
            &cancel,
            Some(async move {
                k.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = tx.send(());
            }),
        ));
        assert!(killed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(res.unwrap_err().message(), "query cancelled");
    }
}
