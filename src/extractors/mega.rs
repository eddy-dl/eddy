//! MEGA's public files and folders. A link carries its key after the "#":
//! MEGA only ever stores files encrypted, with AES-128 in CTR mode, and
//! their names in encrypted attributes. The API (g.api.mega.co.nz) gives a
//! file's size, attributes and a download link (which honours Range, so
//! aria2 fetches it like any file); the file is then decrypted where it is
//! (`decrypt_file`), and checked against the MAC its key carries. A folder
//! link's key opens each of its nodes' keys.

use super::http;
use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit, KeyIvInit, StreamCipher, StreamCipherSeek};
use aes::{Aes128Enc, Block};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const GONE: &str = "This MEGA file or folder doesn't exist (deleted, or the link is wrong)";
pub const BAD_KEY: &str = "This MEGA link's key is missing or wrong (the part after #)";
pub const QUOTA: &str = "MEGA's free transfer limit is used up for now; try again in a few hours";
pub const DAMAGED: &str = "The file from MEGA doesn't match its checksum (it was damaged on the way); try it again";

/// A folder's files are listed up to this many; the rest are left out.
pub const MAX_FILES: usize = 5000;

const API: &str = "https://g.api.mega.co.nz/cs";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    File { handle: String, key: [u8; 32] },
    /// `node`: a file or subfolder inside it, which the link points at.
    Folder { handle: String, key: [u8; 16], node: Option<String> },
}

