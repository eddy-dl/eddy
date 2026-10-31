# Eddy

A download manager for Android, written in Rust with a [Slint](https://slint.dev) interface.
Paste or share a link: Eddy works out what it is, shows a preview, and downloads it.

## What it downloads

- **Files**: plain links, with several connections, pause and resume, through a patched
  [aria2_rust](https://github.com/balovess/aria2_rust).
- **Torrents and magnet links**, picking which files to keep.
- **Videos and music** through yt-dlp, with Eddy's own extractors for TikTok (the best quality,
  others to pick from), Instagram and Pinterest.
- **Google Drive** and **MEGA** files and folders (MEGA's decrypted in place).
- **Web novels as EPUB books**: Royal Road, AO3, Wattpad, Sufficient Velocity, SpaceBattles,
  FanFiction.net, FictionPress and Wuxiaworld. Pick chapters, keep author notes and images or
  not, and check for new chapters later. Sites behind Cloudflare or Anubis are passed in a
  browser check. See [docs/novel-extractors.md](docs/novel-extractors.md).
- **Pages whose file only comes from a click**, in Eddy's own browser.

Downloads go to `Download/Eddy` (by type, if you like) or a folder you pick.
English and French.

## Building

You need Rust (edition 2024), the Android NDK and SDK (API 36), and
[cargo-ndk](https://github.com/bbqsrc/cargo-ndk).

```sh
# The Rust library, for 64-bit ARM phones (the APK takes it from target/)
cargo ndk -t arm64-v8a -P 26 build --lib --release

# The APK
cd platforms/android
./gradlew assembleRelease
```

The release build is signed with the keystore named by `KEYSTORE_FILE`, `KEYSTORE_PASSWORD`,
`KEY_ALIAS` and `KEY_PASSWORD`; the `releasetest` build type uses a debug keystore at the
repository's root (`debug.keystore`, not committed).

Android 8 (API 26) or newer.

To look at the interface without building the app, open it in `slint-viewer`:

```sh
slint-viewer --style material ui/app.slint
```

## Layout

| Path | What's there |
|---|---|
| `src/` | The app: links and previews (`links.rs`), the download list (`downloads.rs`), the downloaders (`downloader/`), extractors (`extractors/`), web novels (`novels/`), platform code (`platform/`) |
| `ui/` | The Slint interface: `app.slint`, components, settings pages, icons |
| `platforms/android/` | The Android app: Gradle, the download service, the browser check and Eddy's browser (Kotlin) |
| `patches/` | Patched crates: aria2_rust, and Slint's Android backend |
| `lang/` | Translations (gettext `.po`) |
| `tools/` | The icon and wordmark generators, and `sync_po.py` for the translations |
| `docs/` | Design notes |

## Logs

Warnings and errors are written to Eddy's own storage. Settings → Advanced → Debug can keep them
in `Download/Eddy/logs` instead, or share them for a bug report. They list the links you
download, so look at them before sending them anywhere.
