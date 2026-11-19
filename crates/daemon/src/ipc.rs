//! Local socket naming and client-side connect, shared with the UI.

use std::io;

use interprocess::local_socket::tokio::{Stream, prelude::*};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, Name};

pub const DEFAULT_SOCKET_NAME: &str = "sich-daemon";

/// Socket name: env `SICH_SOCKET` if set, else `sich-daemon`.
pub fn socket_name_from_env() -> String {
    std::env::var("SICH_SOCKET")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_SOCKET_NAME.to_string())
}

pub(crate) fn to_name(name: &str) -> io::Result<Name<'static>> {
    if GenericNamespaced::is_supported() {
        name.to_string().to_ns_name::<GenericNamespaced>()
    } else {
        std::env::temp_dir().join(format!("{name}.sock")).to_fs_name::<GenericFilePath>()
    }
}

/// Connects to a running daemon.
pub async fn connect(name: &str) -> io::Result<Stream> {
    Stream::connect(to_name(name)?).await
}
