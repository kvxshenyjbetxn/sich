use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::codec::LengthDelimitedCodec;

/// Maximum frame body size (16 MiB).
pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
}

/// u32 big-endian length prefix, bodies up to 16 MiB.
pub fn frame_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_length(4)
        .max_frame_length(MAX_FRAME_LEN)
        .new_codec()
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Bytes, CodecError> {
    Ok(Bytes::from(postcard::to_stdvec(msg)?))
}

pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, CodecError> {
    Ok(postcard::from_bytes(frame)?)
}
