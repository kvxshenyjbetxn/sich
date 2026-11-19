use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "build_hash.rs"]
mod build_hash;

fn main() {
    for path in ["src", "Cargo.toml", "../../Cargo.lock", "../../Cargo.toml"] {
        println!("cargo:rerun-if-changed={path}");
    }
    for member in ["daemon", "git", "hooks", "proto", "term"] {
        println!("cargo:rerun-if-changed=../{member}/src");
        println!("cargo:rerun-if-changed=../{member}/Cargo.toml");
    }
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    println!("cargo:rustc-env=SICH_DAEMON_BUILD_ID={}", build_hash::daemon_build_id(&root));
    let mut git = Command::new("git");
    git.args(["rev-parse", "--short", "HEAD"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        git.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let hash = git.output().ok().filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map_or_else(|| "unknown".to_string(), |hash| hash.trim().to_string());
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    println!("cargo:rustc-env=SICH_BUILD_ID={}-{hash}-{seconds}", env!("CARGO_PKG_VERSION"));
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("app.rc", embed_resource::NONE)
            .manifest_optional()
            .unwrap();
        // GPUI's layout/paint recursion is deep; the default 1 MiB main-thread stack overflows
        // in debug builds with a nested sidebar tree.
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bins=/STACK:8388608");
        }
    }
}
