//! The integration tests of `tuplebase-core`, in one test binary so they link once.
//! Those that need a database, SSH server or AWS are skipped unless `TUPLEBASE_IT=1`
//! (see docs/DEVELOPMENT.md).
//!
//! `aws_profile.rs` and `aws_role.rs` stay their own binaries: each sets AWS
//! variables for the whole process that the tests here would see.

// Db::run dispatches to each family's runner, one more level of nested futures than
// the compiler's default query depth allows here.
#![recursion_limit = "256"]

mod aws;
mod db;
mod dynamo;
mod mongo;
mod sqlite;
mod ssh;
