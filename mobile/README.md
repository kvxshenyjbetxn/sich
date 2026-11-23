# Sich phone app

Tauri 2 (Rust core + React/TypeScript/Vite WebView) client for the Sich gateway. Android now, iOS later (nothing Android-only in shared code). Spec: `docs/phone-mvp.md` (sections 2, 3, 5, 6).

Bundle id `dev.sich.remote`, product name `Sich`.

## Layout

- `src/` - React UI. Pure logic lives in `src/lib/*` (workspace tree, key bytes, i18n, image size math, connection labels, terminal helpers) and is covered by vitest (`src/lib/lib.test.ts`).
- `src-tauri/` - its own cargo workspace (NOT part of the root workspace). Depends on `../../crates/remote-proto` by path.
  - `hosts` (hosts.json), `keys` (phone iroh key), `backoff`, `conn` (ConnectionManager, the only owner of the connection), `commands` (Tauri commands), `ui` (events and binary frames).
  - `gen/android` - generated Android project (committed, build outputs ignored).

## Prerequisites

- Node 20+ and npm.
- Rust (stable, edition 2024) with the Android targets (`rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android`).
- Android SDK + NDK, and a JDK 17+ (Android Studio's bundled JBR works; the default `java` on PATH may be too old).

## Environment (PowerShell)

```powershell
$env:ANDROID_HOME = "$env:LOCALAPPDATA\Android\Sdk"
$env:NDK_HOME     = "$env:LOCALAPPDATA\Android\Sdk\ndk\28.2.13676358"
$env:JAVA_HOME    = "C:\Program Files\Android\Android Studio\jbr"
```

## Commands

```powershell
cd mobile
npm install

npm test                                   # vitest (pure TS logic)
npm run build                              # tsc + vite build into dist/
cd src-tauri; cargo test -j 8; cd ..       # Rust core tests (host target, in-process iroh)

# Debug APK for a phone (arm64):
npx tauri android build --debug --apk --target aarch64
adb install -r src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk

# Live dev on a connected device:
npx tauri android dev
```

Desktop smoke run of the UI (no camera, no Android): `npx tauri dev` (set `SICH_PHONE_RELAY=disabled` for LAN-only tests).

## Data passed between Rust and the WebView

- Events: `workspace` (JSON), `connection` (`{state: idle|connecting|connected|reconnecting|pair_pending|rejected, ...}`), `term_size`, `term_exited`, `hosts`, `gateway_error`.
- Terminal bytes use a per-subscription `Channel` with raw binary frames: `[0, cols_be16, rows_be16]` = Reset, `[1, ...bytes]` = Output. Image uploads send the bytes as the raw request body (file name in the `x-file-name` header, percent-encoded).

## Notes

- The phone's iroh secret key is a plain file in the app data dir (TODO: Android Keystore / iOS Keychain).
- Pairing needs the desktop confirmation (Sich gateway). Scan the QR code or paste the `sich://pair?...` link.
