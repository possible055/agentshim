//! Temporary data contract for the DSH MCP bridge capture path.
//!
//! These types are neutral DTOs that the process tools embed in their requests
//! and metadata while the DSH adapter still reaches this engine through the
//! private MCP bridge. They move out with the bridge removal phase and must not
//! grow new fields or operations.

use serde::{Deserialize, Serialize};

/// Protocol version of the private DSH bridge; metadata fields name it verbatim.
pub const DSH_BRIDGE_VERSION: u64 = 2;

pub const CAPTURE_PROTOCOL_VERSION: u64 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DshCaptureRequest {
    pub version: u64,
    pub id: String,
    pub max_bytes: u64,
    pub preview_bytes: usize,
    pub streams: Vec<String>,
}

impl DshCaptureRequest {
    pub fn validate(&self, expected: &[&str]) -> Result<(), String> {
        if self.version != CAPTURE_PROTOCOL_VERSION {
            return Err(format!(
                "capture version must be {CAPTURE_PROTOCOL_VERSION}"
            ));
        }
        if self.id.len() != 32 || !self.id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("capture id must be 32 hexadecimal characters".to_owned());
        }
        if self.max_bytes == 0 {
            return Err("capture maxBytes must be positive".to_owned());
        }
        if self.preview_bytes == 0 {
            return Err("capture previewBytes must be positive".to_owned());
        }
        if self.streams != expected {
            return Err(format!("capture streams must be {}", expected.join(",")));
        }
        Ok(())
    }
}
