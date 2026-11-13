//! Per-database implementations of the DbSession contract.
//!
//! The sqlx drivers are gated behind their cargo features (`postgres` /
//! `mysql` / `sqlite`); MongoDB is our own light client (only `bson`
//! beyond crates already in the tree) and always compiled in. Drivers are
//! reachable only through `core::registry` — the UI never imports them.
//! Stage 4 next: Redis.

#[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
mod util;

pub mod mongo;
#[cfg(feature = "mysql")]
pub mod mysql;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;
