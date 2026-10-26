//! Google Drive's shared files and folders, without an API key (as gdown
//! does it). A file's download link (drive.usercontent.google.com) answers
//! with the file itself or, for one too large to scan for viruses, a warning
//! page whose form leads to it. A folder lists itself in
//! `embeddedfolderview`: a plain page with every entry, sizes left out (a
//! one-byte request per file gets those). Google Docs, Sheets and Slides
//! have no file of their own and are saved as Office exports.

use super::http;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};

pub const NOT_SHARED: &str = "This Drive file isn't shared publicly: it needs a Google sign-in";
pub const FOLDER_NOT_SHARED: &str = "This Drive folder isn't shared publicly (or doesn't exist)";
pub const QUOTA: &str = "Google Drive: too many people have viewed or downloaded this file recently. Try again later";
pub const GONE: &str = "This Drive file doesn't exist (deleted, or the link is wrong)";

/// A folder's files are listed up to this many (and this many folders
/// looked into); the rest are left out.
pub const MAX_FILES: usize = 1000;
const MAX_FOLDERS: usize = 300;
/// Files whose size is asked for when a folder is listed.
const MAX_SIZED: usize = 300;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    File(String),
    Folder(String),
}

/// A Drive link's file or folder id. `open?id=` links can be either; they're
/// taken as files (`identify` asks Drive).
pub fn parse_link(url: &str) -> Option<Link> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.trim_start_matches("www.");
    if !matches!(host, "drive.google.com" | "docs.google.com" | "drive.usercontent.google.com") {
        return None;
    }
    let mut segments: Vec<&str> = parsed.path_segments()?.filter(|s| !s.is_empty()).collect();
    // A signed-in account's links: u/0/uc?id=…, u/1/open?id=…
    if segments.first() == Some(&"u") && segments.len() > 2 {
        segments.drain(..2);
    }
    let query_id = || parsed.query_pairs().find(|(k, _)| k == "id").map(|(_, v)| v.into_owned()).filter(|id| valid_id(id));
    // drive/folders/<id>, drive/u/0/folders/<id>, drive/mobile/folders/<id>
    if segments.first() == Some(&"drive") {
        let at = segments.iter().position(|s| *s == "folders")?;
        return segments.get(at + 1).filter(|id| valid_id(id)).map(|id| Link::Folder(id.to_string()));
    }
    match segments.as_slice() {
        ["folderview" | "embeddedfolderview"] => query_id().map(Link::Folder),
        // file/d/<id>/view, file/u/0/d/<id>
        ["file", rest @ ..] => {
            let at = rest.iter().position(|s| *s == "d")?;
            rest.get(at + 1).filter(|id| valid_id(id)).map(|id| Link::File(id.to_string()))
        }
        ["uc" | "open" | "download"] => query_id().map(Link::File),
        _ => None,
    }
}

fn valid_id(id: &str) -> bool {
    id.len() >= 10 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn can_handle(url: &str) -> bool {
    parse_link(url).is_some()
}

/// What a link is, asking Drive for `open?id=` links (which open folders
/// too): Drive redirects them to the folder's or the file's page.
pub async fn identify(url: &str) -> Option<Link> {
    let link = parse_link(url)?;
    let is_open = url::Url::parse(url).is_ok_and(|u| u.path().trim_end_matches('/') == "/open");
    let Link::File(id) = &link else {
        return Some(link);
    };
    if !is_open {
        return Some(link);
    }
    let location = redirect_of(&format!("https://drive.google.com/open?id={id}")).await.unwrap_or_default();
    Some(match parse_link(&location) {
        Some(folder @ Link::Folder(_)) => folder,
        _ => link,
    })
}

/// Where `url` redirects to, if it does.
async fn redirect_of(url: &str) -> Option<String> {
    let response = http::no_redirect_client().get(url).send().await.ok()?;
    response.headers().get("location")?.to_str().ok().map(str::to_string)
}

/// One of a folder's files.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub id: String,
    /// Inside the folder, through its subfolders ("sub/file.ext").
    pub path: String,
    /// Bytes, when known.
    pub size: Option<u64>,
    /// A Google Docs editors file, saved as an export: "document",
    /// "spreadsheets" or "presentation"; "" for a file of its own.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub native: String,
}

