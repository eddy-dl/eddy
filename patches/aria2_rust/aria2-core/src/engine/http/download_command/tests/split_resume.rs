//! Split HTTP downloads that stop early: a pause mid-piece, and a
//! recoverable failure through the sequential fallback, each followed by a
//! new group for the same output.

use super::*;

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

use crate::filesystem::control_file::ControlFile;

const MIB: u64 = 1024 * 1024;
const TOTAL: u64 = 4 * MIB;

fn payload() -> Vec<u8> {
    (0..TOTAL).map(|index| (index * 7 % 251) as u8).collect()
}

enum Reply {
    /// The whole requested range.
    Full,
    /// This many bytes, then the connection hangs.
    Hang(u64),
    /// This many bytes, then the connection closes early.
    Close(u64),
    Status(u16),
}

type Policy = Arc<dyn Fn(u64, u64) -> BoxFuture<'static, Reply> + Send + Sync>;

struct Server {
    address: std::net::SocketAddr,
    /// (start, end) of each Range request other than the bytes=0-0 probe.
    ranges: Arc<Mutex<Vec<(u64, u64)>>>,
    /// Range requests answered in full.
    full: Arc<AtomicUsize>,
    policy: Arc<Mutex<Policy>>,
}

impl Server {
    async fn start(policy: Policy) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fixture");
        let address = listener.local_addr().expect("fixture address");
        let ranges = Arc::new(Mutex::new(Vec::new()));
        let full = Arc::new(AtomicUsize::new(0));
        let policy = Arc::new(Mutex::new(policy));
        let (ranges_task, full_task, policy_task) =
            (Arc::clone(&ranges), Arc::clone(&full), Arc::clone(&policy));
        tokio::spawn(async move {
            let body = Arc::new(payload());
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (body, ranges, full, policy) = (
                    Arc::clone(&body),
                    Arc::clone(&ranges_task),
                    Arc::clone(&full_task),
                    Arc::clone(&policy_task),
                );
                tokio::spawn(async move {
                    let request = read_http_request(&mut stream).await;
                    if request.starts_with("HEAD ") {
                        let _ = stream
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {TOTAL}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                                )
                                .as_bytes(),
                            )
                            .await;
                        return;
                    }
                    let (start, end) = request
                        .lines()
                        .find_map(|line| {
                            let value = line.to_ascii_lowercase();
                            let range = value.strip_prefix("range: bytes=")?.to_owned();
                            let (start, end) = range.split_once('-')?;
                            let start: u64 = start.trim().parse().ok()?;
                            let end = end.trim().parse().unwrap_or(TOTAL - 1);
                            Some((start, end))
                        })
                        .unwrap_or((0, TOTAL - 1));
                    let reply = if end == 0 {
                        Reply::Full
                    } else {
                        ranges.lock().unwrap().push((start, end));
                        let policy = Arc::clone(&*policy.lock().unwrap());
                        policy(start, end).await
                    };
                    let length = end - start + 1;
                    let sent = match reply {
                        Reply::Status(code) => {
                            let _ = stream
                                .write_all(
                                    format!(
                                        "HTTP/1.1 {code} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                    )
                                    .as_bytes(),
                                )
                                .await;
                            return;
                        }
                        Reply::Full => length,
                        Reply::Hang(bytes) | Reply::Close(bytes) => bytes.min(length),
                    };
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/{TOTAL}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
                    );
                    let range = &body[start as usize..(start + sent) as usize];
                    if stream.write_all(head.as_bytes()).await.is_err()
                        || stream.write_all(range).await.is_err()
                    {
                        return;
                    }
                    let _ = stream.flush().await;
                    match reply {
                        Reply::Full if end > 0 => {
                            full.fetch_add(1, Ordering::SeqCst);
                        }
                        Reply::Hang(_) => tokio::time::sleep(Duration::from_secs(120)).await,
                        _ => {}
                    }
                });
            }
        });
        Self {
            address,
            ranges,
            full,
            policy,
        }
    }

    fn set_policy(&self, policy: Policy) {
        *self.policy.lock().unwrap() = policy;
        self.ranges.lock().unwrap().clear();
    }

    fn uri(&self) -> String {
        format!("http://{}/payload.bin", self.address)
    }
}

fn full_policy() -> Policy {
    Arc::new(|_, _| Box::pin(async { Reply::Full }))
}

/// Eddy's options: HEAD first, continue, split, an output name.
fn options(directory: &std::path::Path) -> DownloadOptions {
    DownloadOptions {
        dir: Some(directory.to_string_lossy().into_owned()),
        continue_download: true,
        use_head: true,
        split: Some(4),
        max_connection_per_server: Some(4),
        max_retries: 2,
        retry_wait: 0,
        ..DownloadOptions::default()
    }
}

