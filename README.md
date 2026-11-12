# Eddy

[![Latest release](https://img.shields.io/github/v/release/eddy-dl/eddy)](https://github.com/eddy-dl/eddy/releases/latest)

A download manager for Android, iOS, Windows, macOS and Linux, written in Rust with a [Slint](https://slint.dev)
interface.
Paste or share a link: Eddy works out what it is, shows a preview, and downloads it.

<p>
  <img src="docs/screenshots/mobile-downloads.png" alt="Downloads on a phone" width="200">
  <img src="docs/screenshots/mobile-downloads-dark.png" alt="Downloads on a phone, dark theme" width="200">
  <img src="docs/screenshots/mobile-settings.png" alt="Settings on a phone" width="200">
  <img src="docs/screenshots/mobile-settings-dark.png" alt="Settings on a phone, dark theme" width="200">
</p>
<p>
  <img src="docs/screenshots/desktop-downloads.png" alt="Downloads on desktop" width="400">
  <img src="docs/screenshots/desktop-downloads-dark.png" alt="Downloads on desktop, dark theme" width="400">
</p>
<p>
  <img src="docs/screenshots/desktop-settings.png" alt="Settings on desktop" width="400">
  <img src="docs/screenshots/desktop-settings-dark.png" alt="Settings on desktop, dark theme" width="400">
</p>

## What it downloads

- **Files**: plain links, with several connections, pause and resume, through a patched
  [aria2_rust](https://github.com/balovess/aria2_rust).
- **Torrents and magnet links**, picking which files to keep.
- **Videos and music** through yt-dlp, with Eddy's own extractors for TikTok (the best quality,
  others to pick from), Instagram and Pinterest. A Spotify track's link gets the same song from
  YouTube Music: Spotify says which song it is, and yt-dlp finds it there. On Android, yt-dlp comes from
  [YTDLnis](https://github.com/deniscerri/ytdlnis)'s helper apps
  ([ytdlnis-packages](https://github.com/deniscerri/ytdlnis-packages)), which Eddy can fetch
  for you to install.
- **Galleries** (image boards, art and photo sites…) through
  [gallery-dl](https://github.com/mikf/gallery-dl), offered next to yt-dlp in the preview. On
  Android it runs on YTDLnis's Python, with gallery-dl itself fetched from PyPI; on desktop it's
  gallery-dl's own build, or one on your PATH.
- **Google Drive** and **MEGA** files and folders (MEGA's decrypted in place).
- **Web novels as EPUB books**: Royal Road, AO3, Wattpad, Sufficient Velocity, SpaceBattles,
  FanFiction.net, FictionPress, Wuxiaworld, NovelPing, NovelBin and Ranobes. Pick chapters, keep
  author notes and images or not, and check for new chapters later. Sites behind Cloudflare or
  Anubis are passed in Eddy's browser. See [docs/novel-extractors.md](docs/novel-extractors.md).
- **Pages whose file only comes from a click** (a countdown, a captcha, a site that turns away
  anything but a visit), in Eddy's own browser, and **downloads sent from your browser** with its
  extension: see [Eddy's browser and your browser](#eddys-browser-and-your-browser).

On iOS (a first build, sideloaded with AltStore, SideStore or Sideloadly), yt-dlp and gallery-dl
run in the app's own Python, with ffmpeg and a JavaScript engine built in, as iOS lets an app
start no other programs. It can't be on the App Store: Apple's review guidelines don't allow
torrent clients or saving media from sites like YouTube (5.2.3), nor an app downloading code,
as updating yt-dlp does (2.5.2).

Downloads go to `Download/Eddy` (by type, if you like) or a folder you pick.
Settings → General can back up your settings and download list to a file (in the download
folder's `Backups`; not the files themselves, nor cookies) and restore one, on the same device
or another: what's already there is kept.
Settings → About looks for a new Eddy on GitHub (as it opens, once a day at most, unless you turn
that off; pre-releases only if you ask) and downloads it when you say: Android's installer takes
its APK, and on Windows, Linux and macOS Eddy swaps its own program (or app) for the new one and
starts again. On iOS it opens the release's page.
English and French. The French translation was written by an AI model, not a translator, and
hasn't been checked by a native speaker: corrections are welcome.

## Eddy's browser and your browser

Some files don't come from a plain request: the site checks for bots first, or the file only
comes from clicking a button on its page, or the site only lets it go to a browser that visited
that page. Eddy has two ways round that: a browser of its own, and an extension that sends
downloads from yours.

### Eddy's browser

Eddy opens a browser of its own for three things:

- **Bot checks**: a site behind Cloudflare's or Anubis's check ("Just a moment…") is opened in
  it. Most checks pass on their own; when one wants you (a checkbox), the browser shows it to you.
  Eddy then uses the pass it left, or, for a while, reads the site's pages through the browser,
  unseen. The novel sites need this most.
- **Reading pages unseen**, for sites that turn Eddy's own requests away even after a check.
- **Open in browser**, in a link's preview: the page opens for you to click through (a countdown,
  a captcha, a "Download" button), and the download it starts isn't saved by the browser: Eddy
  takes it over and downloads the file itself, with the browser's cookies, its user agent and the
  page as the referrer, so the site sees the same visitor. A link that answers 401 or 403 says so
  in the preview, with the site's own message when it gives one, and suggests Open in browser.

What it is, on each system:

| System | Eddy's browser | What it needs |
|---|---|---|
| Android | Android's WebView, in Eddy | Nothing |
| iOS | A WKWebView over the app, with a Close bar | Nothing (iOS 14.5 or newer to catch downloads) |
| macOS | `eddy-webview`, a helper program (WKWebView) | Nothing: it's inside Eddy.app |
| Windows | `eddy-webview` (WebView2) | The helper, and Microsoft Edge WebView2 (part of Windows 10 and 11) |
| Linux | `eddy-webview` (WebKitGTK) | The helper, and WebKitGTK (`libwebkit2gtk-4.1`, from your distribution) |

On desktop the browser is a program of its own, so Eddy itself never needs WebView2 or WebKitGTK.
Eddy starts it the first time a site needs it, and it quits after two minutes with nothing to do.
On Windows and Linux, get it in **Settings → Downloads → External packages → Eddy's browser**:
Eddy downloads the helper from its own release for the version you're running, checks it against
the SHA-256 GitHub gives for the file, and keeps it in its own folder; it's offered again when a
new Eddy comes with a new one. Eddy also finds it next to its own program (as built, or as the
releases name it, `eddy-webview-linux-x86_64` and so on), at the path in `EDDY_WEBVIEW`, or on
your `PATH`. The helper keeps its cookies in Eddy's data folder, apart from your own browser's.

What it can't hand over:

- Files a page makes itself, in the browser (`blob:` and `data:` links): they only exist there.
- Downloads that come from sending a form (a POST): Eddy asks for the link again with a plain GET,
  which some sites refuse.
- Links that only work once: the browser's request was the one.
- A page that opens a new window opens it in place when it's on the same site; one on another
  site (an ad, usually) isn't opened.

### Browser extension

`extension/` (also `eddy-browser-extension.zip` with each release) sends downloads from Chrome,
Edge, Brave, Vivaldi or Firefox to Eddy on Windows, macOS and Linux, with your browser's cookies,
user agent and the page they came from: a file that only downloads for someone logged in, or who
visited its page, comes through Eddy as it would through the browser.

- Right-click a link, picture, video or page for **Download with Eddy**. It goes to Eddy's
  preview, already knowing its name and size (Eddy doesn't check the link again: without the
  cookies, that's the request a site may refuse).
- In the extension's options, **Send the browser's downloads to Eddy** takes over the browser's own
  downloads: each is paused, handed to Eddy, and dropped from the browser once Eddy has it. When
  Eddy can't be reached, the browser carries on with it.

Setting it up:

1. In Eddy, click **Settings → General → Browser extension**. It tells each of these browsers it
   finds where Eddy is (do it again if Eddy moves).
2. Load the extension:
   - Chrome and the browsers built on it: open `chrome://extensions`, turn on **Developer mode**,
     click **Load unpacked**, and pick the `extension` folder.
   - Firefox (121 or newer): open `about:debugging` → **This Firefox** → **Load Temporary
     Add-on…**, and pick `extension/manifest.json`. It isn't signed by Mozilla yet, so it stays
     until Firefox restarts. Then allow it on all sites in its **Permissions**.

How it reaches Eddy: through the browser's native messaging. Step 1 writes a small file naming
Eddy as the extension's host, `dev.pah.eddy` (on Windows, with registry keys under
`HKCU\Software\…\NativeMessagingHosts` pointing at it). When you send a download, the browser
starts Eddy with the download's details; that Eddy hands them to the Eddy already open, or opens
one, and quits.

When it doesn't work, the extension's toolbar button says why when you point at it. Common
causes:

- Step 1 wasn't done, or Eddy has moved since.
- The browser is a Flatpak or a Snap (Ubuntu's Firefox is a Snap): its sandbox doesn't let it
  start other programs. A browser from your distribution's packages, or the browser's own, works.
- The download is a `blob:` or `data:` link, which only the browser can save.

There's no extension for Safari yet, and the extension's own text is in English only.

## Building

For Android, you need Rust (edition 2024), the Android NDK and SDK (API 36), and
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

On Windows and Linux, the desktop app is a plain Cargo build:

```sh
SLINT_STYLE=fluent cargo build --release --bin eddy-desktop
```

Links and files given on its command line (`eddy-desktop <link>… <file.torrent>…`, or the
releases' `eddy-linux-x86_64` and so on) open in the preview, in the window already open if
there is one. Settings → General can make it what Windows and Linux open magnet links and
`.torrent` files with.

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
| `platforms/ios/` | The iOS app: its Xcode project (XcodeGen) and build script; CI fetches Python, ffmpeg and QuickJS-ng into it |
| `patches/` | Patched crates: aria2_rust, and Slint's Android backend |
| `webview/` | `eddy-webview`, Eddy's browser on desktop, and the browser's jobs it shares with the iOS app |
| `extension/` | The browser extension (Chrome, Firefox), which hands downloads to Eddy |
| `lang/` | Translations (gettext `.po`) |
| `tools/` | The icon and wordmark generators, and `sync_po.py` for the translations |
| `docs/` | Design notes |

## Logs

Warnings and errors are written to Eddy's own storage. Settings → Advanced → Debug can keep them
in `Download/Eddy/logs` instead, or share them for a bug report. They list the links you
download, so look at them before sending them anywhere.

## Credits

- [Palladium](https://github.com/tfourj/Palladium) by [tfourj](https://github.com/tfourj): Eddy's
  iOS port follows its approach: yt-dlp and gallery-dl in an embedded Python, yt-dlp's ffmpeg
  calls answered in-process, and JavaScriptCore with QuickJS as the JavaScript runtime for
  YouTube's challenges.
- [SwiftFFmpeg-iOS](https://github.com/tfourj/SwiftFFmpeg-iOS), also by tfourj: the ffmpeg and
  ffprobe Eddy's iOS app runs.
- [Python-Apple-support](https://github.com/beeware/Python-Apple-support) (BeeWare): the Python
  in the iOS app.
- [QuickJS-ng](https://github.com/quickjs-ng/quickjs): the iOS app's JavaScript fallback.
- [yt-dlp](https://github.com/yt-dlp/yt-dlp) and [gallery-dl](https://github.com/mikf/gallery-dl),
  which do the work for most sites; [aria2_rust](https://github.com/balovess/aria2_rust) for
  files and torrents; [YTDLnis](https://github.com/deniscerri/ytdlnis)'s packages on Android.

## License

Eddy is free software, under the GNU General Public License, version 3 or (at your option) any
later version: see [LICENSE](LICENSE). The patched crates in `patches/` keep their own licenses
(aria2_rust is GPL-3.0-or-later too; Slint's Android backend keeps Slint's).
