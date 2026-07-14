use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    Permission,
    RateLimit,
    QuotaLimit,
    Billing,
    ProviderError,
}

impl AttentionKind {
    pub fn is_provider_limit(self) -> bool {
        matches!(self, Self::RateLimit | Self::QuotaLimit | Self::Billing | Self::ProviderError)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::RateLimit => "rate limit",
            Self::QuotaLimit => "quota limit",
            Self::Billing => "billing",
            Self::ProviderError => "provider error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    pub kind: AttentionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<std::time::SystemTime>,
    pub observed_at: std::time::SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AttentionUpdate {
    Set { attention: Attention },
    Clear { kind: AttentionKind },
}
