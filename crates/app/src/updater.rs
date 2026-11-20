//! Portable-zip auto-updater backed by GitHub Releases of the public Sich repository.
//!
//! The logic is split into pure, tested pieces (release parsing, checksum parsing, applying a zip
//! onto a directory) and thin network glue (`fetch_latest`, `download`). The UI lives in
//! `update_dialog` and `app_view`.

use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

/// Updates are only offered by official release builds (the release script sets `SICH_RELEASE`).
pub const OFFICIAL: bool = option_env!("SICH_RELEASE").is_some();

const LATEST_URL: &str = "https://api.github.com/repos/kvxshenyjbetxn/sich/releases/latest";
const USER_AGENT: &str = "sich-updater";
const STAGE_DIR: &str = ".sich-update";
const STAGED_SUBDIR: &str = "staged";
const REQUIRED_FILE: &str = if cfg!(windows) { "sich.exe" } else { "sich" };
const MAX_JSON_BYTES: u64 = 4 * 1024 * 1024;
const MAX_NOTES_CHARS: usize = 4000;
const MAX_OLD_SUFFIX: u32 = 100;

/// First check delay after startup and the interval between checks.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// The install directory is not writable (e.g. Program Files).
#[derive(Debug)]
pub struct NoWriteAccess;

impl std::fmt::Display for NoWriteAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no write access to the install directory")
    }
}

impl std::error::Error for NoWriteAccess {}

/// The user cancelled the download.
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("update cancelled")
    }
}

impl std::error::Error for Cancelled {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: semver::Version,
    pub notes: String,
    pub zip_name: String,
    pub zip_url: String,
    pub zip_size: u64,
    pub sha_url: String,
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<RawAsset>,
}

#[derive(Deserialize)]
struct RawAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Parses the GitHub "latest release" JSON; `Some` only when it is newer than `current`.
pub fn parse_release(json: &str, current: &semver::Version) -> Result<Option<Release>> {
    let raw: RawRelease = serde_json::from_str(json).context("invalid release JSON")?;
    if raw.draft || raw.prerelease {
        return Ok(None);
    }
    let tag = raw.tag_name.trim();
    let version = semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag))
        .with_context(|| format!("bad release tag {tag:?}"))?;
    if version <= *current {
        return Ok(None);
    }
    let zip_name = format!("sich-{version}-windows-x64.zip");
    let sha_name = format!("{zip_name}.sha256");
    let find = |name: &str| raw.assets.iter().find(|a| a.name == name);
    let zip = find(&zip_name).ok_or_else(|| anyhow!("release {version} has no asset {zip_name}"))?;
    let sha = find(&sha_name).ok_or_else(|| anyhow!("release {version} has no asset {sha_name}"))?;
    Ok(Some(Release {
        version,
        notes: truncate_notes(raw.body.as_deref().unwrap_or(""), MAX_NOTES_CHARS),
        zip_name,
        zip_url: zip.browser_download_url.clone(),
        zip_size: zip.size,
        sha_url: sha.browser_download_url.clone(),
    }))
}

/// Normalizes line endings and cuts very long notes at a char boundary.
pub fn truncate_notes(notes: &str, max_chars: usize) -> String {
    let notes = notes.replace("\r\n", "\n");
    let notes = notes.trim();
    if notes.chars().count() <= max_chars {
        return notes.to_string();
    }
    let cut: String = notes.chars().take(max_chars).collect();
    format!("{}…", cut.trim_end())
}

/// Extracts the hex digest from a `sha256sum` style file for `zip_name`.
pub fn parse_sha256(text: &str, zip_name: &str) -> Result<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), name) = (parts.next(), parts.next()) else { continue };
        let name = name.map(|n| n.trim_start_matches('*'));
        if name.is_some_and(|n| n != zip_name) {
            continue;
        }
        if hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(hash.to_ascii_lowercase());
        }
    }
    bail!("no valid sha256 for {zip_name}")
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Shared download progress (polled by the UI).
#[derive(Default)]
pub struct Progress {
    pub downloaded: AtomicU64,
    pub total: AtomicU64,
    pub cancelled: AtomicBool,
}

