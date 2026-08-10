//! Typed identifiers. All digests are lowercase hex BLAKE3-256.

use serde::{Deserialize, Serialize};

use crate::error::SfsError;

/// A lowercase-hex BLAKE3-256 digest (64 hex chars).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Digest(hex::encode(bytes))
    }

    pub fn parse(s: &str) -> Result<Self, SfsError> {
        if s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Digest(s.to_string()))
        } else {
            Err(SfsError::InvalidId(format!(
                "not a BLAKE3 hex digest: {s:?}"
            )))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

macro_rules! slug_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Validated safe-name: 1..=128 of `[a-z0-9._-]`, no leading dot/dash.
            pub fn parse(s: &str) -> Result<Self, SfsError> {
                let ok = !s.is_empty()
                    && s.len() <= 128
                    && !s.starts_with(['.', '-'])
                    && s.bytes()
                        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'));
                if ok {
                    Ok(Self(s.to_string()))
                } else {
                    Err(SfsError::InvalidId(format!(
                        concat!(stringify!($name), " must be [a-z0-9._-], got {:?}"),
                        s
                    )))
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

slug_id!(
    /// Repository identifier (the AgentFS-compatible safe name).
    RepositoryId
);
slug_id!(
    /// Branch name.
    BranchName
);
slug_id!(
    /// Tenant slug.
    TenantId
);

impl BranchName {
    pub fn main() -> Self {
        BranchName("main".to_string())
    }
}

impl TenantId {
    pub fn default_tenant() -> Self {
        TenantId("default".to_string())
    }
}

macro_rules! digest_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Digest);

        impl $name {
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }

            pub fn parse(s: &str) -> Result<Self, SfsError> {
                Ok(Self(Digest::parse(s)?))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0.as_str())
            }
        }
    };
}

digest_id!(
    /// Content-addressed chunk identity: BLAKE3 of the raw chunk bytes.
    ChunkDigest
);
digest_id!(
    /// Deterministic commit identity (see `canonical::commit_digest`).
    CommitId
);
digest_id!(
    /// State-root identity: digest over the four kind-node digests.
    StateRootId
);
digest_id!(
    /// State-node identity: digest of the canonical node body.
    StateNodeId
);

/// Caller-supplied idempotency key for one publication request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(String);

impl RequestId {
    pub fn parse(s: &str) -> Result<Self, SfsError> {
        if s.is_empty() || s.len() > 256 || s.bytes().any(|b| b.is_ascii_control()) {
            Err(SfsError::InvalidId(format!("invalid request id: {s:?}")))
        } else {
            Ok(RequestId(s.to_string()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