/// A mega.nz (or mega.co.nz) link: `file/<handle>#<key>`,
/// `folder/<handle>#<key>[/file|folder/<node>]`, or the older `#!<handle>!<key>`
/// and `#F!<handle>!<key>[!<node>]`.
pub fn parse_link(url: &str) -> Option<Link> {
    let parsed = url::Url::parse(url.trim()).ok()?;
    let host = parsed.host_str()?.trim_start_matches("www.");
    if !matches!(host, "mega.nz" | "mega.co.nz" | "mega.io") {
        return None;
    }
    let fragment = parsed.fragment().unwrap_or("");
    let segments: Vec<&str> = parsed.path_segments().map(|s| s.filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    match segments.as_slice() {
        ["file" | "embed", handle] => file_link(handle, fragment),
        ["folder", handle] => {
            // "<key>", or "<key>/file/<node>", "<key>/folder/<node>"
            let mut parts = fragment.split('/');
            let key = parts.next()?;
            let node = match (parts.next(), parts.next()) {
                (Some("file" | "folder"), Some(node)) if valid_handle(node) => Some(node.to_string()),
                _ => None,
            };
            folder_link(handle, key, node)
        }
        [] => {
            // The old forms, all in the fragment.
            if let Some(rest) = fragment.strip_prefix('!') {
                let (handle, key) = rest.split_once('!')?;
                file_link(handle, key)
            } else if let Some(rest) = fragment.strip_prefix("F!") {
                let mut parts = rest.split('!');
                let (handle, key) = (parts.next()?, parts.next()?);
                let node = parts.next().filter(|n| valid_handle(n)).map(str::to_string);
                folder_link(handle, key, node)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn file_link(handle: &str, key: &str) -> Option<Link> {
    let key: [u8; 32] = b64(key)?.try_into().ok()?;
    valid_handle(handle).then(|| Link::File { handle: handle.to_string(), key })
}

fn folder_link(handle: &str, key: &str, node: Option<String>) -> Option<Link> {
    let key: [u8; 16] = b64(key)?.try_into().ok()?;
    valid_handle(handle).then(|| Link::Folder { handle: handle.to_string(), key, node })
}

fn valid_handle(handle: &str) -> bool {
    handle.len() == 8 && handle.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub fn can_handle(url: &str) -> bool {
    parse_link(url).is_some()
}

/// MEGA's base64: URL-safe, unpadded (old links may have commas).
fn b64(text: &str) -> Option<Vec<u8>> {
    let text: String = text.chars().filter(|c| !matches!(c, ',' | '=')).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text.as_bytes()).ok()
}

fn b64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A file's key (32 bytes, from its link or its folder): the AES key is its
/// two halves XORed; the CTR nonce and the MAC to check follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileKey {
    pub aes: [u8; 16],
    pub nonce: [u8; 8],
    pub mac: [u8; 8],
}

impl FileKey {
    pub fn new(key: &[u8; 32]) -> Self {
        let mut aes = [0; 16];
        for (i, byte) in aes.iter_mut().enumerate() {
            *byte = key[i] ^ key[i + 16];
        }
        Self { aes, nonce: key[16..24].try_into().unwrap(), mac: key[24..32].try_into().unwrap() }
    }

    /// As a folder entry keeps it (`Entry::key`).
    pub fn from_b64(text: &str) -> Option<Self> {
        let key: [u8; 32] = b64(text)?.try_into().ok()?;
        Some(Self::new(&key))
    }
}

/// One of a folder's files.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    /// Its node handle (its link is asked for with it and the folder's).
    pub handle: String,
    /// Inside the folder, through its subfolders ("sub/file.ext").
    pub path: String,
    pub size: u64,
    /// Its 32-byte key (base64), opened with the folder's.
    pub key: String,
}

/// A MEGA folder's download, handed from the preview to the downloader (as
/// JSON in `DownloadRequest::format`): the files picked. Empty: all of
/// them, listed again when it starts.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MegaRequest {
    pub files: Vec<Entry>,
}

#[derive(Clone, Debug)]
pub struct Folder {
    pub name: String,
    pub entries: Vec<Entry>,
    /// The link pointed at one file inside the folder: it's that file alone.
    pub single: bool,
    /// More than MAX_FILES: the rest were left out.
    pub truncated: bool,
}

/// A file's download, worked out: where to fetch it, its name and size.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub url: String,
    pub name: String,
    pub size: u64,
}

fn error_text(code: i64) -> String {
    match code {
        -2 => "This isn't a valid MEGA link".to_string(),
        -3 => "MEGA is busy; try again in a moment".to_string(),
        -4 | -6 => "MEGA is limiting requests; try again in a moment".to_string(),
        -9 => GONE.to_string(),
        -11 => "This MEGA link can't be opened (it isn't shared publicly)".to_string(),
        -16 => "MEGA has taken this down".to_string(),
        -17 => QUOTA.to_string(),
        -18 => "MEGA: this is unavailable for now; try again later".to_string(),
        code => format!("MEGA refused it (error {code})"),
    }
}

/// One API command; `folder`: the folder link it's under. MEGA answers an
/// array with one result per command, or a bare error number; "busy" and
/// "too fast" are tried again a few times.
async fn api(command: serde_json::Value, folder: Option<&str>) -> Result<serde_json::Value, String> {
    let mut wait = std::time::Duration::from_millis(500);
    for attempt in 0.. {
        let id: u32 = rand::random();
        let url = match folder {
            Some(folder) => format!("{API}?id={id}&n={folder}"),
            None => format!("{API}?id={id}"),
        };
        let response = http::client()
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::json!([command]).to_string())
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| format!("Couldn't reach MEGA: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("MEGA answered HTTP {}", response.status().as_u16()));
        }
        let bytes = response.bytes().await.map_err(|e| format!("Couldn't reach MEGA: {e}"))?;
        let body: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| format!("MEGA sent something unexpected: {e}"))?;
        let result = match body {
            serde_json::Value::Array(mut results) if !results.is_empty() => results.swap_remove(0),
            other => other,
        };
        match result.as_i64() {
            Some(code) if code < 0 => {
                if matches!(code, -3 | -4 | -6) && attempt < 5 {
                    tokio::time::sleep(wait).await;
                    wait *= 2;
                    continue;
                }
                return Err(error_text(code));
            }
            _ => return Ok(result),
        }
    }
    unreachable!()
}

/// A node's name, from its attributes ("MEGA{json}", AES-CBC with a zero IV).
fn name_from_attrs(attrs: &str, key: &[u8; 16]) -> Option<String> {
    let mut data = b64(attrs)?;
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let cipher = Aes128::new(&(*key).into());
    let mut previous = [0u8; 16];
    for block in data.chunks_mut(16) {
        let encrypted: [u8; 16] = block.try_into().unwrap();
        let mut plain = Block::from(encrypted);
        cipher.decrypt_block(&mut plain);
        for ((byte, p), prev) in block.iter_mut().zip(plain.iter()).zip(previous) {
            *byte = p ^ prev;
        }
        previous = encrypted;
    }
    let text = data.strip_prefix(b"MEGA")?;
    let end = text.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let value: serde_json::Value = serde_json::from_slice(&text[..end]).ok()?;
    value.get("n")?.as_str().map(str::to_string)
}

