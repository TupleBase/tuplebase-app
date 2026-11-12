//! TupleBase core: database logic with no UI dependencies.

pub mod agents;
pub mod aws;
pub mod chat;
pub mod config;
pub mod db;
pub mod mcp_status;
pub mod safety;
pub mod secrets;
pub mod ssh;
pub mod state;
pub mod updates;

/// Product name shown by every front end.
pub const PRODUCT_NAME: &str = "TupleBase";
