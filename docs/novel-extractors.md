# Novel extractors (design draft)

Saving web novels from public reading sites (Royal Road first) as EPUB
books, the way [WebToEpub](https://github.com/dteviot/WebToEpub) does in the
browser, but inside Eddy's link checks and downloads.

Status: Royal Road, AO3, Wattpad, Sufficient Velocity and Wuxiaworld
implemented (engine, preview, download, Books settings, library and updates).
Scribble Hub, SpaceBattles (its threads; its forum index loads), Webnovel,
FanFiction.net and NovelUpdates are behind Cloudflare's browser check (a 403
"Just a moment" page), which only a WebView could pass; left for later.

## What WebToEpub does, and what carries over

WebToEpub has one `Parser` base class and ~430 site parsers registered by
host name in a `ParserFactory`. A parser mostly answers four questions, each
with a sensible default in the base class:

| Question | WebToEpub hook | Royal Road's answer |
|---|---|---|
| Which chapters? | `getChapterUrls(dom)` | the `window.chapters = [...]` JSON on the fiction page, with the old `table#chapters` as fallback |
| Where's the text? | `findContent(dom)` | `div.chapter-inner`'s parent |
| What's junk? | `preprocessRawDom`, `removeUnwantedElementsFromContentElement` | hidden watermark paragraphs, author notes (optional), images without `src` |
| Book details? | `extractTitle/Author/Description/Language`, `findCoverImageUrl` | `og:title`, `og:image`, the description block |

Everything else (fetching with retries and rate limits, cleaning HTML,
collecting images, packing the EPUB) is shared. That split is what to copy:
**most sites are a handful of CSS selectors; a few need code.**

What doesn't carry over: WebToEpub runs in the browser, so it gets cookies,
Cloudflare clearance and a DOM for free. Eddy fetches with reqwest, so it
needs an HTML parser and has to live with sites that only open in a browser.

## Checked on Royal Road (2026-10-22)

- A bare request gets Cloudflare's "Just a moment…" page (403). It loads
  (200) with a browser user agent, `Accept` and `Accept-Language`, **and**
  the request written the way browsers write HTTP/1.1: `Host` as the first
  header, header names in title case. reqwest put `Host` last and wrote
  names in lowercase, and was refused even with the right headers (the TLS
  library made no difference: rustls and OpenSSL were both let in once the
  headers were right). Every page behind Cloudflare loads its
  `challenge-platform` script, so that alone doesn't mean a challenge.
- The fiction page has `window.chapters`: 109 entries for *Mother of
  Learning*, each with `id, volumeId, title, slug, date, order, visible,
  subscriptionTiers, isUnlocked, url`. `isUnlocked`/`subscriptionTiers`
  mark paid early-access chapters, which are skipped.
- Chapter text is in `div.chapter-inner.chapter-content`; author notes in
  `div.author-note-portlet`.
- Watermarks: a `<style>` block hides a random class
  (`.cmRhYjQ2… { display: none }`), applied to a paragraph inside the
  chapter. The cleaner drops any element whose class a page `<style>` hides.

## Shape in Eddy

```
src/novels/
  mod.rs        Book, ChapterRef, Chapter; the site registry; probe()/download()
  fetch.rs      polite fetching: browser headers, one request at a time per
                site, a delay between chapters, retries on 429/503,
                Cloudflare detection, an on-disk chapter cache
  clean.rs      the shared cleanup pipeline (HTML → clean XHTML)
  epub.rs       the EPUB packer
  sites/
    royalroad.rs
    scribblehub.rs ...
```

### Data

