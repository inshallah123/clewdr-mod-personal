use serde::{Deserialize, Serialize};

/// Model family for usage bucketing.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelFamily {
    Sonnet,
    Opus,
    Other,
}

impl ModelFamily {
    pub fn classify(model: &str) -> Self {
        let model = model.to_ascii_lowercase();
        if model.contains("opus") {
            Self::Opus
        } else if model.contains("sonnet") {
            Self::Sonnet
        } else {
            Self::Other
        }
    }
}
