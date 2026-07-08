use std::fmt;

use clap::ValueEnum;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

pub const CONVERSATION_IDENTITY_META_KEY: &str = "io.github.wycats.mcp-twill/conversation-identity";
pub const CODEX_THREAD_ID_META_KEY: &str = "threadId";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum ConversationIdentityCompatibility {
    #[default]
    Disabled,
    TrustedCodexThreadId,
}

#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ConversationIdentity {
    version: u32,
    issuer: String,
    id: String,
}

impl ConversationIdentity {
    pub fn new(
        version: u32,
        issuer: impl Into<String>,
        id: impl Into<String>,
    ) -> Result<Self, ConversationIdentityError> {
        let issuer = issuer.into();
        let id = id.into();
        if version != 1 {
            return Err(ConversationIdentityError::new(
                Some("version"),
                "unsupported_version",
            ));
        }
        if !valid_issuer(&issuer) {
            return Err(ConversationIdentityError::new(
                Some("issuer"),
                "invalid_issuer",
            ));
        }
        if id.is_empty() {
            return Err(ConversationIdentityError::new(Some("id"), "empty_id"));
        }
        Ok(Self {
            version,
            issuer,
            id,
        })
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Debug for ConversationIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConversationIdentity")
            .field("version", &self.version)
            .field("issuer", &"<redacted>")
            .field("id", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConversationIdentity {
    version: u32,
    issuer: String,
    id: String,
}

impl<'de> Deserialize<'de> for ConversationIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawConversationIdentity::deserialize(deserializer)?;
        Self::new(raw.version, raw.issuer, raw.id).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationIdentityError {
    pub field: Option<&'static str>,
    pub reason: &'static str,
}

impl ConversationIdentityError {
    fn new(field: Option<&'static str>, reason: &'static str) -> Self {
        Self { field, reason }
    }
}

impl fmt::Display for ConversationIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.field {
            Some(field) => write!(
                formatter,
                "conversation identity field `{field}`: {}",
                self.reason
            ),
            None => write!(formatter, "conversation identity: {}", self.reason),
        }
    }
}

impl std::error::Error for ConversationIdentityError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationIdentitySource {
    Canonical,
    CodexThreadId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationIdentityObservationError {
    pub source: ConversationIdentitySource,
    pub field: Option<&'static str>,
    pub reason: &'static str,
}

impl fmt::Display for ConversationIdentityObservationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let source = match self.source {
            ConversationIdentitySource::Canonical => CONVERSATION_IDENTITY_META_KEY,
            ConversationIdentitySource::CodexThreadId => CODEX_THREAD_ID_META_KEY,
        };
        match self.field {
            Some(field) => write!(
                formatter,
                "invalid request metadata `{source}.{field}`: {}",
                self.reason
            ),
            None => write!(
                formatter,
                "invalid request metadata `{source}`: {}",
                self.reason
            ),
        }
    }
}

impl std::error::Error for ConversationIdentityObservationError {}

pub fn normalize_metadata(
    meta: &Map<String, Value>,
    compatibility: ConversationIdentityCompatibility,
) -> Result<Option<ConversationIdentity>, ConversationIdentityObservationError> {
    let canonical = meta
        .get(CONVERSATION_IDENTITY_META_KEY)
        .map(|value| {
            serde_json::from_value::<ConversationIdentity>(value.clone()).map_err(|error| {
                let reason = if error.to_string().contains("unknown field") {
                    "unknown_field"
                } else if error.to_string().contains("missing field") {
                    "missing_field"
                } else if error.to_string().contains("unsupported_version") {
                    "unsupported_version"
                } else if error.to_string().contains("invalid_issuer") {
                    "invalid_issuer"
                } else if error.to_string().contains("empty_id") {
                    "empty_id"
                } else {
                    "expected_object"
                };
                ConversationIdentityObservationError {
                    source: ConversationIdentitySource::Canonical,
                    field: None,
                    reason,
                }
            })
        })
        .transpose()?;

    if compatibility == ConversationIdentityCompatibility::Disabled {
        return Ok(canonical);
    }

    let codex = meta
        .get(CODEX_THREAD_ID_META_KEY)
        .map(|value| {
            let id = value.as_str().filter(|id| !id.is_empty()).ok_or(
                ConversationIdentityObservationError {
                    source: ConversationIdentitySource::CodexThreadId,
                    field: None,
                    reason: "expected_non_empty_string",
                },
            )?;
            ConversationIdentity::new(1, "com.openai.codex", id).map_err(|error| {
                ConversationIdentityObservationError {
                    source: ConversationIdentitySource::CodexThreadId,
                    field: error.field,
                    reason: error.reason,
                }
            })
        })
        .transpose()?;

    match (canonical, codex) {
        (Some(canonical), Some(codex)) if canonical != codex => {
            Err(ConversationIdentityObservationError {
                source: ConversationIdentitySource::Canonical,
                field: None,
                reason: "conflicting_observations",
            })
        }
        (Some(canonical), _) => Ok(Some(canonical)),
        (None, Some(codex)) => Ok(Some(codex)),
        (None, None) => Ok(None),
    }
}