```rust
pub struct Book {
    pub site: Site,
    pub url: String,
    pub title: String,
    pub author: Option<String>,
    pub description: Option<String>, // HTML, cleaned
    pub cover: Option<String>,       // image URL
    pub language: Option<String>,
    pub tags: Vec<String>,
    pub chapters: Vec<ChapterRef>,
}

pub struct ChapterRef {
    pub url: String,
    pub title: String,
    pub volume: Option<String>, // starts a new section in the table of contents
    pub locked: bool,           // paid / early access: listed, not fetched
}

pub struct Chapter {
    pub title: String,
    pub xhtml: String,          // cleaned body
    pub images: Vec<String>,    // URLs the body refers to
}
```

### Sites: a declarative rule, plus hooks for when selectors aren't enough

Most of WebToEpub's parsers only override selectors, so a site is a value,
not a type:

```rust
pub struct SiteRule {
    pub site: Site,                    // an enum, like extractors::Service
    pub hosts: &'static [&'static str],
    pub chapter_links: &'static str,   // CSS selector on the book page
    pub content: &'static str,         // CSS selector on a chapter page
    pub chapter_title: Option<&'static str>,
    pub remove: &'static [&'static str], // extra junk selectors
    pub author_notes: Option<&'static str>,
    pub title: &'static str, pub author: Option<&'static str>,
    pub cover: Option<&'static str>, pub description: Option<&'static str>,
}
```

Sites that need code (Royal Road's JSON chapter list, paged chapter lists,
APIs like Wattpad's, AO3's own EPUB download) get hooks, as a `match` on
`Site` like the media extractors already do, rather than trait objects:
async functions in traits aren't object-safe, and an enum keeps every site
in one list.

```rust
async fn book(site: Site, url: &str) -> Result<Book, String> {
    match site {
        Site::RoyalRoad => royalroad::book(url).await,
        other => generic::book(rule(other), url).await, // selectors only
    }
}
```

### Shared pipeline

1. **Fetch** (`fetch.rs`): browser headers (what got past Cloudflare
   above), the user's `cookies.txt` for that site (already in Settings →
   Downloads), one request at a time per site, about a second between
   chapters (Royal Road throttles), retries with backoff on 429/503.
   Cloudflare's challenge page is recognised and reported as "This site
   only opens in a browser" instead of being packed as a chapter.
2. **Cache**: each fetched chapter is kept as cleaned XHTML under
   `app_data/novels/<site>/<book id>/`, so an interrupted download
   resumes, and updating a book later only fetches new chapters.
