//! The cookies.txt for sites that need a login (Settings → Downloads):
//! yt-dlp gets it with --cookies, and the TikTok and Instagram extractors
//! read their site's cookies from it. The user picks one exported from
//! their browser (Netscape format) or pastes it into the Cookies page;
//! Eddy keeps a copy in its data folder. A cookie extension's JSON export
//! (Cookie-Editor, EditThisCookie) is turned into a cookies.txt.

use crate::{AppSettings, DownloaderApp};
use slint::ComponentHandle;
use std::collections::BTreeSet;

/// Hooks up the Settings row. Call once, after creating the UI.
pub fn install(ui: &DownloaderApp) {
    let settings = ui.global::<AppSettings>();
    let ui_weak = ui.as_weak();
    settings.on_cookies_add(move || {
        let ui_weak = ui_weak.clone();
        crate::platform::pick_cookies_file(move |path, _name| {
            let result = import(std::path::Path::new(&path));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    let settings = ui.global::<AppSettings>();
                    settings.set_cookies_error(result.err().unwrap_or(0));
                    refresh(&settings);
                }
            });
        });
    });
    let ui_weak = ui.as_weak();
    settings.on_cookies_save(move |text| {
        let result = if text.trim().is_empty() { remove() } else { save(&text) };
        if let Some(ui) = ui_weak.upgrade() {
            let settings = ui.global::<AppSettings>();
            settings.set_cookies_error(result.err().unwrap_or(0));
            refresh(&settings);
        }
    });
    let ui_weak = ui.as_weak();
    settings.on_cookies_remove(move || {
        let _ = remove();
        if let Some(ui) = ui_weak.upgrade() {
            let settings = ui.global::<AppSettings>();
            settings.set_cookies_error(0);
            refresh(&settings);
        }
    });
    refresh(&settings);
}

fn refresh(settings: &AppSettings) {
    let text = std::fs::read_to_string(crate::extractors::http::cookie_file()).unwrap_or_default();
    let (count, sites) = summary(&text);
    settings.set_cookies_text(text.into());
    settings.set_cookies_count(count as i32);
    // A few, so the row stays one line.
    let shown: Vec<&str> = sites.iter().take(3).map(String::as_str).collect();
    let more = sites.len().saturating_sub(shown.len());
    let mut text = shown.join(", ");
    if more > 0 {
        text += &format!(" +{more}");
    }
    settings.set_cookies_sites(text.into());
}

/// How many cookies a Netscape cookies.txt holds, and for which sites.
fn summary(text: &str) -> (usize, BTreeSet<String>) {
    let mut count = 0;
    let mut sites = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 7 {
            count += 1;
            let host = parts[0].trim_start_matches('.');
            // The last two labels: "www.instagram.com" → "instagram.com".
            let labels: Vec<&str> = host.rsplitn(3, '.').collect();
            sites.insert(labels.iter().take(2).rev().copied().collect::<Vec<_>>().join("."));
        }
    }
    (count, sites)
}

/// Keeps a copy of the picked file, if it's a cookies.txt. The error is
/// AppSettings.cookies_error's code.
fn import(path: &std::path::Path) -> Result<(), i32> {
    let text = std::fs::read_to_string(path).map_err(|_| 1)?;
    save(&text)
}

/// Keeps `text` as the cookies, if it holds any (see `normalize`).
fn save(text: &str) -> Result<(), i32> {
    let text = normalize(text).ok_or(2)?;
    let target = crate::extractors::http::cookie_file();
    if let Some(dir) = target.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&target, text).map_err(|e| {
        log::warn!("couldn't save cookies.txt: {e}");
        3
    })
}

fn remove() -> Result<(), i32> {
    match std::fs::remove_file(crate::extractors::http::cookie_file()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(3),
        _ => Ok(()),
    }
}

const HEADER: &str = "# Netscape HTTP Cookie File";

