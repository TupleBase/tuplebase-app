//! TupleBase core: database logic with no UI dependencies.

pub mod config;
pub mod db;
pub mod safety;
pub mod secrets;
pub mod updates;

/// Product name shown by every front end.
pub const PRODUCT_NAME: &str = "TupleBase";