/// A Drive folder's download, handed from the preview to the downloader
/// (as JSON in `DownloadRequest::format`): the files picked. Empty: all of
/// them, listed again when it starts.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DriveRequest {
    pub files: Vec<Entry>,
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub name: String,
    pub entries: Vec<Entry>,
    /// More than MAX_FILES: the rest were left out.
    pub truncated: bool,
}

/// A file's download, worked out: where to fetch it, its name and size.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub url: String,
    pub name: String,
    pub size: Option<u64>,
    /// A Docs export: made as it's asked for, its size unknown (a HEAD
    /// answers 0), so it's fetched in one go rather than through aria2.
    pub export: bool,
}

fn download_url(id: &str) -> String {
    format!("https://drive.usercontent.google.com/download?id={id}&export=download")
}

/// A Docs editors file's export, as the Office format.
fn export_url(native: &str, id: &str) -> Option<String> {
    let format = export_ext(native)?;
    Some(format!("https://docs.google.com/{native}/d/{id}/export?format={format}"))
}

fn export_ext(native: &str) -> Option<&'static str> {
    match native {
        "document" => Some("docx"),
        "spreadsheets" => Some("xlsx"),
        "presentation" => Some("pptx"),
        _ => None,
    }
}

/// A file's download, fresh: the links behind the virus-scan warning don't
/// last, so this runs each time a download starts or carries on.
pub async fn resolve(id: &str, native: &str) -> Result<Resolved, String> {
    if !native.is_empty() {
        return resolve_export(native, id).await;
    }
    let url = download_url(id);
    let response = get(&url, Some("bytes=0-0")).await?;
    let status = response.status().as_u16();
    if let Some(found) = file_response(&url, &response) {
        return Ok(found);
    }
    match status {
        300..=399 => {
            let location = response.headers().get("location").and_then(|l| l.to_str().ok()).unwrap_or_default();
            Err(if location.contains("accounts.google.com") { NOT_SHARED.to_string() } else { format!("Google Drive sent it elsewhere ({location})") })
        }
        401 | 403 => Err(error_of(&response.text().await.unwrap_or_default()).unwrap_or_else(|| NOT_SHARED.to_string())),
        404 | 410 => Err(GONE.to_string()),
        429 => Err(QUOTA.to_string()),
        // An empty file has no first byte; nor has a Docs editors file,
        // which then fails whole (500).
        416 => {
            let response = get(&url, None).await?;
            match file_response(&url, &response) {
                Some(found) => Ok(found),
                None if response.status().as_u16() == 500 => resolve_native(id).await,
                None => Err(format!("Google Drive: HTTP {}", response.status().as_u16())),
            }
        }
        500 => resolve_native(id).await,
        200..=299 => {
            let page = response.text().await.map_err(|e| e.to_string())?;
            if let Some(confirmed) = parse_warning(&page) {
                let response = get(&confirmed, Some("bytes=0-0")).await?;
                return match file_response(&confirmed, &response) {
                    Some(found) => Ok(found),
                    None => Err(error_of(&response.text().await.unwrap_or_default())
                        .unwrap_or_else(|| "Google Drive's download page has changed".to_string())),
                };
            }
            Err(error_of(&page).unwrap_or_else(|| "Google Drive sent a page instead of the file".to_string()))
        }
        _ => Err(format!("Google Drive: HTTP {status}")),
    }
}

/// A Docs editors file (no download of its own): its kind from where its
/// page is, and its export.
async fn resolve_native(id: &str) -> Result<Resolved, String> {
    let location = redirect_of(&format!("https://drive.google.com/open?id={id}")).await.unwrap_or_default();
    match native_kind(&location) {
        Some(native) => resolve_export(native, id).await,
        None => Err("Google Drive couldn't send this file".to_string()),
    }
}

