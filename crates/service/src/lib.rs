//! September's HTTP service and atomic storage operations.
//!
//! Storage is volatile in memory or durable in SQLite, shared within one process.
//! Summary jobs are completed by external workers or an opt-in model worker.

pub mod archive;
mod error;
mod http;
pub mod jobs;
mod mcp;
mod server;
pub mod snapshots;
pub mod storage;
pub mod summarizer;
pub mod worker;

pub use error::Error;
pub use http::router;
pub use server::serve;
