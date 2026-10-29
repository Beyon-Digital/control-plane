use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Paths and process identity belong to the selected runtime, never the client OS.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionTarget {
    #[default]
    Native,
    Wsl {
        distribution: String,
    },
}

impl<'de> Deserialize<'de> for ExecutionTarget {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            kind: String,
            distribution: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        match (wire.kind.as_str(), wire.distribution) {
            ("native", None) => Ok(Self::Native),
            ("wsl", Some(distribution)) if !distribution.is_empty() => {
                Ok(Self::Wsl { distribution })
            }
            _ => Err(serde::de::Error::custom("invalid execution target")),
        }
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub provider: String,
    pub workspace: String,
    pub title: String,
    pub state: String,
    pub owner: String,
    pub created_at: i64,
    pub provider_session_id: Option<String>,
    pub shared: bool,
    pub trusted: bool,
    #[serde(default)]
    pub execution_target: ExecutionTarget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: u64,
    pub session_id: String,
    pub kind: String,
    pub payload: Value,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub role: String,
    pub revoked: bool,
    pub member_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CreateSession {
    pub provider: String,
    pub workspace: String,
    pub title: String,
    #[serde(default)]
    pub trusted: bool,
    pub request_id: String,
    #[serde(default)]
    pub execution_target: ExecutionTarget,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Prompt {
    pub text: String,
    pub request_id: String,
}
