use crate::{Error, MAX_FRAME};
use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio_util::codec::LengthDelimitedCodec;

/// u32 big-endian length prefix, bodies up to 16 MiB.
pub fn frame_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_length(4)
        .max_frame_length(MAX_FRAME)
        .new_codec()
}

/// Encode a message as a frame body for `frame_codec()` (the codec adds the prefix).
/// Fails when the body exceeds `MAX_FRAME`.
pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Bytes, Error> {
    let body = crate::encode(msg);
    if body.len() > MAX_FRAME {
        return Err(Error::TooLarge(body.len()));
    }
    Ok(Bytes::from(body))
}

pub fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, Error> {
    crate::decode(frame)
}
