//! A sequential download whose connection drops mid-stream, and a split
//! download from a server that answers 429 beyond a few connections.

use super::*;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::filesystem::control_file::ControlFile;

const MIB: u64 = 1024 * 1024;

fn payload(total: u64) -> Vec<u8> {
    (0..total).map(|index| (index * 13 % 247) as u8).collect()
}

/// The `bytes=N-` start of a request, if it has one.
fn range_start(request: &str) -> Option<u64> {
    request.lines().find_map(|line| {
        let value = line.to_ascii_lowercase();
        let range = value.strip_prefix("range: bytes=")?;
        range.split('-').next()?.trim().parse().ok()
    })
}

/// Answer HEAD with 405, as some servers do: the download then runs
/// sequentially and learns the size from the GET.
async fn refuse_head(stream: &mut TcpStream) {
    let _ = stream
        .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await;
}

/// Send `body[start..]` (206 when `start > 0`), but only `cut` bytes of it,
/// then drop the connection.
async fn serve_from(stream: &mut TcpStream, body: &[u8], start: u64, cut: Option<u64>) {
    let total = body.len() as u64;
    let length = total - start;
    let head = if start > 0 {
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{}/{total}\r\nConnection: close\r\n\r\n",
            total - 1
        )
    } else {
        format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n")
    };
    let end = start + cut.unwrap_or(length).min(length);
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&body[start as usize..end as usize]).await;
    let _ = stream.flush().await;
}

fn sequential_options(directory: &std::path::Path) -> DownloadOptions {
    DownloadOptions {
        dir: Some(directory.to_string_lossy().into_owned()),
        continue_download: true,
        use_head: true,
        max_retries: 2,
        retry_wait: 0,
        ..DownloadOptions::default()
    }
}

