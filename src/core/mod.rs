//! Driver-agnostic core: the domain model, the async driver contract, the
//! tokio sidecar, the driver registry, and the deterministic mock session.

pub mod error;
pub mod mock;
pub mod model;
pub mod registry;
pub mod runtime;
pub mod session;