/// `text` as a cookies.txt with its header, or None without a cookie in
/// it. Takes a cookies.txt, one whose tabs became spaces (pasted from a
/// note or a web page), or a cookie extension's JSON export.
fn normalize(text: &str) -> Option<String> {
    let trimmed = text.trim_start_matches('\u{feff}').trim();
    let lines = if trimmed.starts_with('[') || trimmed.starts_with('{') {
        from_json(trimmed)?
    } else {
        trimmed.lines().map(|line| retab(line.trim_end())).collect()
    };
    let mut out = String::new();
    if !lines.first().is_some_and(|l| l.starts_with("# Netscape") || l.starts_with("# HTTP Cookie File")) {
        out += HEADER;
        out += "\n";
    }
    for line in &lines {
        out += line;
        out += "\n";
    }
    (summary(&out).0 > 0).then_some(out)
}

/// A cookie line whose tabs became spaces, with its tabs back: the six
/// fields before the value have no spaces in them, so the rest is the value.
fn retab(line: &str) -> String {
    let body = line.strip_prefix("#HttpOnly_").unwrap_or(line);
    if line.contains('\t') || body.is_empty() || body.starts_with('#') {
        return line.to_string();
    }
    let mut fields = Vec::with_capacity(7);
    let mut rest = line.trim_start();
    for _ in 0..6 {
        let Some((field, after)) = rest.split_once(char::is_whitespace) else {
            return line.to_string();
        };
        fields.push(field);
        rest = after.trim_start();
    }
    let flag = |f: &str| f.eq_ignore_ascii_case("true") || f.eq_ignore_ascii_case("false");
    // Only lines shaped like a cookie: TRUE/FALSE flags and a number for
    // the expiry.
    if rest.is_empty() || !flag(fields[1]) || !flag(fields[3]) || fields[4].parse::<i64>().is_err() {
        return line.to_string();
    }
    fields.push(rest);
    fields.join("\t")
}

/// Cookie-Editor / EditThisCookie JSON: an array of cookies (or an object
/// with a "cookies" array), as cookies.txt lines.
fn from_json(text: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let cookies = value.as_array().or_else(|| value.get("cookies")?.as_array())?;
    let lines = cookies
        .iter()
        .filter_map(|c| {
            let domain = c.get("domain")?.as_str()?;
            let name = c.get("name")?.as_str()?;
            let value = c.get("value").and_then(|v| v.as_str()).unwrap_or("");
            let flag = |key: &str| c.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
            let path = c.get("path").and_then(|v| v.as_str()).unwrap_or("/");
            let expires = c
                .get("expirationDate")
                .or_else(|| c.get("expires"))
                .and_then(|v| v.as_f64())
                .filter(|_| !flag("session"))
                .map_or(0, |e| e.max(0.0) as i64);
            let subdomains = domain.starts_with('.') || !flag("hostOnly");
            let domain = if subdomains && !domain.starts_with('.') { format!(".{domain}") } else { domain.to_string() };
            let bool_text = |b: bool| if b { "TRUE" } else { "FALSE" };
            Some(format!(
                "{}{domain}\t{}\t{path}\t{}\t{expires}\t{name}\t{value}",
                if flag("httpOnly") { "#HttpOnly_" } else { "" },
                bool_text(subdomains),
                bool_text(flag("secure")),
            ))
        })
        .collect::<Vec<_>>();
    (!lines.is_empty()).then_some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaces_become_tabs() {
        let text = normalize(".example.com TRUE / FALSE 1999999999 sid abc def").unwrap();
        assert_eq!(text, format!("{HEADER}\n.example.com\tTRUE\t/\tFALSE\t1999999999\tsid\tabc def\n"));
    }

    #[test]
    fn json_export() {
        let json = r#"[{"domain":"www.instagram.com","hostOnly":true,"name":"sessionid","value":"x","path":"/","secure":true,"httpOnly":true,"expirationDate":1999999999.5},
                       {"domain":".tiktok.com","name":"tt","value":"y","session":true}]"#;
        let text = normalize(json).unwrap();
        assert!(text.contains("#HttpOnly_www.instagram.com\tFALSE\t/\tTRUE\t1999999999\tsessionid\tx\n"));
        assert!(text.contains(".tiktok.com\tTRUE\t/\tFALSE\t0\ttt\ty\n"));
        assert_eq!(summary(&text).0, 2);
    }

    #[test]
    fn not_cookies() {
        assert!(normalize("hello there").is_none());
        assert!(normalize("[]").is_none());
    }
}