/// A node key from a folder listing, opened with the folder's key (AES-ECB).
fn open_node_key(encrypted: &str, folder_key: &[u8; 16]) -> Option<Vec<u8>> {
    let mut data = b64(encrypted)?;
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let cipher = Aes128::new(&(*folder_key).into());
    for block in data.chunks_mut(16) {
        let mut plain = Block::from(<[u8; 16]>::try_from(&*block).unwrap());
        cipher.decrypt_block(&mut plain);
        block.copy_from_slice(&plain);
    }
    Some(data)
}

fn download_link(result: &serde_json::Value) -> Result<(String, u64), String> {
    let size = result.get("s").and_then(|s| s.as_u64()).unwrap_or(0);
    match result.get("g").and_then(|g| g.as_str()) {
        Some(url) if !url.is_empty() => Ok((url.to_string(), size)),
        // Over the transfer quota, MEGA gives no link.
        _ if result.get("efq").is_some() || result.get("tl").is_some() => Err(QUOTA.to_string()),
        _ => Err("MEGA gave no download link for it".to_string()),
    }
}

/// A public file's name, size and download link (fresh each time: links
/// don't last).
pub async fn resolve_file(handle: &str, key: &[u8; 32]) -> Result<Resolved, String> {
    let result = api(serde_json::json!({"a": "g", "g": 1, "ssl": 2, "p": handle}), None).await?;
    let file_key = FileKey::new(key);
    let attrs = result.get("at").and_then(|a| a.as_str()).unwrap_or("");
    let name = name_from_attrs(attrs, &file_key.aes).ok_or(BAD_KEY)?;
    let (url, size) = download_link(&result)?;
    Ok(Resolved { url, name, size })
}

/// The download link of a file in a folder (fresh each time).
pub async fn resolve_node(folder: &str, node: &str) -> Result<(String, u64), String> {
    let result = api(serde_json::json!({"a": "g", "g": 1, "ssl": 2, "n": node}), Some(folder)).await?;
    download_link(&result)
}

/// A folder's files (or the subfolder's, or the one file, the link points
/// at), each with its path inside it and its key.
pub async fn list_folder(handle: &str, key: &[u8; 16], node: Option<&str>) -> Result<Folder, String> {
    let result = api(serde_json::json!({"a": "f", "c": 1, "ca": 1, "r": 1}), Some(handle)).await?;
    let nodes = result.get("f").and_then(|f| f.as_array()).ok_or("MEGA sent no folder listing")?;
    struct Node {
        parent: String,
        folder: bool,
        name: String,
        size: u64,
        key: Vec<u8>,
    }
    let handles: std::collections::HashSet<&str> = nodes.iter().filter_map(|n| n.get("h")?.as_str()).collect();
    // The shared folder is the node whose parent isn't listed; a node's key
    // entries ("owner:key", "/"-separated) under its handle are the ones
    // the link's key opens (others are its owner's own).
    let root = nodes
        .iter()
        .find(|n| n.get("p").and_then(|p| p.as_str()).is_none_or(|p| !handles.contains(p)))
        .and_then(|n| n.get("h")?.as_str())
        .ok_or("MEGA sent an empty folder")?
        .to_string();
    let mut found: std::collections::HashMap<String, Node> = std::collections::HashMap::new();
    for n in nodes {
        let field = |name: &str| n.get(name).and_then(|v| v.as_str()).unwrap_or("");
        let folder = n.get("t").and_then(|t| t.as_i64()) == Some(1);
        let Some(encrypted) = field("k").split('/').find_map(|entry| entry.split_once(':').filter(|(owner, _)| *owner == root).map(|(_, k)| k)) else {
            continue;
        };
        let Some(node_key) = open_node_key(encrypted, key) else {
            continue;
        };
        let attrs_key: [u8; 16] = match (folder, node_key.len()) {
            (true, 16..) => node_key[..16].try_into().unwrap(),
            (false, 32..) => FileKey::new(&node_key[..32].try_into().unwrap()).aes,
            _ => continue,
        };
        let Some(name) = name_from_attrs(field("a"), &attrs_key) else {
            log::warn!("MEGA folder {handle}: a node whose name can't be read");
            continue;
        };
        found.insert(
            field("h").to_string(),
            Node { parent: field("p").to_string(), folder, name, size: n.get("s").and_then(|s| s.as_u64()).unwrap_or(0), key: node_key },
        );
    }
    let Some(root_node) = found.get(&root) else {
        return Err(BAD_KEY.to_string());
    };
    // What the link points at inside, else the shared folder itself.
    let top = node.filter(|n| found.contains_key(*n)).unwrap_or(&root).to_string();
    let top_node = &found[&top];
    let path_of = |handle: &str| -> Option<String> {
        let mut parts = Vec::new();
        let mut at = handle;
        while at != top {
            let node = found.get(at)?;
            parts.push(node.name.clone());
            at = &node.parent;
        }
        parts.reverse();
        Some(parts.join("/"))
    };
    let mut entries: Vec<Entry> = if top_node.folder {
        found
            .iter()
            .filter(|(_, n)| !n.folder && n.key.len() >= 32)
            .filter_map(|(h, n)| Some(Entry { handle: h.clone(), path: path_of(h)?, size: n.size, key: b64_encode(&n.key[..32]) }))
            .collect()
    } else {
        vec![Entry { handle: top.clone(), path: top_node.name.clone(), size: top_node.size, key: b64_encode(&top_node.key[..32]) }]
    };
    entries.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    let truncated = entries.len() > MAX_FILES;
    entries.truncate(MAX_FILES);
    Ok(Folder {
        name: if top_node.folder { top_node.name.clone() } else { root_node.name.clone() },
        entries,
        single: !top_node.folder,
        truncated,
    })
}

