//! libbp: a synchronous Bundle Protocol socket library.

pub mod adapter;
pub mod eid;
pub mod error;

pub use adapter::BpaAdapter;
pub use eid::Eid;
pub use error::BpError;
