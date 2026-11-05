use serde::{Deserialize, Serialize};

pub type UserId = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    User,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "admin" => Some(Role::Admin),
            "user" => Some(Role::User),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct User {
    pub id: UserId,
    pub name: String,
    pub role: Role,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Per-user limits. `None` means unlimited.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Quota {
    pub max_running_jobs: Option<u32>,
    pub max_running_gpus: Option<u32>,
    pub max_queued_jobs: Option<u32>,
}