/// MEGA's MAC chunks: 128 KiB, 256 KiB, … up to 1 MiB, then 1 MiB each.
fn chunks(size: u64) -> impl Iterator<Item = (u64, u64)> {
    let mut offset = 0u64;
    let mut step = 0x20000u64;
    std::iter::from_fn(move || {
        if offset >= size {
            return None;
        }
        let chunk = (offset, step.min(size - offset));
        offset += step;
        step = (step + 0x20000).min(0x100000);
        Some(chunk)
    })
}

/// One chunk's CBC-MAC (its plaintext, zero-padded), starting from the
/// nonce twice.
fn chunk_mac(cipher: &Aes128Enc, nonce: &[u8; 8], data: &[u8]) -> [u8; 16] {
    let mut mac = Block::default();
    mac[..8].copy_from_slice(nonce);
    mac[8..].copy_from_slice(nonce);
    for block in data.chunks(16) {
        for (m, b) in mac.iter_mut().zip(block) {
            *m ^= b;
        }
        cipher.encrypt_block(&mut mac);
    }
    mac.into()
}

/// Folds a chunk's MAC into the file's (CBC over the chunk MACs).
fn fold_mac(cipher: &Aes128Enc, file_mac: &mut [u8; 16], chunk: [u8; 16]) {
    let mut block = Block::default();
    for ((b, f), c) in block.iter_mut().zip(file_mac.iter()).zip(chunk) {
        *b = f ^ c;
    }
    cipher.encrypt_block(&mut block);
    *file_mac = block.into();
}

/// Where `decrypt_file` is: the chunk it's on (with its ciphertext's first
/// 32 bytes, which decrypting changes),
/// and the file's MAC up to it. Written before each chunk is, so a restart
/// after the app was stopped knows whether that chunk was written yet; at
/// the end it says the file is decrypted (its offset is the file's size),
/// and stays until the file is moved to its name (then the caller removes
/// it), so that a file decrypted already is never decrypted again.
pub fn journal_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".dec");
    path.with_file_name(name)
}

fn read_journal(path: &Path) -> Option<(u64, [u8; 32], [u8; 16])> {
    let data = std::fs::read(journal_path(path)).ok()?;
    if data.len() != 56 {
        return None;
    }
    Some((u64::from_le_bytes(data[..8].try_into().ok()?), data[8..40].try_into().ok()?, data[40..56].try_into().ok()?))
}

fn write_journal(path: &Path, offset: u64, start: &[u8; 32], mac: &[u8; 16]) -> std::io::Result<()> {
    let mut entry = Vec::with_capacity(56);
    entry.extend_from_slice(&offset.to_le_bytes());
    entry.extend_from_slice(start);
    entry.extend_from_slice(mac);
    std::fs::write(journal_path(path), &entry)
}

