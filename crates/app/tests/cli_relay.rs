//! `sich-cli` relays to the real `sich` binary: output, stderr and the exit code come through.
//! Hermetic: no SICH_TERM_ID, temp config dir, unique socket.

use std::process::Command;

fn cli(args: &[&str]) -> std::process::Output {
    let dir = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_sich-cli"))
        .args(args)
        .env("SICH_EXE", env!("CARGO_BIN_EXE_sich"))
        .env("SICH_CONFIG_DIR", dir.path())
        .env("SICH_SOCKET", format!("sich-test-relay-{}", std::process::id()))
        .env_remove("SICH_TERM_ID")
        .output()
        .unwrap()
}

#[test]
fn relays_error_text_and_nonzero_exit_code() {
    let out = cli(&["orch", "list"]);
    assert_ne!(out.status.code(), Some(0), "{out:?}");
    assert!(out.status.code().is_some());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("SICH_TERM_ID") || err.contains("Sich terminal") || err.contains("terminal"), "{err}");
}

#[test]
fn refuses_no_args_and_unknown_subcommands_with_usage() {
    for args in [&[][..], &["--takeover"][..], &["bogus"][..]] {
        let out = cli(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("usage: sich-cli"));
    }
}

#[test]
fn reports_a_missing_target() {
    let out = Command::new(env!("CARGO_BIN_EXE_sich-cli"))
        .args(["orch", "list"])
        .env("SICH_EXE", "Z:/definitely/missing/sich.exe")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot start"));
}
