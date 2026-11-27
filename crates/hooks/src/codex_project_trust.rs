//! Marks a directory as trusted in Codex's `config.toml` (`[projects."<path>"]`) so a worker
//! launched there does not stop at the "do you trust this folder" prompt. Only appends a new
//! section when the path has none; the rest of the file is never rewritten.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;

/// The `[projects."<path>"]` header (TOML basic-string key, backslashes and quotes escaped).
pub fn section_header(dir: &Path) -> String {
    let path = dir.to_string_lossy();
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    format!("[projects.\"{}\"]", path.replace('\\', "\\\\").replace('"', "\\\""))
}

fn normalize(path: &str) -> String {
    let path = path.replace('/', "\\");
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path).trim_end_matches('\\').to_owned();
    if cfg!(windows) { path.to_ascii_lowercase() } else { path }
}

/// The key of a `[projects.<key>]` header line (basic or literal string), if it is one.
fn header_key(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("[projects.")?.trim_start();
    if let Some(body) = rest.strip_prefix('"') {
        let mut key = String::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return Some(key),
                '\\' => key.push(chars.next()?),
                c => key.push(c),
            }
        }
        None
    } else if let Some(body) = rest.strip_prefix('\'') {
        body.split_once('\'').map(|(key, _)| key.to_owned())
    } else {
        None
    }
}

/// True when `config` already has a `[projects.<dir>]` section (either string style).
pub fn has_entry(config: &str, dir: &Path) -> bool {
    let want = normalize(&dir.to_string_lossy());
    config.lines().filter_map(header_key).any(|key| normalize(&key) == want)
}

/// The text to append for `dir` given the current file content (`None` when already present).
pub fn section_to_append(config: &str, dir: &Path) -> Option<String> {
    if has_entry(config, dir) {
        return None;
    }
    let lead = if config.is_empty() || config.ends_with("\n\n") {
        ""
    } else if config.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    Some(format!("{lead}{}\ntrust_level = \"trusted\"\n", section_header(dir)))
}

/// Ensures `<codex_home>/config.toml` trusts `dir`. Existing entries are left untouched.
pub fn ensure_trusted(codex_home: &Path, dir: &Path) -> anyhow::Result<()> {
    let path = codex_home.join("config.toml");
    let config = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).context("read Codex config.toml"),
    };
    let Some(section) = section_to_append(&config, dir) else { return Ok(()) };
    fs::create_dir_all(codex_home).context("create Codex home")?;
    let mut file = OpenOptions::new().create(true).append(true).open(&path).context("open Codex config.toml")?;
    file.write_all(section.as_bytes()).context("append Codex project trust")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_escapes_backslashes() {
        assert_eq!(
            section_header(Path::new(r"E:\dev\program\live.translator")),
            r#"[projects."E:\\dev\\program\\live.translator"]"#
        );
        assert_eq!(section_header(Path::new(r"\\?\C:\a")), r#"[projects."C:\\a"]"#);
    }

    #[test]
    fn existing_entries_are_detected_in_both_string_styles() {
        let config = r#"model = "x"

[projects."E:\\dev\\game"]
trust_level = "trusted"

[projects.'C:\Users\me']
trust_level = "trusted"
"#;
        assert!(has_entry(config, Path::new(r"E:\dev\game")));
        assert!(has_entry(config, Path::new(r"C:\Users\me")));
        assert!(!has_entry(config, Path::new(r"E:\dev\other")));
        assert_eq!(section_to_append(config, Path::new(r"E:\dev\game")), None);
        if cfg!(windows) {
            assert!(has_entry(config, Path::new("e:/dev/game/")));
        }
    }

    #[test]
    fn appends_with_a_separating_blank_line_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let original = "model = \"x\"\n# note";
        fs::write(dir.path().join("config.toml"), original).unwrap();
        ensure_trusted(dir.path(), Path::new(r"E:\w")).unwrap();
        let text = fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert_eq!(text, format!("{original}\n\n[projects.\"E:\\\\w\"]\ntrust_level = \"trusted\"\n"));
        ensure_trusted(dir.path(), Path::new(r"E:\w")).unwrap();
        assert_eq!(fs::read_to_string(dir.path().join("config.toml")).unwrap(), text);
    }
}