fn command(gid: u64, uri: &str, options: &DownloadOptions) -> (DownloadCommand, Arc<std::sync::RwLock<RequestGroup>>) {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(gid),
        vec![uri.to_string()],
        options.clone(),
    )));
    let command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        uri,
        options,
        None,
        Some("payload.bin.part"),
    )
    .expect("create split resume command");
    (command, group)
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_mid_piece_resumes_from_in_flight_offset() {
    let server = Server::start(Arc::new(|start, _| {
        Box::pin(async move {
            if start == MIB {
                Reply::Hang(MIB / 2)
            } else {
                Reply::Full
            }
        })
    }))
    .await;
    let directory = tempfile::tempdir().expect("create output directory");
    let output = directory.path().join("payload.bin.part");
    let options = options(directory.path());

    let (mut first, group) = command(2001, &server.uri(), &options);
    let run = tokio::spawn(async move { first.execute().await });
    let full = Arc::clone(&server.full);
    wait_until("the other pieces", || full.load(Ordering::SeqCst) >= 3).await;
    // Let the client write what it received of the hanging piece.
    tokio::time::sleep(Duration::from_millis(500)).await;
    group.recover_mut().pause().expect("pause download");
    let result = tokio::time::timeout(Duration::from_secs(10), run)
        .await
        .expect("paused download should stop")
        .expect("download task");
    assert!(result.is_err(), "a paused download does not complete");

    let control = ControlFile::load(&ControlFile::control_path_for(&output))
        .await
        .expect("load control file")
        .expect("control file kept on pause");
    assert!(control.is_piece_done(0) && control.is_piece_done(2) && control.is_piece_done(3));
    assert!(!control.is_piece_done(1));
    assert_eq!(control.in_flight_prefix(1), MIB / 2);

    server.set_policy(full_policy());
    let (mut second, _) = command(2002, &server.uri(), &options);
    tokio::time::timeout(Duration::from_secs(20), second.execute())
        .await
        .expect("resumed download should not hang")
        .expect("resumed download should complete");

    assert_eq!(
        server.ranges.lock().unwrap().clone(),
        vec![(MIB + MIB / 2, 2 * MIB - 1)],
        "only the rest of the paused piece is fetched"
    );
    assert_eq!(tokio::fs::read(&output).await.unwrap(), payload());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_failed_fallback_keeps_completed_pieces() {
    // Piece 1 is refused once the others are served, which makes the
    // concurrent attempt fall back to sequential gaps; those then break off
    // after 256 KiB until the attempt fails.
    let piece_one = Arc::new(AtomicUsize::new(0));
    let server = Server::start(full_policy()).await;
    let server_full = Arc::clone(&server.full);
    let policy: Policy = {
        let piece_one = Arc::clone(&piece_one);
        Arc::new(move |start, _| {
            let (server_full, piece_one) = (Arc::clone(&server_full), Arc::clone(&piece_one));
            Box::pin(async move {
                if start != MIB {
                    return Reply::Full;
                }
                if piece_one.fetch_add(1, Ordering::SeqCst) > 0 {
                    return Reply::Close(MIB / 4);
                }
                while server_full.load(Ordering::SeqCst) < 3 {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                // Let the client finish those pieces first.
                tokio::time::sleep(Duration::from_millis(500)).await;
                Reply::Status(416)
            })
        })
    };
    server.set_policy(policy);
    let directory = tempfile::tempdir().expect("create output directory");
    let output = directory.path().join("payload.bin.part");
    let options = options(directory.path());

    let (mut first, _) = command(2003, &server.uri(), &options);
    let result = tokio::time::timeout(Duration::from_secs(20), first.execute())
        .await
        .expect("failing download should not hang");
    assert!(
        matches!(result, Err(Aria2Error::Recoverable(_))),
        "the attempt fails with a recoverable error: {result:?}"
    );
    assert!(piece_one.load(Ordering::SeqCst) >= 2, "the gap download ran");

    let control = ControlFile::load(&ControlFile::control_path_for(&output))
        .await
        .expect("load control file")
        .expect("control file kept after the failure");
    assert!(control.is_piece_done(0) && control.is_piece_done(2) && control.is_piece_done(3));
    assert_eq!(control.in_flight_prefix(1), MIB / 4);

    // Eddy's retry: a new group for the same output.
    server.set_policy(full_policy());
    let (mut retry, _) = command(2004, &server.uri(), &options);
    tokio::time::timeout(Duration::from_secs(20), retry.execute())
        .await
        .expect("retry should not hang")
        .expect("retry should complete");

    assert_eq!(
        server.ranges.lock().unwrap().clone(),
        vec![(MIB + MIB / 4, 2 * MIB - 1)],
        "the retry fetches only what the failed attempt did not finish"
    );
    assert_eq!(tokio::fs::read(&output).await.unwrap(), payload());
}