async fn resolve_export(native: &str, id: &str) -> Result<Resolved, String> {
    let url = export_url(native, id).ok_or("This kind of Google file can't be downloaded")?;
    // Exports are made on request: the name only comes with the file.
    let response = http::client().get(&url).send().await.map_err(|e| e.to_string())?;
    match file_response(&url, &response) {
        Some(found) => Ok(Resolved { size: None, export: true, ..found }),
        None if response.url().host_str() == Some("accounts.google.com") => Err(NOT_SHARED.to_string()),
        None if response.status().as_u16() == 404 => Err(GONE.to_string()),
        None => Err(format!("Google Drive couldn't export it (HTTP {})", response.status().as_u16())),
    }
}

/// "document" for a docs.google.com/document/d/… link, and so on.
fn native_kind(url: &str) -> Option<&'static str> {
    ["document", "spreadsheets", "presentation"].into_iter().find(|kind| url.contains(&format!("docs.google.com/{kind}/d/")))
}

async fn get(url: &str, range: Option<&str>) -> Result<reqwest::Response, String> {
    let mut request = http::no_redirect_client().get(url);
    if let Some(range) = range {
        request = request.header("Range", range);
    }
    request.send().await.map_err(|e| format!("Couldn't reach Google Drive: {e}"))
}

/// The file itself (an attachment), if that's what came.
fn file_response(url: &str, response: &reqwest::Response) -> Option<Resolved> {
    if !response.status().is_success() {
        return None;
    }
    let header = |name: &str| response.headers().get(name).and_then(|v| v.to_str().ok());
    let name = crate::links::disposition_name(header("content-disposition")?)?;
    let size = header("content-range")
        .and_then(|r| r.rsplit('/').next()?.parse().ok())
        .or_else(|| if response.status().as_u16() == 206 { None } else { header("content-length")?.parse().ok() });
    Some(Resolved { url: url.to_string(), name, size, export: false })
}

/// The link behind a "can't scan this file for viruses" page: its form's
/// action with its hidden fields (id, export, confirm, uuid), or an older
/// page's "download anyway" link.
pub fn parse_warning(page: &str) -> Option<String> {
    let html = Html::parse_document(page);
    let select = |css: &str| Selector::parse(css).unwrap();
    if let Some(form) = html.select(&select("form#download-form")).next() {
        let action = form.value().attr("action")?;
        let mut url = url::Url::parse("https://drive.usercontent.google.com/").ok()?.join(action).ok()?;
        {
            let mut query = url.query_pairs_mut();
            for input in form.select(&select("input[type=hidden]")) {
                if let (Some(name), Some(value)) = (input.value().attr("name"), input.value().attr("value")) {
                    query.append_pair(name, value);
                }
            }
        }
        return Some(url.to_string());
    }
    html.select(&select("a[href*=\"/uc?export=download\"]"))
        .next()
        .and_then(|a| a.value().attr("href"))
        .and_then(|href| url::Url::parse("https://docs.google.com/").ok()?.join(href).ok())
        .map(|u| u.to_string())
}

/// What a Drive page says went wrong, in words the user can act on.
pub fn error_of(page: &str) -> Option<String> {
    if page.contains("Too many users have viewed or downloaded this file") || page.contains("Quota exceeded") {
        return Some(QUOTA.to_string());
    }
    let html = Html::parse_document(page);
    let select = |css: &str| Selector::parse(css).unwrap();
    let text = |css: &str| {
        html.select(&select(css)).next().map(|el| el.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" "))
    };
    if let Some(error) = text(".uc-error-subcaption").or_else(|| text(".uc-error-caption")).filter(|e| !e.is_empty()) {
        return Some(format!("Google Drive: {error}"));
    }
    let title = text("title").unwrap_or_default();
    if title.contains("Sign in") || title.contains("Sign-in") || page.contains("accounts.google.com/ServiceLogin") {
        return Some(NOT_SHARED.to_string());
    }
    None
}

