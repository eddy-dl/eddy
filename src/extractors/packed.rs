//! The packed scripts download helpers answer with (SnapSave for Instagram,
//! tikmate.io for TikTok): an eval() of the real answer, packed. Deno runs
//! it when there's a Deno; otherwise it's unpacked here.

use regex::Regex;
use std::sync::LazyLock;

/// The answer inside `raw_js` (as it is if it isn't packed).
pub async fn decode(raw_js: &str) -> String {
    if !raw_js.contains("eval(") {
        return raw_js.to_string();
    }
    match with_deno(raw_js).await {
        Some(answer) => answer,
        None => unpack(raw_js).unwrap_or_else(|| raw_js.to_string()),
    }
}

/// The fallback when Deno isn't there (or fails): the answer is hidden
/// in an eval() of a packed string: groups of digits in base `e`, written
/// with the alphabet `n` and split by `n[e]`, each a character code plus
/// `t`; the characters are UTF-8 bytes.
fn unpack(raw_js: &str) -> Option<String> {
    static PACKED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"\}\("([^"]*)",\s*\d+,\s*"([^"]*)",\s*(\d+),\s*(\d+),\s*\d+\)\)"#).unwrap()
    });
    let caps = PACKED.captures(raw_js)?;
    let (packed, alphabet) = (&caps[1], caps[2].chars().collect::<Vec<char>>());
    let offset: u32 = caps[3].parse().ok()?;
    let base: u32 = caps[4].parse().ok()?;
    let separator = *alphabet.get(base as usize)?;
    let mut bytes = Vec::new();
    for group in packed.split(separator).filter(|g| !g.is_empty()) {
        let mut value: u32 = 0;
        for c in group.chars() {
            let digit = alphabet.iter().position(|&a| a == c)? as u32;
            value = value.checked_mul(base)?.checked_add(digit)?;
        }
        bytes.push(u8::try_from(value.checked_sub(offset)?).ok()?);
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The real answer, by running the script through Deno (on Android,
/// YTDLnis's Deno helper app) with eval() caught, so whatever changes in
/// the packing still works. None without Deno, or if it failed.
async fn with_deno(raw_js: &str) -> Option<String> {
    let script = format!(
        r#"
    const raw = {};
    const fakeEvalHolder = {{ val: '' }};
    const patched = raw.replace(/\beval\s*\(/, 'fakeEvalHolder.val=(');
    try {{
        const fn = new Function('fakeEvalHolder', patched);
        fn(fakeEvalHolder);
    }} catch (e) {{}}
    const output = fakeEvalHolder.val;
    Deno.stdout.writeSync(new TextEncoder().encode(output));
    "#,
        serde_json::to_string(raw_js).unwrap_or_default()
    );
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        // The site's script, so it gets a time limit: one that never ends
        // would leave Deno running, and the lookup waiting, for good.
        const LIMIT: std::time::Duration = std::time::Duration::from_secs(15);
        let mut command = crate::platform::deno_command()?;
        let mut child = crate::children::own_group(&mut command)
            .args(["run", "-"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(script.as_bytes());
        }
        let mut stdout = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            let mut text = Vec::new();
            let _ = stdout.read_to_end(&mut text);
            text
        });
        let started = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() < LIMIT => std::thread::sleep(std::time::Duration::from_millis(50)),
                _ => {
                    log::warn!("deno took too long unpacking a script; stopped it");
                    crate::children::kill_tree(&mut child);
                    let _ = child.wait();
                    return None;
                }
            }
        };
        let text = String::from_utf8(reader.join().ok()?).ok()?;
        (status.success() && !text.is_empty()).then_some(text)
    })
    .await
    .ok()
    .flatten()
}

