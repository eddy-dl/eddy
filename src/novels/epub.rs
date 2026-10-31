//! Packs a book as EPUB 3, with an EPUB 2 table of contents (toc.ncx) for
//! older readers: `mimetype` first and stored, the package document, the
//! navigation (volumes as sections), an information page (description,
//! source, when it was saved), the chapters, and the images.

use super::Book;
use super::clean::escape;
use super::images::Image;
use std::io::{Cursor, Write};
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

pub struct Chapter {
    pub title: String,
    pub volume: Option<String>,
    pub xhtml: String,
}

const STYLE: &str = "body { margin: 0 4%; line-height: 1.5; }
p { margin: 0 0 0.8em 0; }
h1 { font-size: 1.4em; margin: 1em 0; }
img { max-width: 100%; height: auto; }
table { border-collapse: collapse; }
td, th { border: 1px solid #888; padding: 0.2em 0.4em; }
aside.author-note { margin-top: 2em; padding: 0.6em 0.8em; border-left: 3px solid #888; font-size: 0.92em; }
.image-alt { font-style: italic; opacity: 0.7; }
.info dt { font-weight: bold; margin-top: 0.6em; }
";

/// The book as EPUB bytes. `cover` is the cover image's bytes and type;
/// `part`: the book comes in parts (updates in files of their own), and
/// this is the nth, so readers list them together, in order, as a series.
/// The same chapters always make the same bytes (no dates in it), so a
/// reader that knows books by their contents knows it again.
pub fn pack(book: &Book, chapters: &[Chapter], cover: Option<(Vec<u8>, String)>, images: Vec<Image>, part: Option<usize>) -> Result<Vec<u8>, String> {
    // A later part is a book of its own: its title says which part, and its
    // id isn't the first part's.
    let series = part.map(|n| (book.title.clone(), n));
    let mut parted;
    let book = match part {
        Some(n) if n > 1 => {
            parted = book.clone();
            parted.title = format!("{} (part {n})", book.title);
            parted.id = format!("{}-part{n}", book.id);
            &parted
        }
        _ => book,
    };
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut add = |name: &str, bytes: &[u8], compress: bool| -> Result<(), String> {
        zip.start_file(name, if compress { deflated } else { stored }).map_err(|e| e.to_string())?;
        zip.write_all(bytes).map_err(|e| e.to_string())
    };

    // Readers find the format by `mimetype` being first and uncompressed.
    add("mimetype", b"application/epub+zip", false)?;
    add(
        "META-INF/container.xml",
        br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
        true,
    )?;
    add("OEBPS/style.css", STYLE.as_bytes(), true)?;

    let cover_path = cover.as_ref().map(|(_, t)| format!("images/cover.{}", image_extension(t)));
    if let (Some((bytes, _)), Some(path)) = (&cover, &cover_path) {
        add(&format!("OEBPS/{path}"), bytes, false)?;
        add("OEBPS/text/cover.xhtml", page("Cover", &format!("<div style=\"text-align: center\"><img src=\"../{path}\" alt=\"Cover\"/></div>")).as_bytes(), true)?;
    }
    add("OEBPS/text/info.xhtml", page(&book.title, &info(book)).as_bytes(), true)?;
    for (i, chapter) in chapters.iter().enumerate() {
        let body = format!("<h1>{}</h1>\n{}", escape(&chapter.title), chapter.xhtml);
        add(&format!("OEBPS/text/{}", chapter_file(i)), page(&chapter.title, &body).as_bytes(), true)?;
    }
    for image in &images {
        // Already compressed.
        add(&format!("OEBPS/{}", image.path), &image.bytes, false)?;
    }
    add("OEBPS/nav.xhtml", nav(book, chapters, cover_path.is_some()).as_bytes(), true)?;
    add("OEBPS/toc.ncx", ncx(book, chapters).as_bytes(), true)?;
    add("OEBPS/content.opf", opf(book, chapters, cover.as_ref().map(|(_, t)| t.as_str()), &images, series.as_ref()).as_bytes(), true)?;

    zip.finish().map(|c| c.into_inner()).map_err(|e| e.to_string())
}

fn chapter_file(index: usize) -> String {
    format!("chapter{:04}.xhtml", index + 1)
}

fn image_extension(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpg",
    }
}

/// An XHTML page around `body`.
fn page(title: &str, body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>{}</title><link rel="stylesheet" type="text/css" href="../style.css"/></head>
<body>
{body}
</body>
</html>"#,
        escape(title)
    )
}

/// The information page: title, author, description, tags, source.
fn info(book: &Book) -> String {
    let mut html = format!("<h1>{}</h1>\n", escape(&book.title));
    if let Some(author) = &book.author {
        html += &format!("<p>by {}</p>\n", escape(author));
    }
    if let Some(description) = &book.description {
        html += &format!("<div class=\"description\">{description}</div>\n");
    }
    html += "<dl class=\"info\">\n";
    if !book.tags.is_empty() {
        html += &format!("<dt>Tags</dt><dd>{}</dd>\n", escape(&book.tags.join(", ")));
    }
    html += &format!("<dt>Source</dt><dd><a href=\"{0}\">{0}</a> ({1})</dd>\n", escape(&book.url), book.site.label());
    html += "<dt>Saved</dt><dd>with Eddy</dd>\n";
    html += "</dl>";
    html
}