/// One of a folder page's entries: its id, title, and what it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub kind: ItemKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Folder,
    File,
    /// A Docs editors file ("document", "spreadsheets"…).
    Native(String),
}

/// An `embeddedfolderview` page: the folder's name, and its entries (their
/// links tell folders, files and Docs files apart).
pub fn parse_folder(page: &str) -> Option<(String, Vec<Item>)> {
    let html = Html::parse_document(page);
    let select = |css: &str| Selector::parse(css).unwrap();
    // Every listing has the entries' box, even an empty one.
    html.select(&select(".flip-entries")).next()?;
    let name = html.select(&select("title")).next().map(|t| t.text().collect::<String>().trim().to_string()).unwrap_or_default();
    let mut items = Vec::new();
    for entry in html.select(&select("div.flip-entry")) {
        let Some(id) = entry.value().attr("id").and_then(|id| id.strip_prefix("entry-")).filter(|id| valid_id(id)) else {
            continue;
        };
        let href = entry.select(&select("a")).next().and_then(|a| a.value().attr("href")).unwrap_or_default();
        let title = entry
            .select(&select(".flip-entry-title"))
            .next()
            .map(|t| t.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let kind = if href.contains("/folders/") || href.contains("folderview") {
            ItemKind::Folder
        } else if let Some(kind) = href.split("docs.google.com/").nth(1).and_then(|rest| rest.split('/').next()).filter(|k| *k != "uc") {
            ItemKind::Native(kind.to_string())
        } else {
            ItemKind::File
        };
        items.push(Item { id: id.to_string(), title, kind });
    }
    Some((name, items))
}

/// A name that's safe as one part of a path.
fn path_part(name: &str) -> String {
    let cleaned: String =
        name.chars().map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c }).collect();
    let cleaned = cleaned.trim().trim_matches('.').trim().to_string();
    if cleaned.is_empty() { "_".to_string() } else { cleaned }
}

/// Lists a shared folder's files, subfolders' too (their paths keep the
/// subfolders), with the sizes of the first MAX_SIZED.
pub async fn list_folder(id: &str) -> Result<Folder, String> {
    let (name, items) = folder_page(id).await?;
    let mut entries = Vec::new();
    let mut truncated = false;
    // Breadth first, so a cap leaves out the deepest files.
    let mut queue = std::collections::VecDeque::from([(String::new(), items)]);
    let mut folders = 1;
    while let Some((prefix, items)) = queue.pop_front() {
        for item in items {
            let path = format!("{prefix}{}", path_part(&item.title));
            match item.kind {
                ItemKind::Folder if folders < MAX_FOLDERS => {
                    folders += 1;
                    match folder_page(&item.id).await {
                        Ok((_, inside)) => queue.push_back((format!("{path}/"), inside)),
                        Err(e) => log::warn!("couldn't list Drive folder {path}: {e}"),
                    }
                }
                ItemKind::Folder => truncated = true,
                _ if entries.len() >= MAX_FILES => truncated = true,
                ItemKind::File => entries.push(Entry { id: item.id, path, size: None, native: String::new() }),
                ItemKind::Native(kind) => match export_ext(&kind) {
                    Some(ext) => {
                        let path = if path.to_ascii_lowercase().ends_with(&format!(".{ext}")) { path } else { format!("{path}.{ext}") };
                        entries.push(Entry { id: item.id, path, size: None, native: kind });
                    }
                    // Forms, drawings, sites: nothing to save.
                    None => log::info!("skipping Drive item {path} ({kind})"),
                },
            }
        }
    }
    fill_sizes(&mut entries).await;
    Ok(Folder { name, entries, truncated })
}

