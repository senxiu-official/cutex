//! Owner-requested file-only undo. History is read-only; no Git or shell commands.
pub mod journal;
pub mod patch;
pub mod plan;

use sha2::{Digest, Sha256};

pub fn hash(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn content_hash(bytes: &Option<Vec<u8>>) -> Option<String> {
    bytes.as_ref().map(|bytes| hash(bytes))
}
