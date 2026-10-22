//! libbp: a synchronous Bundle Protocol socket library.
//!
//! `BpaAdapter` abstracts the bundle protocol agent; `Dtn7Adapter` is the only
//! implementation. `capi` exposes it to C (see `include/bp.h`).

pub mod adapter;
pub mod capi;
pub mod dtn7;
pub mod eid;
pub mod error;

pub use adapter::BpaAdapter;
pub use dtn7::Dtn7Adapter;
pub use eid::Eid;
pub use error::BpError;
