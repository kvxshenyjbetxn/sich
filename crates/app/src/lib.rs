/// Identifier shared by the GUI and daemon compiled in this build (changes on every rebuild).
pub fn build_id() -> &'static str {
    env!("SICH_BUILD_ID")
}

/// Content hash of the daemon-relevant sources (daemon, proto, their manifests, Cargo.lock).
/// Changes only when daemon behavior may have changed; the daemon reports it as
/// `DaemonInfo.build_id` and the GUI compares against it.
pub fn daemon_build_id() -> &'static str {
    env!("SICH_DAEMON_BUILD_ID")
}

#[cfg(test)]
#[path = "../build_hash.rs"]
mod build_hash;
