//! Wire types and frame codec shared by the daemon and all clients.

mod codec;
mod msg;

pub use codec::{CodecError, MAX_FRAME_LEN, decode, encode, frame_codec};
pub use msg::{ClientMsg, MAX_OUTPUT_CHUNK, PROTO_VERSION, ServerMsg, TermId, TerminalInfo};

#[cfg(test)]
mod tests;