impl Progress {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> (u64, u64) {
        (self.downloaded.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed))
    }
}

/// Whole percent in 0..=100; 0 when the total is unknown.
pub fn percent(done: u64, total: u64) -> u32 {
    if total == 0 { 0 } else { (done.min(total) * 100 / total) as u32 }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Downloading,
    Verifying,
    Installing,
}

pub fn current_version() -> semver::Version {
    semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("workspace version is valid semver")
}

/// Directory holding the user-facing executables (parent of the launcher exe).
pub fn install_dir() -> Option<PathBuf> {
    crate::daemon_host::launcher_exe()?.parent().map(Path::to_path_buf)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .build()
}

fn read_limited(resp: ureq::Response, limit: u64) -> Result<String> {
    let mut text = String::new();
    resp.into_reader().take(limit).read_to_string(&mut text)?;
    Ok(text)
}

/// Blocking: asks GitHub for the latest release.
pub fn fetch_latest(current: &semver::Version) -> Result<Option<Release>> {
    let resp = agent()
        .get(LATEST_URL)
        .set("User-Agent", USER_AGENT)
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("release check failed")?;
    parse_release(&read_limited(resp, MAX_JSON_BYTES)?, current)
}

/// Fails with `NoWriteAccess` when files cannot be created in `install_dir`.
pub fn ensure_writable(install_dir: &Path) -> Result<()> {
    let stage = install_dir.join(STAGE_DIR);
    let probe = || -> std::io::Result<()> {
        fs::create_dir_all(&stage)?;
        let p = stage.join(".probe");
        fs::write(&p, b"")?;
        fs::remove_file(&p)
    };
    probe().map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow::Error::new(NoWriteAccess)
        } else {
            anyhow::Error::new(e).context("prepare update directory")
        }
    })
}

/// Blocking: downloads and verifies the zip; returns its path. Deletes the file on failure.
pub fn download(
    release: &Release,
    install_dir: &Path,
    progress: &Progress,
    on_stage: &dyn Fn(Stage),
) -> Result<PathBuf> {
    let zip_path = install_dir.join(STAGE_DIR).join(&release.zip_name);
    let result = download_inner(release, &zip_path, progress, on_stage);
    if result.is_err() {
        let _ = fs::remove_file(&zip_path);
    }
    result.map(|()| zip_path)
}

fn download_inner(
    release: &Release,
    zip_path: &Path,
    progress: &Progress,
    on_stage: &dyn Fn(Stage),
) -> Result<()> {
    on_stage(Stage::Downloading);
    let agent = agent();
    let resp = agent
        .get(&release.zip_url)
        .set("User-Agent", USER_AGENT)
        .call()
        .context("download failed")?;
    let total = resp
        .header("Content-Length")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(release.zip_size);
    progress.total.store(total, Ordering::Relaxed);
    progress.downloaded.store(0, Ordering::Relaxed);
    let mut reader = resp.into_reader();
    let mut file = File::create(zip_path).with_context(|| format!("create {}", zip_path.display()))?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if progress.cancelled.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let n = reader.read(&mut buf).context("download interrupted")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        progress.downloaded.fetch_add(n as u64, Ordering::Relaxed);
    }
    file.flush()?;
    drop(file);

    on_stage(Stage::Verifying);
    let sha_text = read_limited(
        agent.get(&release.sha_url).set("User-Agent", USER_AGENT).call().context("checksum download failed")?,
        4096,
    )?;
    let expected = parse_sha256(&sha_text, &release.zip_name)?;
    let actual = sha256_file(zip_path)?;
    if expected != actual {
        bail!("checksum mismatch for {}", release.zip_name);
    }
    Ok(())
}

