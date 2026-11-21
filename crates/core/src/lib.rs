//! TupleBase core: database logic with no UI dependencies.

pub mod agents;
pub mod app_status;
pub mod chat;
pub mod cloud;
pub mod config;
pub mod credentials;
pub mod db;
pub mod mcp_status;
pub mod mongo_shell;
pub mod safety;
pub mod secrets;
pub mod ssh;
pub mod state;
pub mod updates;

/// Product name shown by every front end.
pub const PRODUCT_NAME: &str = "TupleBase";