fn nav(book: &Book, chapters: &[Chapter], has_cover: bool) -> String {
    let mut items = String::new();
    if has_cover {
        items += "<li><a href=\"text/cover.xhtml\">Cover</a></li>\n";
    }
    items += "<li><a href=\"text/info.xhtml\">Information</a></li>\n";
    // Chapters, grouped under their volumes.
    let mut open_volume = false;
    let mut current: Option<&str> = None;
    for (i, chapter) in chapters.iter().enumerate() {
        let volume = chapter.volume.as_deref();
        if volume.is_some() && volume != current {
            if open_volume {
                items += "</ol></li>\n";
            }
            items += &format!("<li><a href=\"text/{}\">{}</a><ol>\n", chapter_file(i), escape(volume.unwrap()));
            open_volume = true;
            current = volume;
        }
        items += &format!("<li><a href=\"text/{}\">{}</a></li>\n", chapter_file(i), escape(&chapter.title));
    }
    if open_volume {
        items += "</ol></li>\n";
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<head><title>{}</title></head>
<body>
<nav epub:type="toc" id="toc"><h1>Contents</h1><ol>
{items}</ol></nav>
</body>
</html>"#,
        escape(&book.title)
    )
}

fn ncx(book: &Book, chapters: &[Chapter]) -> String {
    let mut points = String::new();
    let mut order = 1;
    let mut point = |label: &str, src: &str| {
        let p = format!(
            "<navPoint id=\"p{order}\" playOrder=\"{order}\"><navLabel><text>{}</text></navLabel><content src=\"{src}\"/></navPoint>\n",
            escape(label)
        );
        order += 1;
        p
    };
    points += &point("Information", "text/info.xhtml");
    for (i, chapter) in chapters.iter().enumerate() {
        points += &point(&chapter.title, &format!("text/{}", chapter_file(i)));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
<head><meta name="dtb:uid" content="{}"/></head>
<docTitle><text>{}</text></docTitle>
<navMap>
{points}</navMap>
</ncx>"#,
        escape(&identifier(book)),
        escape(&book.title)
    )
}

fn opf(book: &Book, chapters: &[Chapter], cover_type: Option<&str>, images: &[Image], series: Option<&(String, usize)>) -> String {
    let mut metadata = format!(
        "<dc:identifier id=\"book-id\">{}</dc:identifier>\n<dc:title>{}</dc:title>\n<dc:language>{}</dc:language>\n<meta property=\"dcterms:modified\">{}</meta>\n<dc:source>{}</dc:source>\n<dc:publisher>{}</dc:publisher>\n",
        escape(&identifier(book)),
        escape(&book.title),
        escape(book.language.as_deref().unwrap_or("en")),
        // Required; fixed, so the same chapters make the same file.
        "2000-01-01T00:00:00Z",
        escape(&book.url),
        book.site.label(),
    );
    if let Some(author) = &book.author {
        metadata += &format!("<dc:creator>{}</dc:creator>\n", escape(author));
    }
    if let Some(description) = &book.description {
        // Plain text for the reader's library view.
        let text = regex::Regex::new(r"<[^>]+>").unwrap().replace_all(description, " ");
        metadata += &format!("<dc:description>{}</dc:description>\n", escape(text.split_whitespace().collect::<Vec<_>>().join(" ").trim()));
    }
    for tag in &book.tags {
        metadata += &format!("<dc:subject>{}</dc:subject>\n", escape(tag));
    }
    if let Some((name, index)) = series {
        // The series and its place in it, as EPUB 3 and Calibre write them
        // (readers' series shelves use one or the other).
        metadata += &format!(
            "<meta property=\"belongs-to-collection\" id=\"series\">{0}</meta>\n<meta refines=\"#series\" property=\"collection-type\">series</meta>\n<meta refines=\"#series\" property=\"group-position\">{index}</meta>\n<meta name=\"calibre:series\" content=\"{0}\"/>\n<meta name=\"calibre:series_index\" content=\"{index}\"/>\n",
            escape(name)
        );
    }

    let mut manifest = String::from(
        "<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n<item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n<item id=\"style\" href=\"style.css\" media-type=\"text/css\"/>\n<item id=\"info\" href=\"text/info.xhtml\" media-type=\"application/xhtml+xml\"/>\n",
    );
    let mut spine = String::new();
    if let Some(media_type) = cover_type {
        // Both ways of naming the cover: EPUB 3's property, EPUB 2's meta.
        manifest += &format!(
            "<item id=\"cover-image\" href=\"images/cover.{}\" media-type=\"{media_type}\" properties=\"cover-image\"/>\n<item id=\"cover\" href=\"text/cover.xhtml\" media-type=\"application/xhtml+xml\"/>\n",
            image_extension(media_type)
        );
        metadata += "<meta name=\"cover\" content=\"cover-image\"/>\n";
        spine += "<itemref idref=\"cover\"/>\n";
    }
    spine += "<itemref idref=\"info\"/>\n";
    for i in 0..chapters.len() {
        manifest += &format!("<item id=\"c{i}\" href=\"text/{}\" media-type=\"application/xhtml+xml\"/>\n", chapter_file(i));
        spine += &format!("<itemref idref=\"c{i}\"/>\n");
    }
    for (i, image) in images.iter().enumerate() {
        manifest += &format!("<item id=\"i{i}\" href=\"{}\" media-type=\"{}\"/>\n", image.path, image.media_type);
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="book-id">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
{metadata}</metadata>
<manifest>
{manifest}</manifest>
<spine toc="ncx">
{spine}</spine>
</package>"#
    )
}

/// Stable per book, so an updated EPUB replaces the old one in readers'
/// libraries instead of showing up twice.
fn identifier(book: &Book) -> String {
    format!("urn:eddy:{}:{}", book.site.label().to_lowercase().replace(' ', ""), book.id)
}

