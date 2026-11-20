# Releasing Sich

Releases are published to GitHub Releases of `kvxshenyjbetxn/sich` from a local machine.

## Cut a release

Prerequisites: clean working tree on `master`, `gh auth login` done, Rust toolchain installed.

```powershell
pwsh scripts/release.ps1 -Version 0.1.0 -Notes "What changed"
pwsh scripts/release.ps1 -Bump patch            # computes the next version, auto-generated notes
pwsh scripts/release.ps1 -Version 0.1.0 -DryRun # build + zip + sha256 only, nothing published
```

The script checks preconditions (clean tree, `master`, `gh` auth, tag `v<ver>` free locally and on
origin, version greater than current), bumps `[workspace.package] version` in `Cargo.toml`, updates
`Cargo.lock`, runs `cargo check` and `cargo test`, builds with `SICH_RELEASE=1`, packs `dist/`,
then commits `release v<ver>`, tags, pushes and creates the GitHub release. With `-DryRun`,
`Cargo.toml` and `Cargo.lock` are restored afterwards.

Helper functions live in `scripts/release-lib.ps1`; test them with `pwsh scripts/release.tests.ps1`.

## Asset format (the in-app updater depends on it)

- Git tag: `v<semver>` (e.g. `v0.1.0`).
- Asset `sich-<ver>-windows-x64.zip` with exactly these files at the zip root (no folder):
  `sich.exe`, `sich-cli.exe`, `conpty.dll`, `OpenConsole.exe`.
- Asset `sich-<ver>-windows-x64.zip.sha256` in `sha256sum` format:
  `<lowercase hex>  sich-<ver>-windows-x64.zip` followed by a single LF.

## Updater gating

Only builds made with `SICH_RELEASE=1` (the app reads `option_env!("SICH_RELEASE")`) check for
updates. Normal dev builds never do. Never set `SICH_RELEASE` outside the release script.
