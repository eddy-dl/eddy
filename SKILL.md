---
name: eddy
description: How to work on Eddy, the Rust + Slint download manager for Android — checking that it builds, looking at the UI, translations, logs, and the project's conventions. Use for any change to this repository.
---

# Working on Eddy

Eddy is a Rust app with a Slint UI (`ui/`), for Android, Windows and Linux. On Android it's a
library (`src/`, built as `libeddy.so`) inside a small Kotlin app (`platforms/android/`) that
hosts it: the download service, notifications,
the browser check (`CloudflareActivity`), the hidden page fetcher (`PageFetcher`) and Eddy's
browser (`BrowserActivity`). Kotlin talks to Rust through `EddyNative` (JNI, in
`src/platform/android.rs`).

## Checking a change

- **Rust:** check against Android (most of the app's platform code is there):

  ```sh
  ANDROID_JAR=$ANDROID_SDK/platforms/android-36/android.jar ANDROID_PLATFORM=android-36 \
  SLINT_STYLE=material cargo ndk -t arm64-v8a -P 26 check --lib --release
  ```

  The release profile is LTO with one codegen unit: `check` skips that, so use `build` only when
  an APK is wanted. On a phone, run it niced on the small cores
  (`taskset -c 0-5 nice -n 19 …`). Code changed while a check runs: stop it and start again.
  The desktop app (`src/main.rs`, `src/platform/desktop.rs`) builds with plain
  `SLINT_STYLE=fluent cargo build --release`; CI builds Windows, Linux and Android.
- **Slint only:** no Rust build needed. Open it in the viewer, in both styles:

  ```sh
  slint-viewer --style material ui/app.slint
  slint-viewer --style fluent ui/app.slint
  ```

  A page or component on its own: `--component Name`, and `--screenshot out.png --size WxH` to
  look at it. A "BackendSelector" message is normal.
- **Kotlin:** only Gradle builds it (`platforms/android`, `./gradlew assembleRelease`).

## Conventions

- **Comments** say what something is for, in plain sentences, as the surrounding code does.
  Match the file's density; don't narrate the change.
- **UI text** goes through `@tr(...)`, and every new string gets its French entry in
  `lang/fr/LC_MESSAGES/eddy-ui.po`. `python3 tools/sync_po.py lang/fr/LC_MESSAGES/eddy-ui.po`
  lists what's missing. French uses `’` and a space before `:`.
- **Look:** Material 3 (Expressive). Explanatory lines in sheets and settings use the `Note`
  component (an (i) and grey text), not plain grey `Text`. Settings rows are `SettingsCard` /
  `SwitchRow` groups (`first` / `last` default to true, so set them inside a group), with
  `SectionHeader` above a group.
- **Slint 1.18 gotchas:**
  - Wrapped text in layouts and Flickables measures wrong: use `WrappedText`.
  - A Flickable's content layout needs `width: parent.width`.
  - Numbers turned into strings use the locale's decimal separator: anything parsed back (a
    `Path`'s `commands`) needs `.to-string-unlocalized()`.
  - `Timer` is unreliable for press timing: use an animated property (see `HoldTouchArea`).
- **Logs:** use `log::warn!` / `log::error!` for anything worth looking into later; only
  warnings and errors reach the log files (`src/logs.rs`, which file by module). Info goes to
  logcat only.
- **Bot checks:** sites behind Cloudflare or Anubis are passed by the user in a WebView, and
  Cloudflare's pages are then read through the hidden WebView. Don't make Eddy's own requests
  imitate a browser's fingerprint, and don't solve checks in code.

## Commits

- Commits on `main` carry no `Co-Authored-By` lines. Feature branches go in as one squash
  commit with a hand-written message (pass the body, as GitHub copies the trailers otherwise).
- Never commit keystores or passwords (`*.keystore` and `*.jks` are ignored).
