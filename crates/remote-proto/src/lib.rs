//! Phone <-> gateway wire protocol. Platform independent (also built for Android).
//! Does not depend on sich-proto: the few needed types are mirrored here.

mod msg;
mod pair;

#[cfg(feature = "tokio")]
mod codec;

#[cfg(test)]
mod tests;

pub use msg::*;
pub use pair::{PairUrl, PairUrlError};

#[cfg(feature = "tokio")]
pub use codec::{decode_frame, encode_frame, frame_codec};

use serde::Serialize;
use serde::de::DeserializeOwned;

pub const ALPN: &[u8] = b"sich/remote/1";
pub const REMOTE_PROTO_VERSION: u32 = 1;
pub const MIN_COMPATIBLE: u32 = 1;
/// Maximum frame body size (16 MiB).
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("frame too large: {0} bytes")]
    TooLarge(usize),
}

/// Both sides exchange `(proto_version, min_compatible)`; compatible iff each
/// side's version is at least the other's minimum.
pub fn compatible(their_version: u32, their_min: u32) -> bool {
    REMOTE_PROTO_VERSION >= their_min && their_version >= MIN_COMPATIBLE
}

/// Postcard body, no length prefix.
pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    postcard::to_stdvec(msg).expect("postcard serialization of wire types cannot fail")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Error> {
    Ok(postcard::from_bytes(bytes)?)
}

/// u32 big-endian length prefix + postcard body.
pub fn frame<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = encode(msg);
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}
