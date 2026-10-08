//! The tokio sidecar. DB drivers (sqlx now, mongodb/redis later) need tokio;
//! GPUI runs its own executor. The bridge:
//!
//! 1. `init` starts a multi-thread tokio runtime on a dedicated thread and
//!    parks it forever; the process-wide `Handle` is stored in a static.
//! 2. DB work is spawned onto that runtime via `spawn`, which returns a
//!    `tokio::sync::oneshot::Receiver`.
//! 3. GPUI tasks (`cx.spawn`) await those receivers — oneshot/mpsc receivers
//!    are runtime-agnostic, so this is safe outside tokio.
//!
//! Tokio *timers* (`sleep`, `timeout`, `interval`) are runtime-bound: use
//! them on the sidecar side only (inside session futures), never inside
//! GPUI tasks.

use std::future::Future;
use std::sync::OnceLock;

use tokio::sync::oneshot;

static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Start the DB runtime on a dedicated thread. Must be called once, before
/// any session call.
pub fn init() {
    if HANDLE.get().is_some() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel::<tokio::runtime::Handle>();
    std::thread::Builder::new()
        .name("databridge-db".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .thread_name("databridge-db-worker")
                .enable_all()
                .build()
                .expect("failed to build the tokio runtime");
            tx.send(rt.handle().clone()).expect("main thread went away");
            // Park forever, keeping the runtime alive for the whole process.
            rt.block_on(std::future::pending::<()>());
        })
        .expect("failed to spawn the db runtime thread");
    let handle = rx.recv().expect("db runtime thread died during startup");
    HANDLE
        .set(handle)
        .expect("db runtime initialized twice");
}

pub fn handle() -> &'static tokio::runtime::Handle {
    HANDLE.get().expect("runtime::init was not called")
}

/// Spawn `fut` on the sidecar runtime. The returned receiver is safe to
/// await inside GPUI tasks; a dropped receiver (tab closed mid-flight) is
/// fine — the send result is ignored.
pub fn spawn<T, F>(fut: F) -> oneshot::Receiver<T>
where
    T: Send + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    handle().spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx
}