/// Blocking: the whole update except the restart.
pub fn run_update(
    release: &Release,
    install_dir: &Path,
    progress: &Progress,
    on_stage: &dyn Fn(Stage),
) -> Result<()> {
    ensure_writable(install_dir)?;
    let zip = download(release, install_dir, progress, on_stage)?;
    on_stage(Stage::Installing);
    apply_update(&zip, install_dir)
}

/// Extracts `zip` and swaps the files into `install_dir`, keeping the old ones as `*.old`.
pub fn apply_update(zip: &Path, install_dir: &Path) -> Result<()> {
    apply_update_with(zip, install_dir, REQUIRED_FILE)
}

fn apply_update_with(zip: &Path, install_dir: &Path, required: &str) -> Result<()> {
    let staged_dir = install_dir.join(STAGE_DIR).join(STAGED_SUBDIR);
    let names = extract(zip, &staged_dir, required)?;
    swap_in(&names, &staged_dir, install_dir)
}

/// Flat, plain file name only (no separators, no `..`, no drive prefixes).
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':'])
        && Path::new(name).components().count() == 1
}

fn extract(zip_path: &Path, staged_dir: &Path, required: &str) -> Result<Vec<String>> {
    if staged_dir.exists() {
        fs::remove_dir_all(staged_dir).context("clear staging directory")?;
    }
    fs::create_dir_all(staged_dir).context("create staging directory")?;
    let mut archive = zip::ZipArchive::new(File::open(zip_path).context("open update zip")?)
        .context("invalid update zip")?;
    let mut names = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if entry.is_dir() {
            bail!("unexpected directory {name:?} in update zip");
        }
        if !is_plain_name(&name) {
            bail!("unsafe entry {name:?} in update zip");
        }
        let mut out = File::create(staged_dir.join(&name))?;
        std::io::copy(&mut entry, &mut out).with_context(|| format!("extract {name}"))?;
        names.push(name);
    }
    if !names.iter().any(|n| n == required) {
        bail!("update zip does not contain {required}");
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Chooses a free `<name>.old` (or `.old.N`) path, deleting a stale one when possible.
fn pick_old_path(dir: &Path, name: &str) -> Result<PathBuf> {
    let base = dir.join(format!("{name}.old"));
    let candidates =
        std::iter::once(base).chain((1..=MAX_OLD_SUFFIX).map(|n| dir.join(format!("{name}.old.{n}"))));
    for path in candidates {
        if path.symlink_metadata().is_err() || fs::remove_file(&path).is_ok() {
            return Ok(path);
        }
    }
    bail!("no free backup name for {name}")
}

struct Swapped {
    target: PathBuf,
    old: Option<PathBuf>,
    placed: bool,
}

fn swap_in(names: &[String], staged_dir: &Path, install_dir: &Path) -> Result<()> {
    let mut done: Vec<Swapped> = Vec::new();
    for name in names {
        let target = install_dir.join(name);
        let mut step = Swapped { target: target.clone(), old: None, placed: false };
        let result = (|| -> Result<()> {
            if let Ok(meta) = target.symlink_metadata() {
                if meta.is_dir() {
                    bail!("{} is a directory", target.display());
                }
                let old = pick_old_path(install_dir, name)?;
                fs::rename(&target, &old).with_context(|| format!("move aside {name}"))?;
                step.old = Some(old);
            }
            fs::rename(staged_dir.join(name), &target).with_context(|| format!("install {name}"))?;
            step.placed = true;
            Ok(())
        })();
        done.push(step);
        if let Err(err) = result {
            rollback(&done);
            return Err(err);
        }
    }
    Ok(())
}

fn rollback(done: &[Swapped]) {
    for step in done.iter().rev() {
        if step.placed {
            let _ = fs::remove_file(&step.target);
        }
        if let Some(old) = &step.old {
            if let Err(e) = fs::rename(old, &step.target) {
                tracing::error!("update rollback failed for {}: {e}", step.target.display());
            }
        }
    }
}

fn is_old_name(name: &str) -> bool {
    if name.ends_with(".old") {
        return true;
    }
    match name.rsplit_once('.') {
        Some((rest, n)) => !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && rest.ends_with(".old"),
        None => false,
    }
}

/// Best-effort removal of leftovers from a previous update.
pub fn cleanup_old(install_dir: &Path) {
    if let Ok(entries) = fs::read_dir(install_dir) {
        for entry in entries.flatten() {
            let is_file = entry.file_type().is_ok_and(|t| t.is_file());
            if is_file && entry.file_name().to_str().is_some_and(is_old_name) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let _ = fs::remove_dir_all(install_dir.join(STAGE_DIR));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ver(s: &str) -> semver::Version {
        semver::Version::parse(s).unwrap()
    }

    fn release_json(tag: &str, draft: bool, pre: bool, assets: &[&str]) -> String {
        let assets: Vec<_> = assets
            .iter()
            .map(|n| format!(r#"{{"name":"{n}","browser_download_url":"https://x/{n}","size":1234}}"#))
            .collect();
        format!(
            r#"{{"tag_name":"{tag}","body":"notes\r\nline","draft":{draft},"prerelease":{pre},"assets":[{}]}}"#,
            assets.join(",")
        )
    }

    const ASSETS: [&str; 2] = ["sich-0.2.0-windows-x64.zip", "sich-0.2.0-windows-x64.zip.sha256"];

    #[test]
    fn newer_release_is_returned() {
        let r = parse_release(&release_json("v0.2.0", false, false, &ASSETS), &ver("0.1.0")).unwrap().unwrap();
        assert_eq!(r.version, ver("0.2.0"));
        assert_eq!(r.notes, "notes\nline");
        assert_eq!(r.zip_name, "sich-0.2.0-windows-x64.zip");
        assert_eq!(r.zip_url, "https://x/sich-0.2.0-windows-x64.zip");
        assert_eq!(r.zip_size, 1234);
        assert_eq!(r.sha_url, "https://x/sich-0.2.0-windows-x64.zip.sha256");
    }

    #[test]
    fn same_or_older_release_is_none() {
        let json = release_json("v0.2.0", false, false, &ASSETS);
        assert!(parse_release(&json, &ver("0.2.0")).unwrap().is_none());
        assert!(parse_release(&json, &ver("0.3.0")).unwrap().is_none());
    }

    #[test]
    fn tag_without_v_prefix_works() {
        assert!(parse_release(&release_json("0.2.0", false, false, &ASSETS), &ver("0.1.0")).unwrap().is_some());
    }

    #[test]
    fn draft_and_prerelease_are_skipped() {
        assert!(parse_release(&release_json("v0.2.0", true, false, &ASSETS), &ver("0.1.0")).unwrap().is_none());
        assert!(parse_release(&release_json("v0.2.0", false, true, &ASSETS), &ver("0.1.0")).unwrap().is_none());
    }

    #[test]
    fn missing_assets_are_an_error() {
        let only_zip = release_json("v0.2.0", false, false, &ASSETS[..1]);
        assert!(parse_release(&only_zip, &ver("0.1.0")).is_err());
        assert!(parse_release(&release_json("v0.2.0", false, false, &[]), &ver("0.1.0")).is_err());
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse_release("not json", &ver("0.1.0")).is_err());
        assert!(parse_release(&release_json("latest", false, false, &ASSETS), &ver("0.1.0")).is_err());
    }

    #[test]
    fn missing_body_gives_empty_notes() {
        let json = r#"{"tag_name":"v0.2.0","assets":[
            {"name":"sich-0.2.0-windows-x64.zip","browser_download_url":"u1"},
            {"name":"sich-0.2.0-windows-x64.zip.sha256","browser_download_url":"u2"}]}"#;
        let r = parse_release(json, &ver("0.1.0")).unwrap().unwrap();
        assert_eq!(r.notes, "");
    }

    #[test]
    fn long_notes_are_truncated() {
        assert_eq!(truncate_notes("short", 10), "short");
        let cut = truncate_notes(&"я".repeat(50), 10);
        assert_eq!(cut.chars().count(), 11);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn sha256_line_parses() {
        let hash = "ab".repeat(32);
        assert_eq!(parse_sha256(&format!("{hash}  a.zip\n"), "a.zip").unwrap(), hash);
        assert_eq!(parse_sha256(&format!("{}  *a.zip\n", hash.to_uppercase()), "a.zip").unwrap(), hash);
        assert_eq!(parse_sha256(&format!("{hash}\n"), "a.zip").unwrap(), hash);
    }

    #[test]
    fn sha256_rejects_wrong_name_and_bad_hash() {
        let hash = "ab".repeat(32);
        assert!(parse_sha256(&format!("{hash}  other.zip\n"), "a.zip").is_err());
        assert!(parse_sha256("xyz  a.zip\n", "a.zip").is_err());
        assert!(parse_sha256("", "a.zip").is_err());
    }

    #[test]
    fn sha256_of_file_matches_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(sha256_file(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn percent_is_clamped() {
        assert_eq!(percent(0, 0), 0);
        assert_eq!(percent(50, 200), 25);
        assert_eq!(percent(300, 200), 100);
    }

    #[test]
    fn progress_cancel_and_snapshot() {
        let p = Progress::default();
        p.total.store(10, Ordering::Relaxed);
        p.downloaded.store(4, Ordering::Relaxed);
        assert_eq!(p.snapshot(), (4, 10));
        assert!(!p.cancelled.load(Ordering::Relaxed));
        p.cancel();
        assert!(p.cancelled.load(Ordering::Relaxed));
    }

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap();
    }

    fn read(p: impl AsRef<Path>) -> String {
        fs::read_to_string(p).unwrap()
    }

    #[test]
    fn apply_replaces_files_and_keeps_old() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("app");
        fs::create_dir(&install).unwrap();
        fs::write(install.join("sich.exe"), "old-sich").unwrap();
        fs::write(install.join("untouched.txt"), "keep").unwrap();
        let zip = dir.path().join("u.zip");
        make_zip(&zip, &[("sich.exe", b"new-sich"), ("conpty.dll", b"new-dll")]);

        apply_update_with(&zip, &install, "sich.exe").unwrap();

        assert_eq!(read(install.join("sich.exe")), "new-sich");
        assert_eq!(read(install.join("conpty.dll")), "new-dll");
        assert_eq!(read(install.join("sich.exe.old")), "old-sich");
        assert_eq!(read(install.join("untouched.txt")), "keep");
    }

    #[test]
    fn apply_works_when_old_backup_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path();
        fs::write(install.join("sich.exe"), "v1").unwrap();
        fs::write(install.join("sich.exe.old"), "v0").unwrap();
        let zip = install.join("u.zip");
        make_zip(&zip, &[("sich.exe", b"v2")]);
        apply_update_with(&zip, install, "sich.exe").unwrap();
        assert_eq!(read(install.join("sich.exe")), "v2");
        assert_eq!(read(install.join("sich.exe.old")), "v1");
    }

    #[test]
    fn apply_uses_numbered_backup_when_old_cannot_be_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path();
        fs::write(install.join("sich.exe"), "v1").unwrap();
        // A non-empty directory named `.old` cannot be removed with remove_file.
        fs::create_dir(install.join("sich.exe.old")).unwrap();
        fs::write(install.join("sich.exe.old").join("x"), "x").unwrap();
        let zip = install.join("u.zip");
        make_zip(&zip, &[("sich.exe", b"v2")]);
        apply_update_with(&zip, install, "sich.exe").unwrap();
        assert_eq!(read(install.join("sich.exe")), "v2");
        assert_eq!(read(install.join("sich.exe.old.1")), "v1");
    }

    #[test]
    fn apply_rolls_back_when_a_later_step_fails() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path();
        fs::write(install.join("a.dll"), "a-old").unwrap();
        fs::write(install.join("sich.exe"), "exe-old").unwrap();
        // `z.dll` is a directory in the install dir, so installing it fails last (sorted order).
        fs::create_dir(install.join("z.dll")).unwrap();
        let zip = install.join("u.zip");
        make_zip(&zip, &[("sich.exe", b"exe-new"), ("a.dll", b"a-new"), ("z.dll", b"z-new")]);

        assert!(apply_update_with(&zip, install, "sich.exe").is_err());

        assert_eq!(read(install.join("a.dll")), "a-old");
        assert_eq!(read(install.join("sich.exe")), "exe-old");
        assert!(install.join("z.dll").is_dir());
        assert!(!install.join("a.dll.old").exists());
        assert!(!install.join("sich.exe.old").exists());
    }

    #[test]
    fn apply_rejects_traversal_and_subdirectories() {
        for bad in ["../evil.exe", "sub/x.dll", "..\\evil.exe", "C:evil.exe"] {
            let dir = tempfile::tempdir().unwrap();
            let install = dir.path().join("app");
            fs::create_dir(&install).unwrap();
            let zip = dir.path().join("u.zip");
            make_zip(&zip, &[("sich.exe", b"x"), (bad, b"evil")]);
            assert!(apply_update_with(&zip, &install, "sich.exe").is_err(), "{bad}");
            assert!(!dir.path().join("evil.exe").exists(), "{bad}");
            assert!(!install.join("sich.exe").exists(), "{bad}");
        }
    }

    #[test]
    fn apply_rejects_zip_without_required_file() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("u.zip");
        make_zip(&zip, &[("conpty.dll", b"x")]);
        let err = apply_update_with(&zip, dir.path(), "sich.exe").unwrap_err();
        assert!(format!("{err:#}").contains("sich.exe"));
        assert!(!dir.path().join("conpty.dll").exists());
    }

    #[test]
    fn apply_rejects_non_zip() {
        let dir = tempfile::tempdir().unwrap();
        let zip = dir.path().join("u.zip");
        fs::write(&zip, "not a zip").unwrap();
        assert!(apply_update_with(&zip, dir.path(), "sich.exe").is_err());
    }

    #[test]
    fn old_names_are_recognised() {
        for yes in ["sich.exe.old", "sich.exe.old.1", "a.old.23"] {
            assert!(is_old_name(yes), "{yes}");
        }
        for no in ["sich.exe", "old", "a.old.x", "a.old.", "sich.exe.older"] {
            assert!(!is_old_name(no), "{no}");
        }
    }

    #[test]
    fn cleanup_removes_old_files_and_stage_dir() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        for f in ["sich.exe", "sich.exe.old", "conpty.dll.old.2", "keep.txt"] {
            fs::write(d.join(f), "x").unwrap();
        }
        fs::create_dir_all(d.join(STAGE_DIR).join("staged")).unwrap();
        fs::write(d.join(STAGE_DIR).join("staged").join("f"), "x").unwrap();

        cleanup_old(d);

        assert!(d.join("sich.exe").exists());
        assert!(d.join("keep.txt").exists());
        assert!(!d.join("sich.exe.old").exists());
        assert!(!d.join("conpty.dll.old.2").exists());
        assert!(!d.join(STAGE_DIR).exists());
        cleanup_old(&d.join("missing"));
    }

    #[test]
    fn ensure_writable_succeeds_in_temp_dir_and_other_errors_are_not_permission() {
        let dir = tempfile::tempdir().unwrap();
        ensure_writable(dir.path()).unwrap();
        let file = dir.path().join("f");
        fs::write(&file, "x").unwrap();
        let err = ensure_writable(&file).unwrap_err();
        assert!(err.downcast_ref::<NoWriteAccess>().is_none());
    }

    #[test]
    fn current_version_parses() {
        let _ = current_version();
    }
}
