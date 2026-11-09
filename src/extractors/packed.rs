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
/// YTDLnis's Deno helper app; on iOS, the app's own JavaScript engine) with
/// eval() caught, so whatever changes in the packing still works. None
/// without Deno, or if it failed.
async fn with_deno(raw_js: &str) -> Option<String> {
    // Deno writes the answer out; the iOS engines have console.log.
    let print = if cfg!(target_os = "ios") { "console.log(output);" } else { "Deno.stdout.writeSync(new TextEncoder().encode(output));" };
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
    {}
    "#,
        serde_json::to_string(raw_js).unwrap_or_default(),
        print
    );
    tokio::task::spawn_blocking(move || {
        // The site's script, so it gets a time limit: one that never ends
        // would leave Deno running, and the lookup waiting, for good.
        const LIMIT: std::time::Duration = std::time::Duration::from_secs(15);
        #[cfg(target_os = "ios")]
        return in_app(&script, LIMIT);
        #[cfg(not(target_os = "ios"))]
        {
            use std::io::{Read, Write};
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
        }
    })
    .await
    .ok()
    .flatten()
}

/// iOS: the script in the app's own engine (platform::python's "js").
#[cfg(target_os = "ios")]
fn in_app(script: &str, limit: std::time::Duration) -> Option<String> {
    use std::io::Read;
    let mut run = crate::platform::python::run("js", &[script.to_string()]).ok()?;
    let mut stdout = run.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.read_to_end(&mut text);
        text
    });
    let started = std::time::Instant::now();
    let success = loop {
        match run.try_wait() {
            Ok(Some(success)) => break success,
            Ok(None) if started.elapsed() < limit => std::thread::sleep(std::time::Duration::from_millis(50)),
            _ => {
                log::warn!("unpacking a script took too long; stopped it");
                run.kill_tree();
                return None;
            }
        }
    };
    let text = String::from_utf8(reader.join().ok()?).ok()?;
    let text = text.trim_end_matches('\n').to_string();
    (success && !text.is_empty()).then_some(text)
}
