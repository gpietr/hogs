//! Core data model of hogs: the scanned file tree plus helpers shared by the
//! scanners, the CLI and the TUI.

pub mod build;
pub mod fmt;
pub mod progress;
pub mod protect;
pub mod query;
pub mod snapshot;
pub mod staging;
pub mod tree;
pub mod user;

pub use progress::Progress;
pub use tree::{NodeId, Tree, flags};
