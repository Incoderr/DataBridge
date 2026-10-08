//! Per-database implementations of the DbSession contract.
//!
//! Each driver is gated behind its cargo feature (`postgres` / `mysql` /
//! `sqlite`) and reachable only through `core::registry` — the UI never
//! imports drivers directly. Stage 4 adds MongoDB and Redis.

#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
mod util;

#[cfg(feature = "mysql")]
pub mod mysql;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;
