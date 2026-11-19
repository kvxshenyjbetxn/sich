//! Environment for spawned terminals: a clean user environment instead of whatever the
//! process that launched the daemon happened to leak (agent sessions, other terminals...).

use std::ffi::{OsStr, OsString};

/// Variables that must never leak from the launcher (exact names).
const BLOCKED_NAMES: &[&str] =
    &["CLAUDECODE", "AI_AGENT", "TERM_PROGRAM", "TERM_PROGRAM_VERSION", "NO_COLOR", "FORCE_COLOR"];

/// Variables that must never leak from the launcher (prefixes).
const BLOCKED_PREFIXES: &[&str] = &["CLAUDE_CODE_", "CLAUDE_", "ORCA_", "SICH_"];

/// Variables we always set, overriding anything inherited.
fn forced_vars() -> Vec<(OsString, OsString)> {
    vec![
        ("TERM".into(), "xterm-256color".into()),
        ("COLORTERM".into(), "truecolor".into()),
        ("TERM_PROGRAM".into(), "sich".into()),
        ("TERM_PROGRAM_VERSION".into(), env!("CARGO_PKG_VERSION").into()),
    ]
}

/// Key used for comparisons: case-insensitive on Windows, exact elsewhere.
fn norm(key: &OsStr) -> String {
    let s = key.to_string_lossy();
    if cfg!(windows) { s.to_uppercase() } else { s.into_owned() }
}

fn is_blocked(key: &OsStr) -> bool {
    let k = norm(key);
    BLOCKED_NAMES.iter().any(|n| k == *n) || BLOCKED_PREFIXES.iter().any(|p| k.starts_with(p))
}

fn upsert(out: &mut Vec<(OsString, OsString)>, key: OsString, val: OsString) {
    let nk = norm(&key);
    if let Some(slot) = out.iter_mut().find(|(k, _)| norm(k) == nk) {
        *slot = (key, val);
    } else {
        out.push((key, val));
    }
}

