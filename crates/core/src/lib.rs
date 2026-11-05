//! Domain model and pure scheduling logic shared by the daemon, CLI, client and MCP crates.
//! This crate performs no IO.

pub mod api;
pub mod fair_share;
pub mod job;
pub mod resource;
pub mod scheduler;
pub mod user;

pub use job::{Job, JobId, JobState};
pub use resource::{Inventory, ResourceRequest, parse_bytes, parse_duration_secs};
pub use scheduler::{Assignment, Decision, schedule};
pub use user::{Quota, Role, User, UserId};
