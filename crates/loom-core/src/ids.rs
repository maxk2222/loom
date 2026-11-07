use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! new_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
            pub fn parse(s: &str) -> Result<Self, uuid::Error> {
                Uuid::parse_str(s).map(Self)
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

new_id!(RequestId);
new_id!(ClientId);
new_id!(DeploymentId);
new_id!(VirtualModelId);
new_id!(LeaseId);
new_id!(ProjectId);
new_id!(ApiKeyId);
new_id!(UserId);
new_id!(SessionId);

/// Generate an opaque random token (URL-safe, no padding).
pub fn opaque_token(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        buf,
    )
}

/// sha256 hex digest — used for API keys, device credentials, session tokens.
pub fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(input.as_bytes()))
}