fn new_command(gid: u64, uri: &str, options: &DownloadOptions) -> DownloadCommand {
    DownloadCommand::new(GroupId::new(gid), uri, options, None, Some("stream.bin.part"))
        .expect("create download command")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_retry_in_group_continues_where_the_stream_broke() {
    const TOTAL: u64 = 8 * MIB;
    const CUT: u64 = 3 * MIB + 12_345;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let server_ranges = Arc::clone(&ranges);
    tokio::spawn(async move {
        let body = payload(TOTAL);
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let request = read_http_request(&mut stream).await;
            if request.starts_with("HEAD ") {
                refuse_head(&mut stream).await;
                continue;
            }
            let start = range_start(&request).unwrap_or(0);
            server_ranges.lock().unwrap().push(start);
            let cut = (start == 0).then_some(CUT);
            serve_from(&mut stream, &body, start, cut).await;
        }
    });

    let directory = tempfile::tempdir().unwrap();
    let options = sequential_options(directory.path());
    let mut command = new_command(3001, &format!("http://{address}/stream.bin"), &options);
    tokio::time::timeout(Duration::from_secs(20), command.execute())
        .await
        .expect("download should not hang")
        .expect("the retry in the group completes it");

    assert_eq!(*ranges.lock().unwrap(), vec![0, CUT], "the retry resumes at the break");
    assert_eq!(
        tokio::fs::read(directory.path().join("stream.bin.part")).await.unwrap(),
        payload(TOTAL)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sequential_failure_checkpoints_the_exact_break_for_a_new_group() {
    const TOTAL: u64 = 8 * MIB;
    const CUT: u64 = 3 * MIB + 12_345;
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("stream.bin.part");
    let options = sequential_options(directory.path());

    // The first server breaks the stream, then goes away.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let body = payload(TOTAL);
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            if request.starts_with("HEAD ") {
                refuse_head(&mut stream).await;
                continue;
            }
            serve_from(&mut stream, &body, 0, Some(CUT)).await;
            return;
        }
    });
    let mut command = new_command(3002, &format!("http://{address}/stream.bin"), &options);
    let first = tokio::spawn(async move { command.execute().await });
    server.await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(20), first)
        .await
        .expect("failing download should not hang")
        .unwrap();
    assert!(matches!(result, Err(Aria2Error::Recoverable(_))), "{result:?}");

    let control = ControlFile::load(&ControlFile::control_path_for(&output))
        .await
        .unwrap()
        .expect("control file kept after the failure");
    assert_eq!(control.completed_length(), CUT, "the checkpoint is where the bytes end");

    // Eddy's retry: a new group for the same output.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let server_ranges = Arc::clone(&ranges);
    tokio::spawn(async move {
        let body = payload(TOTAL);
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let request = read_http_request(&mut stream).await;
            if request.starts_with("HEAD ") {
                refuse_head(&mut stream).await;
                continue;
            }
            let start = range_start(&request).unwrap_or(0);
            server_ranges.lock().unwrap().push(start);
            serve_from(&mut stream, &body, start, None).await;
        }
    });
    let mut retry = new_command(3003, &format!("http://{address}/stream.bin"), &options);
    tokio::time::timeout(Duration::from_secs(20), retry.execute())
        .await
        .expect("retry should not hang")
        .expect("retry should complete");
    assert_eq!(*ranges.lock().unwrap(), vec![CUT]);
    assert_eq!(tokio::fs::read(&output).await.unwrap(), payload(TOTAL));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn too_many_connections_back_off_instead_of_failing() {
    const TOTAL: u64 = 16 * MIB;
    const LIMIT: usize = 3;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::new(AtomicUsize::new(0));
    let (server_active, server_rejected) = (Arc::clone(&active), Arc::clone(&rejected));
    tokio::spawn(async move {
        let body = Arc::new(payload(TOTAL));
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let (body, active, rejected) = (
                Arc::clone(&body),
                Arc::clone(&server_active),
                Arc::clone(&server_rejected),
            );
            tokio::spawn(async move {
                let request = read_http_request(&mut stream).await;
                if request.starts_with("HEAD ") {
                    let _ = stream
                        .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {TOTAL}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n").as_bytes())
                        .await;
                    return;
                }
                let (start, end) = request
                    .lines()
                    .find_map(|line| {
                        let value = line.to_ascii_lowercase();
                        let (start, end) =
                            value.strip_prefix("range: bytes=")?.split_once('-')?;
                        Some((start.trim().parse::<u64>().ok()?, end.trim().parse::<u64>().ok()?))
                    })
                    .unwrap_or((0, TOTAL - 1));
                // The 1-byte probe doesn't count against the limit.
                let counted = end > 0;
                if counted && active.fetch_add(1, Ordering::SeqCst) >= LIMIT {
                    active.fetch_sub(1, Ordering::SeqCst);
                    rejected.fetch_add(1, Ordering::SeqCst);
                    let _ = stream
                        .write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                let length = end - start + 1;
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/{TOTAL}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(head.as_bytes()).await;
                // Slow enough that the ranges overlap.
                for chunk in body[start as usize..=end as usize].chunks(256 * 1024) {
                    if stream.write_all(chunk).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                let _ = stream.flush().await;
                if counted {
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            });
        }
    });

    let directory = tempfile::tempdir().unwrap();
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        continue_download: true,
        use_head: true,
        split: Some(16),
        max_connection_per_server: Some(16),
        max_retries: 2,
        retry_wait: 0,
        ..DownloadOptions::default()
    };
    let mut command = new_command(3004, &format!("http://{address}/stream.bin"), &options);
    tokio::time::timeout(Duration::from_secs(60), command.execute())
        .await
        .expect("download should not hang")
        .expect("429s must not fail the download");

    assert_eq!(
        tokio::fs::read(directory.path().join("stream.bin.part")).await.unwrap(),
        payload(TOTAL)
    );
    // The first burst is refused (up to 13 of 16 ranges); after that the
    // client stays at what the server takes, apart from a rare probe up.
    let rejected = rejected.load(Ordering::SeqCst);
    assert!(rejected >= 1, "the limit was hit");
    assert!(rejected <= 13 + 4, "connections settled at the limit: {rejected} rejections");
}