async fn folder_page(id: &str) -> Result<(String, Vec<Item>), String> {
    let response = http::client()
        .get(format!("https://drive.google.com/embeddedfolderview?id={id}"))
        .send()
        .await
        .map_err(|e| format!("Couldn't reach Google Drive: {e}"))?;
    if response.url().host_str() == Some("accounts.google.com") || matches!(response.status().as_u16(), 401 | 403 | 404) {
        return Err(FOLDER_NOT_SHARED.to_string());
    }
    if !response.status().is_success() {
        return Err(format!("Google Drive: HTTP {}", response.status().as_u16()));
    }
    let page = response.text().await.map_err(|e| e.to_string())?;
    parse_folder(&page).ok_or_else(|| match error_of(&page) {
        Some(e) if e == NOT_SHARED => FOLDER_NOT_SHARED.to_string(),
        Some(e) => e,
        None => "Google Drive's folder page has changed".to_string(),
    })
}

/// Sizes for the files, by asking for each one's first byte (a few at a
/// time); Docs exports have none until they're made.
async fn fill_sizes(entries: &mut [Entry]) {
    let mut tasks = tokio::task::JoinSet::new();
    let wanted: Vec<usize> = (0..entries.len()).filter(|&i| entries[i].native.is_empty()).take(MAX_SIZED).collect();
    let mut next = wanted.into_iter();
    let spawn = |tasks: &mut tokio::task::JoinSet<_>, i: usize, id: String| {
        tasks.spawn(async move { (i, size_of(&id).await) });
    };
    for i in next.by_ref().take(8) {
        spawn(&mut tasks, i, entries[i].id.clone());
    }
    while let Some(done) = tasks.join_next().await {
        if let Ok((i, size)) = done {
            entries[i].size = size;
        }
        if let Some(i) = next.next() {
            spawn(&mut tasks, i, entries[i].id.clone());
        }
    }
}

