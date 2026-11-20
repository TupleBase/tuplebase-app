//! Cloud providers: one module per provider, each the only code that talks to it (L6).
//!
//! [`aws`] covers Amazon Web Services: credentials kept in Secrets Manager and Parameter
//! Store, RDS IAM auth tokens and the profiles on this machine. A new provider is a new
//! module here.

pub mod aws;