3. **Clean** (`clean.rs`):
   - drop `script`, `style`, `iframe`, forms, ads, share buttons, and
     previous/next chapter links;
   - drop anything a page `<style>` hides (Royal Road's watermarks);
   - optionally drop author notes;
   - drop empty images, keep `p, h1–h6, em, strong, blockquote, hr, img,
     table, ul/ol`;
   - make it well-formed XHTML, which EPUB readers insist on.
4. **Images**: download each once, deduplicate, rename to
   `images/0001.jpg`, and rewrite the `src`s; the cover too. Options
   (below) can skip some or fetch smaller ones.
5. **Pack** (`epub.rs`): EPUB 3 with an EPUB 2 `toc.ncx` for older
   readers.
   - `mimetype` first and stored uncompressed;
   - `content.opf` with title, author, language, description, source URL,
     tags and cover;
   - `nav.xhtml`, with volumes as sections;
   - an information page (source, description, date saved).
   - Uses the `zip` crate, already a desktop dependency; it also builds for
     Android.

### In the app

- **Library**: Eddy remembers each saved book (site, book id, URL, title,
  chapter count, the EPUB's path, options used) in its database. Opening a
  book link that's already saved says so in the preview, with "Update"
  instead of "Download".
- **Updating**: fetches the chapter list again, downloads only the new
  chapters (the rest come from the chapter cache), and writes the EPUB
  again in the same place, so readers pick up the new version.
- **Link check**: `links::probe()` asks the novel registry first, by host,
  like the native media extractors. A match becomes a new `LinkBackend.Book`
  with file type Book: title, author, cover thumbnail, "109 chapters".
- **Preview**:
  - a chapter range ("1–109", or from/to pickers; locked chapters greyed
    out);
  - an "Author notes" switch;
  - image options (see Images);
  - a file name (`<title> - <author>.epub`).
- **Download**: a job like the media ones (`downloader::media`'s
  `begin/progress/end`), with progress counted in chapters fetched out of
  the total. The finished `.epub` goes to the download folder (staged and
  copied into picked folders like other downloads); the notification names
  the file.
- **"Download now"** with Auto: every chapter, author notes kept.

### Images

EPUB readers vary a lot with images, and novels' images (author-note
banners, Discord links, reaction GIFs) are often what makes a book big.
Three options in the preview, saved as defaults:

- **Images**: on or off. Off replaces each with its alt text, if any.
- **Skip images in author notes**: author notes stay but their images go
  (banners, "support me" buttons). Works with author notes kept or
  dropped.
- **Small images**: asks for a smaller version instead of the original,
  from the same server, without downloading the full one:
  - `srcset`/`data-srcset`: the smallest candidate at least 480 px wide;
  - known image hosts' size parameters: WordPress/Jetpack `?w=480`, Imgur's
    `m`/`l` suffixes (`abc.jpg` → `abcl.jpg`), Discord's `?width=480`,
    Royal Road's CDN size variants;
  - otherwise the original (Eddy doesn't re-encode images, so there's no
    image codec to add).

**GIFs** (the URL patterns below are from memory; each gets checked against
real links when it's built): readers don't play them, so instead of downloading an animation
to show its first frame, ask for a still:

- Imgur: `abc.gif` → `abc.jpg` (or `abcl.jpg` with Small images).
- Giphy: the `_s.gif` / `480w_s.jpg` still variants.
- Tenor: a still of the same media id (format to check when building it).
- A `poster` or a `<noscript>`/`data-src` still next to it on the page.
- Anything else: the original GIF, which readers show as a still (its
  first frame) anyway.

### New dependency

An HTML parser with CSS selectors: [`scraper`](https://crates.io/crates/scraper)
(html5ever + selectors, what most Rust scrapers use). It adds a fair bit to
the binary. The alternative, `tl` (much smaller), has weaker selector
support. Worth measuring the APK size before picking.

### Tests

Each site gets saved HTML fixtures (a book page, a chapter, a page with a
watermark) and parsing tests that run in the Linux CI job, without network.
Sites change their markup often, so these are what tell us a parser broke.

## First sites

1. **Royal Road**: checked above.
2. **Scribble Hub**: similar, with a paged chapter list (AJAX).
3. **AO3**: offers its own EPUB per work, so the "extractor" just finds
   that link.
4. **Wattpad**: JSON API for parts.

Later: Webnovel/NovelUpdates-listed translation sites, FanFiction.net
(hard Cloudflare; probably "only opens in a browser").

## Rules

- Public chapters only: paid, early-access and login-locked chapters are
  listed as locked and skipped, never worked around.
- Polite: one request at a time per site, delays, and back off on 429.
  These are small sites run by readers and authors.

## Decided

- EPUB only to start.
- Eddy keeps a library of saved books, and updates them with new
  chapters.

## Open question: where books live in the app

Getting a book in is the same either way: a novel link shared or pasted
goes through the preview like any other link. The question is where saved
books show up, so they can be updated.

- **A. In the downloads list.** A book is a finished download with a book
  icon. Tapping it opens the EPUB; its menu (or the selection actions)
  gets "Check for new chapters". Nothing new to learn, but books sit among
  videos and files, and checking every book at once has no obvious place.
- **B. A Books tab.** Its own tab next to the download lists: covers,
  titles, "12 new chapters" badges, "Update all", per-book settings. The
  clearest for a reading library, but a new screen to build and a fourth
  tab.
- **C. Both, in steps.** A first, since it needs no new screen; B later if
  the library grows past a handful of books.

Recommendation: C.
