//! The phone's iroh secret key (its identity), stored as 32 raw bytes in the app data dir.
//!
//! TODO: move the key into the Android Keystore / iOS Keychain. The app data dir is private to
//! the app, which is acceptable for the MVP.

use std::io;
use std::path::Path;

use crate::hosts::write_atomic;

/// Load the key, creating (and saving) a fresh one when the file does not exist.
/// A file of the wrong size is an error: replacing it would silently change the identity.
pub fn load_or_create(path: &Path) -> io::Result<iroh::SecretKey> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "phone key file has the wrong size")
            })?;
            Ok(iroh::SecretKey::from_bytes(&arr))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let key = iroh::SecretKey::generate();
            write_atomic(path, &key.to_bytes())?;
            Ok(key)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_then_reloads_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested").join("phone.key");
        let a = load_or_create(&p).unwrap();
        let b = load_or_create(&p).unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(std::fs::read(&p).unwrap().len(), 32);
    }

    #[test]
    fn different_dirs_get_different_keys() {
        let d1 = tempfile::tempdir().unwrap();
        let d2 = tempfile::tempdir().unwrap();
        let a = load_or_create(&d1.path().join("k")).unwrap();
        let b = load_or_create(&d2.path().join("k")).unwrap();
        assert_ne!(a.public(), b.public());
    }

    #[test]
    fn wrong_size_is_an_error_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("k");
        std::fs::write(&p, b"short").unwrap();
        assert!(load_or_create(&p).is_err());
        assert_eq!(std::fs::read(&p).unwrap(), b"short");
    }
}