fn valid_issuer(issuer: &str) -> bool {
    let labels = issuer.split('.').collect::<Vec<_>>();
    labels.len() >= 2
        && labels.iter().all(|label| {
            let bytes = label.as_bytes();
            !bytes.is_empty()
                && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
                && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_and_redacts_conversation_identities() {
        let identity = ConversationIdentity::new(1, "com.openai.codex", "thread-secret").unwrap();
        assert_eq!(identity.version(), 1);
        assert_eq!(identity.issuer(), "com.openai.codex");
        assert_eq!(identity.id(), "thread-secret");
        let debug = format!("{identity:?}");
        assert!(!debug.contains("com.openai.codex"));
        assert!(!debug.contains("thread-secret"));

        for issuer in ["single", "Com.example", "com..example", "com.-bad"] {
            assert!(ConversationIdentity::new(1, issuer, "id").is_err());
        }
        assert!(ConversationIdentity::new(2, "com.example", "id").is_err());
        assert!(ConversationIdentity::new(1, "com.example", "").is_err());
    }

    #[test]
    fn canonical_metadata_is_always_normalized() {
        let meta = Map::from_iter([(
            CONVERSATION_IDENTITY_META_KEY.to_string(),
            json!({"version":1,"issuer":"com.example.host","id":"conversation"}),
        )]);
        let identity = normalize_metadata(&meta, ConversationIdentityCompatibility::Disabled)
            .unwrap()
            .unwrap();
        assert_eq!(identity.issuer(), "com.example.host");
        assert_eq!(identity.id(), "conversation");
    }

    #[test]
    fn codex_compatibility_is_disabled_by_default() {
        let meta = Map::from_iter([(CODEX_THREAD_ID_META_KEY.to_string(), json!({"bad":true}))]);
        assert_eq!(
            normalize_metadata(&meta, ConversationIdentityCompatibility::Disabled).unwrap(),
            None
        );

        let meta = Map::from_iter([(CODEX_THREAD_ID_META_KEY.to_string(), json!("thread-secret"))]);
        let identity = normalize_metadata(
            &meta,
            ConversationIdentityCompatibility::TrustedCodexThreadId,
        )
        .unwrap()
        .unwrap();
        assert_eq!(identity.issuer(), "com.openai.codex");
        assert_eq!(identity.id(), "thread-secret");
    }

    #[test]
    fn matching_dual_observations_succeed_and_conflicts_fail() {
        let matching = Map::from_iter([
            (
                CONVERSATION_IDENTITY_META_KEY.to_string(),
                json!({"version":1,"issuer":"com.openai.codex","id":"thread"}),
            ),
            (CODEX_THREAD_ID_META_KEY.to_string(), json!("thread")),
        ]);
        assert!(
            normalize_metadata(
                &matching,
                ConversationIdentityCompatibility::TrustedCodexThreadId
            )
            .unwrap()
            .is_some()
        );

        let mut conflicting = matching;
        conflicting.insert(CODEX_THREAD_ID_META_KEY.to_string(), json!("other"));
        let error = normalize_metadata(
            &conflicting,
            ConversationIdentityCompatibility::TrustedCodexThreadId,
        )
        .unwrap_err();
        assert_eq!(error.reason, "conflicting_observations");
    }
}