/// Decrypts a downloaded file where it is, checking it against its key's
/// MAC. Stopped partway (the app closed, or `stop` set between chunks), it
/// carries on from its journal (`<file>.dec`) next time; `Ok(false)` when
/// it stopped. Blocking: run it off the async threads.
pub fn decrypt_file(path: &Path, key: &FileKey, stop: &std::sync::atomic::AtomicBool, mut progress: impl FnMut(u64, u64)) -> Result<bool, String> {
    type Ctr = ctr::Ctr128BE<Aes128>;
    let fail = |e: std::io::Error| format!("Couldn't decrypt {}: {e}", path.display());
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(path).map_err(fail)?;
    let size = file.metadata().map_err(fail)?.len();
    let cipher = Aes128Enc::new(&key.aes.into());
    let mut iv = [0u8; 16];
    iv[..8].copy_from_slice(&key.nonce);
    let mut ctr = Ctr::new(&key.aes.into(), &iv.into());
    let (start, mut file_mac, pending) = match read_journal(path) {
        Some((offset, hash, mac)) => (offset, mac, Some(hash)),
        None => (0, [0u8; 16], None),
    };
    let mut buffer = Vec::with_capacity(0x100000);
    for (offset, length) in chunks(size).skip_while(|(offset, _)| *offset < start) {
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(false);
        }
        buffer.resize(length as usize, 0);
        file.seek(SeekFrom::Start(offset)).map_err(fail)?;
        file.read_exact(&mut buffer).map_err(fail)?;
        // Its first 32 bytes (or all of a shorter chunk, zero-padded).
        let mut head = [0u8; 32];
        let n = buffer.len().min(32);
        head[..n].copy_from_slice(&buffer[..n]);
        // The chunk the journal names, written before it stopped: it's
        // plaintext already (its ciphertext's start no longer matches).
        let written = offset == start && pending.is_some_and(|h| h != head);
        if !written {
            write_journal(path, offset, &head, &file_mac).map_err(fail)?;
            ctr.seek(offset);
            ctr.apply_keystream(&mut buffer);
        }
        fold_mac(&cipher, &mut file_mac, chunk_mac(&cipher, &key.nonce, &buffer));
        if !written {
            file.seek(SeekFrom::Start(offset)).map_err(fail)?;
            file.write_all(&buffer).map_err(fail)?;
        }
        progress(offset + length, size);
    }
    file.sync_all().map_err(fail)?;
    // Decrypted: said so before anything else can happen to it.
    if start < size || pending.is_none() {
        write_journal(path, size, &[0; 32], &file_mac).map_err(fail)?;
    }
    let mut meta = [0u8; 8];
    for i in 0..4 {
        meta[i] = file_mac[i] ^ file_mac[i + 4];
        meta[i + 4] = file_mac[i + 8] ^ file_mac[i + 12];
    }
    if size > 0 && meta != key.mac {
        let _ = std::fs::remove_file(journal_path(path));
        return Err(DAMAGED.to_string());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links() {
        let file_key = "oHX8U0ZjGepKhGjpWrxlxF8UVDpPuWDmGAkZyNLcGwA";
        for url in [format!("https://mega.nz/file/nFcGFbbA#{file_key}"), format!("https://mega.nz/#!nFcGFbbA!{file_key}"), format!("https://mega.co.nz/#!nFcGFbbA!{file_key}")] {
            assert!(matches!(parse_link(&url), Some(Link::File { handle, .. }) if handle == "nFcGFbbA"), "{url}");
        }
        assert!(matches!(
            parse_link("https://mega.nz/folder/TqBmgDyI#USMnXad535FoQtIkHPEb9g"),
            Some(Link::Folder { handle, node: None, .. }) if handle == "TqBmgDyI"
        ));
        assert!(matches!(
            parse_link("https://mega.nz/folder/TqBmgDyI#USMnXad535FoQtIkHPEb9g/file/myZRBRaa"),
            Some(Link::Folder { node: Some(node), .. }) if node == "myZRBRaa"
        ));
        assert!(matches!(parse_link("https://mega.nz/#F!TqBmgDyI!USMnXad535FoQtIkHPEb9g"), Some(Link::Folder { .. })));
        // No key, a short key, another site.
        assert_eq!(parse_link("https://mega.nz/file/nFcGFbbA"), None);
        assert_eq!(parse_link("https://mega.nz/file/nFcGFbbA#oHX8U0ZjGep"), None);
        assert_eq!(parse_link(&format!("https://example.com/file/nFcGFbbA#{file_key}")), None);
    }

    #[test]
    fn chunk_sizes() {
        let sizes: Vec<u64> = chunks(0x500000).map(|(_, n)| n).collect();
        assert_eq!(&sizes[..8], &[0x20000, 0x40000, 0x60000, 0x80000, 0xA0000, 0xC0000, 0xE0000, 0x100000]);
        assert_eq!(sizes.iter().sum::<u64>(), 0x500000);
        assert_eq!(chunks(0).count(), 0);
    }

    /// Encrypts `plain` as MEGA would, with a key whose MAC matches it.
    fn encrypt(plain: &[u8]) -> (Vec<u8>, FileKey) {
        let mut key = FileKey { aes: *b"0123456789abcdef", nonce: *b"noncenon", mac: [0; 8] };
        let cipher = Aes128Enc::new(&key.aes.into());
        let mut file_mac = [0u8; 16];
        for (offset, length) in chunks(plain.len() as u64) {
            fold_mac(&cipher, &mut file_mac, chunk_mac(&cipher, &key.nonce, &plain[offset as usize..(offset + length) as usize]));
        }
        for i in 0..4 {
            key.mac[i] = file_mac[i] ^ file_mac[i + 4];
            key.mac[i + 4] = file_mac[i + 8] ^ file_mac[i + 12];
        }
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&key.nonce);
        let mut data = plain.to_vec();
        ctr::Ctr128BE::<Aes128>::new(&key.aes.into(), &iv.into()).apply_keystream(&mut data);
        (data, key)
    }

    #[test]
    fn decrypts_and_checks() {
        let dir = std::env::temp_dir().join(format!("eddy-mega-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let plain: Vec<u8> = (0..3_000_017u32).map(|i| (i * 7 % 251) as u8).collect();
        let (encrypted, key) = encrypt(&plain);
        let path = dir.join("file.mega");
        std::fs::write(&path, &encrypted).unwrap();
        let go = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(decrypt_file(&path, &key, &go, |_, _| {}), Ok(true));
        assert_eq!(std::fs::read(&path).unwrap(), plain);
        // Decrypted, it says so: run again, it leaves the file be.
        assert!(journal_path(&path).exists());
        assert_eq!(decrypt_file(&path, &key, &go, |_, _| {}), Ok(true));
        assert_eq!(std::fs::read(&path).unwrap(), plain);
        std::fs::remove_file(journal_path(&path)).unwrap();

        // Told to stop: it does, and carries on later.
        std::fs::write(&path, &encrypted).unwrap();
        let stop = std::sync::atomic::AtomicBool::new(false);
        let mut calls = 0;
        assert_eq!(decrypt_file(&path, &key, &stop, |_, _| { calls += 1; if calls == 4 { stop.store(true, std::sync::atomic::Ordering::SeqCst) } }), Ok(false));
        assert_eq!(decrypt_file(&path, &key, &go, |_, _| {}), Ok(true));
        assert_eq!(std::fs::read(&path).unwrap(), plain);
        std::fs::remove_file(journal_path(&path)).unwrap();

        // Stopped right after a chunk was written (its journal entry names
        // it): it carries on, neither undoing nor repeating it.
        std::fs::write(&path, &encrypted).unwrap();
        let mut stopped = encrypted.clone();
        let mut done = 0;
        let mut file_mac = [0u8; 16];
        let cipher = Aes128Enc::new(&key.aes.into());
        for (n, (offset, length)) in chunks(plain.len() as u64).enumerate() {
            let range = offset as usize..(offset + length) as usize;
            if n == 3 {
                let mut entry = offset.to_le_bytes().to_vec();
                entry.extend_from_slice(&encrypted[offset as usize..offset as usize + 32]);
                entry.extend_from_slice(&file_mac);
                std::fs::write(journal_path(&path), entry).unwrap();
                stopped[range.clone()].copy_from_slice(&plain[range]);
                break;
            }
            stopped[range.clone()].copy_from_slice(&plain[range.clone()]);
            fold_mac(&cipher, &mut file_mac, chunk_mac(&cipher, &key.nonce, &plain[range]));
            done += 1;
        }
        assert_eq!(done, 3);
        std::fs::write(&path, &stopped).unwrap();
        assert_eq!(decrypt_file(&path, &key, &go, |_, _| {}), Ok(true));
        assert_eq!(std::fs::read(&path).unwrap(), plain);
        std::fs::remove_file(journal_path(&path)).unwrap();

        // A damaged byte fails the MAC.
        let mut damaged = encrypted.clone();
        damaged[1_500_000] ^= 1;
        std::fs::write(&path, &damaged).unwrap();
        assert_eq!(decrypt_file(&path, &key, &go, |_, _| {}), Err(DAMAGED.to_string()));
        assert!(!journal_path(&path).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