/// A file's size from its first byte's answer (confirmed, so large files
/// answer too, not their warning page).
async fn size_of(id: &str) -> Option<u64> {
    let response = get(&format!("{}&confirm=t", download_url(id)), Some("bytes=0-0")).await.ok()?;
    match response.status().as_u16() {
        206 => response.headers().get("content-range")?.to_str().ok()?.rsplit('/').next()?.parse().ok(),
        416 => Some(0),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        let file = |id: &str| Some(Link::File(id.to_string()));
        let folder = |id: &str| Some(Link::Folder(id.to_string()));
        assert_eq!(parse_link("https://drive.google.com/file/d/0B9P1L--7Wd2vU3VUVlFnbTgtS2c/view?usp=sharing"), file("0B9P1L--7Wd2vU3VUVlFnbTgtS2c"));
        assert_eq!(parse_link("https://drive.google.com/file/u/1/d/13AhW1Z1GYGaiTpJ0Pr2TTXoQivb6jx-a"), file("13AhW1Z1GYGaiTpJ0Pr2TTXoQivb6jx-a"));
        assert_eq!(parse_link("https://drive.google.com/open?id=0B9P1L--7Wd2vU3VUVlFnbTgtS2c"), file("0B9P1L--7Wd2vU3VUVlFnbTgtS2c"));
        assert_eq!(parse_link("https://drive.google.com/u/0/uc?id=0B9P1L--7Wd2vU3VUVlFnbTgtS2c&export=download"), file("0B9P1L--7Wd2vU3VUVlFnbTgtS2c"));
        assert_eq!(parse_link("https://drive.google.com/uc?id=1l_5RK28JRL19wpT22B-DY9We3TVXnnQQ&export=download"), file("1l_5RK28JRL19wpT22B-DY9We3TVXnnQQ"));
        assert_eq!(parse_link("https://docs.google.com/uc?export=download&id=1l_5RK28JRL19wpT22B-DY9We3TVXnnQQ"), file("1l_5RK28JRL19wpT22B-DY9We3TVXnnQQ"));
        assert_eq!(
            parse_link("https://drive.usercontent.google.com/download?id=1l_5RK28JRL19wpT22B-DY9We3TVXnnQQ&export=download&confirm=t"),
            file("1l_5RK28JRL19wpT22B-DY9We3TVXnnQQ")
        );
        assert_eq!(parse_link("https://drive.google.com/drive/folders/15uNXeRBIhVvZJIhL4yTw4IsStMhUaaxl?usp=sharing"), folder("15uNXeRBIhVvZJIhL4yTw4IsStMhUaaxl"));
        assert_eq!(parse_link("https://drive.google.com/drive/u/0/folders/1KpLl_1tcK0eeehzN980zbG-3M2nhbVks"), folder("1KpLl_1tcK0eeehzN980zbG-3M2nhbVks"));
        assert_eq!(parse_link("https://drive.google.com/embeddedfolderview?id=1KpLl_1tcK0eeehzN980zbG-3M2nhbVks#list"), folder("1KpLl_1tcK0eeehzN980zbG-3M2nhbVks"));
        assert_eq!(parse_link("https://drive.google.com/drive/my-drive"), None);
        assert_eq!(parse_link("https://docs.google.com/document/d/1TFYNzuZJTgNGzGmjraZ58ZVOh9_YoKeBnU-opWgXQL4/edit"), None);
        assert_eq!(parse_link("https://example.com/file/d/0B9P1L--7Wd2vU3VUVlFnbTgtS2c/view"), None);
    }

    // drive.usercontent.google.com's answer for fcn8s_from_caffe.npz (476 MB),
    // its style cut.
    const WARNING: &str = r#"<!DOCTYPE html><html><head><title>Google Drive - Virus scan warning</title><meta http-equiv="content-type" content="text/html; charset=utf-8"/></head><body><div class="uc-main"><div id="uc-dl-icon" class="image-container"><div class="drive-sprite-aux-download-file"></div></div><div id="uc-text"><p class="uc-warning-caption">Google Drive can't scan this file for viruses.</p><p class="uc-warning-subcaption"><span class="uc-name-size"><a href="/open?id=1l_5RK28JRL19wpT22B-DY9We3TVXnnQQ">fcn8s_from_caffe.npz</a> (476M)</span> is too large for Google to scan for viruses. Would you still like to download this file?</p><form id="download-form" action="https://drive.usercontent.google.com/download" method="get"><input type="submit" id="uc-download-link" class="goog-inline-block jfk-button jfk-button-action" value="Download anyway"/><input type="hidden" name="id" value="1l_5RK28JRL19wpT22B-DY9We3TVXnnQQ"><input type="hidden" name="export" value="download"><input type="hidden" name="confirm" value="t"><input type="hidden" name="uuid" value="6c013360-8f97-49af-9ece-10d3d6ee2d17"></form></div></div><div class="uc-footer"><hr class="uc-footer-divider"></div></body></html>"#;

    #[test]
    fn virus_scan_warning() {
        assert_eq!(
            parse_warning(WARNING).as_deref(),
            Some("https://drive.usercontent.google.com/download?id=1l_5RK28JRL19wpT22B-DY9We3TVXnnQQ&export=download&confirm=t&uuid=6c013360-8f97-49af-9ece-10d3d6ee2d17")
        );
        assert_eq!(error_of(WARNING), None);
        let old = r#"<a id="uc-download-link" href="/uc?export=download&amp;confirm=AbCd&amp;id=0B9P1L--7Wd2vU3VUVlFnbTgtS2c">Download anyway</a>"#;
        assert_eq!(parse_warning(old).as_deref(), Some("https://docs.google.com/uc?export=download&confirm=AbCd&id=0B9P1L--7Wd2vU3VUVlFnbTgtS2c"));
    }

    #[test]
    fn errors() {
        let quota = r#"<html><head><title>Google Drive - Quota exceeded</title></head><body><p class="uc-error-caption">Sorry, you can't view or download this file at this time.</p><p class="uc-error-subcaption">Too many users have viewed or downloaded this file recently. Please try accessing the file again later.</p></body></html>"#;
        assert_eq!(error_of(quota).as_deref(), Some(QUOTA));
        let other = r#"<p class="uc-error-caption">Sorry, the file you have requested does not exist.</p>"#;
        assert_eq!(error_of(other).as_deref(), Some("Google Drive: Sorry, the file you have requested does not exist."));
        assert_eq!(parse_warning(quota), None);
    }

    // embeddedfolderview?id=1KpLl_1tcK0eeehzN980zbG-3M2nhbVks (gdown's test
    // folder), its scripts, styles and pictures cut, with a Docs entry from
    // 15uNXeRBIhVvZJIhL4yTw4IsStMhUaaxl added.
    const FOLDER: &str = r#"<!DOCTYPE html><html><head><title>gdown_folder_test</title><meta http-equiv="content-type" content="text/html; charset=utf-8"/></head><body class="flip-embedded"><div id="flip-contents" class="flip-contents flip-list-view"><div class="flip-list-header"><div role="heading" aria-level="2" class="flip-list-title-header">TITLE</div></div><div class="flip-entries"><div class="flip-entry" id="entry-1aMZqPaU03E7XOQNXtjSCdguRHBaIQ82m" tabindex="0" role="link"><div class="flip-entry-info"><a href="https://drive.google.com/drive/folders/1aMZqPaU03E7XOQNXtjSCdguRHBaIQ82m" target="_blank"><div class="flip-entry-list-icon"><div aria-label="Folder" class="icon-color-1 drive-sprite-folder-list-shared-icon"></div></div><div class="flip-entry-title">directory-0</div></a></div><div class="flip-entry-last-modified"><div>10/29/20</div></div></div><div class="flip-entry" id="entry-1Z2VYnXb01h-3uvEptoQ48Fo__eAn0wc1" tabindex="0" role="link"><div class="flip-entry-info"><a href="https://drive.google.com/file/d/1Z2VYnXb01h-3uvEptoQ48Fo__eAn0wc1/view?usp=drive_web" target="_blank"><div class="flip-entry-list-icon"><img src="https://drive-thirdparty.googleusercontent.com/16/type/image/jpeg" alt=""/></div><div class="flip-entry-title">fractal.jpg</div></a></div><div class="flip-entry-last-modified"><div>10/29/20</div></div></div><div class="flip-entry" id="entry-14xzOzvKjP0at07jfonV7qVrTKoctFijz" tabindex="0" role="link"><div class="flip-entry-info"><a href="https://drive.google.com/file/d/14xzOzvKjP0at07jfonV7qVrTKoctFijz/view?usp=drive_web" target="_blank"><div class="flip-entry-title">this is a file.txt</div></a></div></div><div class="flip-entry" id="entry-1TFYNzuZJTgNGzGmjraZ58ZVOh9_YoKeBnU-opWgXQL4" tabindex="0" role="link"><div class="flip-entry-info"><a href="https://docs.google.com/document/d/1TFYNzuZJTgNGzGmjraZ58ZVOh9_YoKeBnU-opWgXQL4/edit?usp=drive_web" target="_blank"><div class="flip-entry-title">gdown.docx</div></a></div></div></div></div></body></html>"#;

    #[test]
    fn folder_listing() {
        let (name, items) = parse_folder(FOLDER).unwrap();
        assert_eq!(name, "gdown_folder_test");
        let item = |id: &str, title: &str, kind| Item { id: id.to_string(), title: title.to_string(), kind };
        assert_eq!(
            items,
            vec![
                item("1aMZqPaU03E7XOQNXtjSCdguRHBaIQ82m", "directory-0", ItemKind::Folder),
                item("1Z2VYnXb01h-3uvEptoQ48Fo__eAn0wc1", "fractal.jpg", ItemKind::File),
                item("14xzOzvKjP0at07jfonV7qVrTKoctFijz", "this is a file.txt", ItemKind::File),
                item("1TFYNzuZJTgNGzGmjraZ58ZVOh9_YoKeBnU-opWgXQL4", "gdown.docx", ItemKind::Native("document".to_string())),
            ]
        );
        // An empty folder still lists; a page that isn't a listing doesn't.
        let empty = r#"<html><head><title>directory-0</title></head><body><div id="flip-contents"><div class="flip-entries"></div></div></body></html>"#;
        assert_eq!(parse_folder(empty), Some(("directory-0".to_string(), Vec::new())));
        assert_eq!(parse_folder("<html><title>Error 404 (Not Found)!!1</title></html>"), None);
    }
}
