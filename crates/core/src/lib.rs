//! Domain model and pure scheduling logic shared by the daemon, CLI, client and MCP crates.
//! This crate performs no IO.

pub mod api;
pub mod fair_share;
pub mod job;
pub mod resource;
pub mod scheduler;
pub mod task;
pub mod user;

pub use job::{Job, JobId, JobState};
pub use resource::{
    Inventory, ResourceOverrides, ResourceRequest, parse_bytes, parse_duration_secs,
};
pub use scheduler::{Assignment, Decision, NodeInventory, schedule};
pub use task::{Snapshot, SnapshotId, Task, TaskDetail, TaskId, TaskState, TaskSummary};
pub use user::{Quota, Role, User, UserId};
