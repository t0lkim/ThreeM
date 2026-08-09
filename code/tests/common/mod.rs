//! Fixture helpers for the integration suites.
//!
//! Everything here now lives in [`mmm::fixtures`], which ships as part of the
//! library so `mmm-fixtures` can generate the same synthetic media a user can
//! point the tool at. This file is the re-export that kept that move from
//! touching a hundred call sites: the suites `use common::{MediaTree, naive, …}`
//! exactly as before.
//!
//! Nothing test-specific remains. If something is needed only by tests and
//! never by the generator, it belongs here rather than in the library — the
//! boundary is whether a *user* generating a library would want it.

#![allow(
    unused_imports,
    reason = "a re-export surface; each suite pulls only the part it needs"
)]

pub use mmm::fixtures::*;
