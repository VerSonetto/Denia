//! Shared domain types for the denia harness.
//!
//! Every cross-crate boundary speaks these vocabulary types: the streaming
//! protocol ([`stream`]), the failure taxonomy ([`error`]), model invocation
//! and selection records ([`config`]), and the chat vocabulary ([`message`).

pub mod config;
pub mod error;
pub mod message;
pub mod session;
pub mod stream;
pub mod tool;