/// Filters out leaking variables from `base` and applies the forced ones.
pub fn build_env(base: Vec<(OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let mut out: Vec<(OsString, OsString)> = Vec::with_capacity(base.len() + 4);
    for (k, v) in base {
        if k.is_empty() || is_blocked(&k) {
            continue;
        }
        upsert(&mut out, k, v);
    }
    for (k, v) in forced_vars() {
        upsert(&mut out, k, v);
    }
    out
}

/// The environment the terminals should start from (before `build_env`).
#[cfg_attr(not(test), allow(dead_code))]
pub fn base_env() -> Vec<(OsString, OsString)> {
    #[cfg(windows)]
    {
        match windows_user_env() {
            Ok(v) => return v,
            Err(e) => tracing::warn!("{e}; using the inherited environment"),
        }
    }
    std::env::vars_os().collect()
}

/// Sets the forced vars without filtering anything else.
pub fn with_forced(base: Vec<(OsString, OsString)>) -> Vec<(OsString, OsString)> {
    let mut out: Vec<(OsString, OsString)> = Vec::with_capacity(base.len() + 4);
    for (k, v) in base.into_iter().chain(forced_vars()) {
        if !k.is_empty() {
            upsert(&mut out, k, v);
        }
    }
    out
}

/// Complete environment for a new terminal.
///
/// The Windows registry environment is what the user configured on purpose, so it is kept
/// as-is (launcher leaks can't be in it). Only an inherited environment gets filtered.
pub fn terminal_env() -> Vec<(OsString, OsString)> {
    #[cfg(windows)]
    {
        match windows_user_env() {
            Ok(v) => return with_forced(v),
            Err(e) => tracing::warn!("{e}; using the filtered inherited environment"),
        }
    }
    build_env(std::env::vars_os().collect())
}

/// Parses a UTF-16 double-NUL-terminated environment block. Skips `=`-prefixed entries.
#[cfg(windows)]
fn parse_block(block: &[u16]) -> Vec<(OsString, OsString)> {
    use std::os::windows::ffi::OsStringExt as _;
    let mut out = Vec::new();
    for entry in block.split(|&c| c == 0) {
        if entry.is_empty() {
            break;
        }
        if entry[0] == u16::from(b'=') {
            continue;
        }
        if let Some(eq) = entry.iter().position(|&c| c == u16::from(b'=')) {
            out.push((OsString::from_wide(&entry[..eq]), OsString::from_wide(&entry[eq + 1..])));
        }
    }
    out
}

#[cfg(windows)]
fn windows_user_env() -> Result<Vec<(OsString, OsString)>, String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE};
    use windows_sys::Win32::Security::{TOKEN_DUPLICATE, TOKEN_QUERY};
    use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: plain Win32 calls; the token is closed and the block destroyed on every path,
    // and the block is only read while alive, up to its double-NUL terminator.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_DUPLICATE, &mut token) == 0 {
            return Err(format!("OpenProcessToken failed: {}", std::io::Error::last_os_error()));
        }
        let mut block: *mut core::ffi::c_void = std::ptr::null_mut();
        if CreateEnvironmentBlock(&mut block, token, FALSE) == 0 {
            let e = std::io::Error::last_os_error();
            CloseHandle(token);
            return Err(format!("CreateEnvironmentBlock failed: {e}"));
        }
        let p = block as *const u16;
        let mut len = 0usize;
        while !(*p.add(len) == 0 && *p.add(len + 1) == 0) {
            len += 1;
        }
        let vars = parse_block(std::slice::from_raw_parts(p, len + 2));
        DestroyEnvironmentBlock(block);
        CloseHandle(token);
        Ok(vars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> OsString {
        s.into()
    }
    fn pairs(v: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        v.iter().map(|(k, x)| (os(k), os(x))).collect()
    }
    fn get<'a>(env: &'a [(OsString, OsString)], key: &str) -> Option<&'a OsString> {
        env.iter().find(|(k, _)| norm(k) == norm(OsStr::new(key))).map(|(_, v)| v)
    }

    #[test]
    fn with_forced_keeps_user_configured_vars() {
        let env = with_forced(pairs(&[
            ("CLAUDE_CODE_PLUGIN_DIRS", "C:\\mods"),
            ("TERM_PROGRAM", "other"),
            ("PATH", "C:\\bin"),
        ]));
        assert_eq!(get(&env, "CLAUDE_CODE_PLUGIN_DIRS"), Some(&os("C:\\mods")));
        assert_eq!(get(&env, "PATH"), Some(&os("C:\\bin")));
        assert_eq!(get(&env, "TERM_PROGRAM"), Some(&os("sich")));
        assert_eq!(get(&env, "COLORTERM"), Some(&os("truecolor")));
    }

    #[test]
    fn blocked_names_removed() {
        for n in BLOCKED_NAMES {
            let env = build_env(pairs(&[(n, "leak"), ("KEEP", "1")]));
            // TERM_PROGRAM* are removed and then re-set to our own values.
            assert_ne!(get(&env, n), Some(&os("leak")), "{n} leaked");
            assert!(get(&env, "KEEP").is_some());
            if !n.starts_with("TERM_PROGRAM") {
                assert!(get(&env, n).is_none(), "{n} leaked");
            }
        }
    }

    #[test]
    fn blocked_prefixes_removed() {
        for p in BLOCKED_PREFIXES {
            let name = format!("{p}SOMETHING");
            let env = build_env(pairs(&[(&name, "x"), ("KEEP", "1")]));
            assert!(get(&env, &name).is_none(), "{name} leaked");
            assert!(get(&env, "KEEP").is_some());
        }
    }

    #[test]
    fn unrelated_vars_kept() {
        let env = build_env(pairs(&[("PATH", "/bin"), ("HOME", "/h"), ("CLAUDEX", "1"), ("MY_SICH_X", "2")]));
        assert_eq!(get(&env, "PATH"), Some(&os("/bin")));
        assert_eq!(get(&env, "HOME"), Some(&os("/h")));
        assert!(get(&env, "CLAUDEX").is_some());
        assert!(get(&env, "MY_SICH_X").is_some());
    }

    #[test]
    fn forced_vars_set_and_override() {
        let env = build_env(pairs(&[("TERM", "dumb"), ("COLORTERM", "no"), ("TERM_PROGRAM", "other")]));
        assert_eq!(get(&env, "TERM"), Some(&os("xterm-256color")));
        assert_eq!(get(&env, "COLORTERM"), Some(&os("truecolor")));
        assert_eq!(get(&env, "TERM_PROGRAM"), Some(&os("sich")));
        assert_eq!(get(&env, "TERM_PROGRAM_VERSION"), Some(&os(env!("CARGO_PKG_VERSION"))));
        assert_eq!(env.iter().filter(|(k, _)| k == "TERM").count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn case_insensitive_on_windows() {
        let env = build_env(pairs(&[
            ("Path", "a"),
            ("PATH", "b"),
            ("claudecode", "1"),
            ("Claude_Code_X", "1"),
            ("term", "dumb"),
        ]));
        let count = |n: &str| env.iter().filter(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case(n)).count();
        assert_eq!(count("path"), 1);
        assert_eq!(get(&env, "path"), Some(&os("b")));
        assert!(get(&env, "CLAUDECODE").is_none());
        assert!(get(&env, "CLAUDE_CODE_X").is_none());
        assert_eq!(count("term"), 1);
        assert_eq!(get(&env, "TERM"), Some(&os("xterm-256color")));
    }

    #[cfg(not(windows))]
    #[test]
    fn case_sensitive_on_unix() {
        let env = build_env(pairs(&[("Path", "a"), ("PATH", "b")]));
        assert_eq!(env.iter().filter(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("path")).count(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn parse_block_skips_equals_entries() {
        let mut block: Vec<u16> = Vec::new();
        for s in ["=C:=C:\\x", "A=1", "B=x=y"] {
            block.extend(s.encode_utf16());
            block.push(0);
        }
        block.push(0);
        assert_eq!(parse_block(&block), pairs(&[("A", "1"), ("B", "x=y")]));
    }

    #[cfg(windows)]
    #[test]
    fn windows_base_env_has_path_and_systemroot() {
        let env = base_env();
        assert!(get(&env, "PATH").is_some(), "no PATH");
        assert!(get(&env, "SystemRoot").is_some(), "no SystemRoot");
        assert!(env.iter().all(|(k, _)| !k.to_string_lossy().starts_with('=')));
    }
}
