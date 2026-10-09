//! Driver-agnostic core: the domain model, the async driver contract, the
//! tokio sidecar, the driver registry, the deterministic mock session, and
//! the Stage-3 recent-connections store.

pub mod error;
pub mod mock;
pub mod model;
pub mod registry;
pub mod runtime;
pub mod session;
pub mod storage;
