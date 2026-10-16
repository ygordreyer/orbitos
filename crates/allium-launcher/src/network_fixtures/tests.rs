use crate::network_cache::*;
use crate::network_fs::*;
use crate::network_library::*;
use crate::network_transfer::*;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "allium-network-fixture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(fs::canonicalize(root).unwrap())
    }
    fn cache(&self) -> Cache {
        Cache::new(CacheConfig::bind(self.0.clone(), Some(self.0.join("stay_awake"))).unwrap())
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn sha(data: &[u8]) -> String {
    digest_hex(Sha256::digest(data).as_slice())
}
fn game(data: &[u8]) -> NetworkGame {
    let revision = "b".repeat(64);
    NetworkGame {
        id: "fixture".into(),
        display_title: "Fixture".into(),
        platform: "PS".into(),
        bundle_revision: revision.clone(),
        game_path: "Roms/PS/Fixture.bin".into(),
        cache_path: "Roms/PS/Fixture.bin".into(),
        install_kind: "file".into(),
        launch_path: "Roms/PS/Fixture.bin".into(),
        files: vec![Payload {
            id: "disc".into(),
            relative_path: String::new(),
            bytes: data.len() as u64,
            sha256: sha(data),
            role: "rom".into(),
            http_route: format!("/v1/files/fixture/{revision}/disc"),
        }],
        discs: vec![],
    }
}
struct Server {
    endpoint: Endpoint,
    requests: Arc<Mutex<Vec<String>>>,
    thread: Option<thread::JoinHandle<()>>,
}
struct ResponseScript {
    header_chunks: Vec<(Duration, Vec<u8>)>,
    body_chunks: Vec<(Duration, Vec<u8>)>,
    close_delay: Duration,
}
impl ResponseScript {
    fn immediate(header: String, body: Vec<u8>, delay_ms: u64) -> Self {
        Self {
            header_chunks: vec![(Duration::from_millis(delay_ms), header.into_bytes())],
            body_chunks: vec![(Duration::ZERO, body)],
            close_delay: Duration::ZERO,
        }
    }
}
impl Server {
    fn new(replies: Vec<(String, Vec<u8>, u64)>) -> Self {
        Self::scripted(
            replies
                .into_iter()
                .map(|(header, body, delay)| ResponseScript::immediate(header, body, delay))
                .collect(),
        )
    }
    fn scripted(replies: Vec<ResponseScript>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = Endpoint::lan(listener.local_addr().unwrap()).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let record = requests.clone();
        let thread = thread::spawn(move || {
            for reply in replies {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((s, _)) => break s,
                        Err(_) if Instant::now() < deadline => {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(_) => return,
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        return;
                    }
                    request.push(byte[0]);
                }
                record
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(request).unwrap());
                for (delay, bytes) in reply.header_chunks {
                    thread::sleep(delay);
                    if stream.write_all(&bytes).is_err() {
                        break;
                    }
                }
                for (delay, bytes) in reply.body_chunks {
                    thread::sleep(delay);
                    if stream.write_all(&bytes).is_err() {
                        break;
                    }
                }
                thread::sleep(reply.close_delay);
            }
        });
        Self {
            endpoint,
            requests,
            thread: Some(thread),
        }
    }
}
fn scripted_response(
    file: &Payload,
    offset: u64,
    first_body_delay: Duration,
    body_chunks: Vec<(Duration, Vec<u8>)>,
) -> ResponseScript {
    let (header, _, _) = response(file, offset, &[]);
    let mut chunks = vec![(first_body_delay, Vec::new())];
    chunks.extend(body_chunks);
    ResponseScript {
        header_chunks: vec![(Duration::ZERO, header.into_bytes())],
        body_chunks: chunks,
        close_delay: Duration::ZERO,
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}
fn response(file: &Payload, offset: u64, body: &[u8]) -> (String, Vec<u8>, u64) {
    let range = if offset == 0 {
        String::new()
    } else {
        format!(
            "Content-Range: bytes {offset}-{}/{}\r\n",
            file.bytes - 1,
            file.bytes
        )
    };
    (
        format!(
            "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nETag: \"{}\"\r\n{range}Connection: close\r\n\r\n",
            if offset == 0 { 200 } else { 206 },
            file.bytes - offset,
            file.sha256
        ),
        body.to_vec(),
        0,
    )
}
struct Plenty;
impl SpaceProbe for Plenty {
    fn observe(&self, _: &AnchoredFs) -> anyhow::Result<Space> {
        Ok(Space {
            free_bytes: u64::MAX / 2,
            allocation_unit: 4096,
        })
    }
}
struct SlowWriter {
    bytes: Vec<u8>,
    delay: Duration,
}
impl Write for SlowWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        thread::sleep(self.delay);
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn run(
    cache: &Cache,
    g: &NetworkGame,
    s: &Server,
    space: &dyn SpaceProbe,
) -> anyhow::Result<CacheState> {
    let (tx, _) = mpsc::sync_channel(8);
    cache.run(g, &s.endpoint, &Cancellation::default(), space, &tx)
}
fn job(root: &Path, g: &NetworkGame) -> PathBuf {
    root.join(".allium/state/network-library/jobs")
        .join(&g.id)
        .join(&g.bundle_revision)
}
#[test]
fn full_install_identity_ready_and_lease_ownership() {
    let f = Fixture::new();
    let g = game(b"fake-rom");
    fs::write(f.0.join("stay_awake"), b"preexisting").unwrap();
    let s = Server::new(vec![response(&g.files[0], 0, b"fake-rom")]);
    let cache = f.cache();
    assert!(
        run(&cache, &g, &s, &Plenty)
            .unwrap()
            .permit_launch(&g, LaunchRoute::Play)
    );
    assert_eq!(fs::read(f.0.join(&g.launch_path)).unwrap(), b"fake-rom");
    assert_eq!(fs::read(f.0.join("stay_awake")).unwrap(), b"preexisting");
    assert!(
        cache
            .ready(&g, &Cancellation::default())
            .unwrap()
            .permit_launch(&g, LaunchRoute::Resume)
    );
    let mut different = g.clone();
    different.bundle_revision = "a".repeat(64);
    assert!(cache.ready(&different, &Cancellation::default()).is_err());
}
#[test]
fn truncated_transfer_resumes_verified_prefix_and_exact_range() {
    let f = Fixture::new();
    let bytes = vec![7u8; 128 * 1024];
    let g = game(&bytes);
    let cache = f.cache();
    let first = Server::new(vec![response(&g.files[0], 0, &bytes[..4096])]);
    assert!(run(&cache, &g, &first, &Plenty).is_err());
    assert!(!f.0.join(&g.cache_path).exists());
    assert!(!f.0.join("stay_awake").exists());
    drop(first);
    let next = Server::new(vec![response(&g.files[0], 4096, &bytes[4096..])]);
    assert!(run(&cache, &g, &next, &Plenty).is_ok());
    let requests = next.requests.lock().unwrap();
    assert!(requests[0].contains("Range: bytes=4096-\r\n"));
    assert!(requests[0].contains(&format!("If-Range: \"{}\"", g.files[0].sha256)));
}
#[test]
fn bad_hash_etag_lengths_and_trailing_bytes_never_install() {
    for mode in 0..5 {
        let f = Fixture::new();
        let g = game(b"good");
        let mut reply = response(&g.files[0], 0, if mode == 0 { b"evil" } else { b"good" });
        if mode == 1 {
            reply.0 = reply.0.replace(&g.files[0].sha256, &"a".repeat(64));
        }
        if mode == 2 {
            reply.0 = reply.0.replace("Content-Length: 4", "Content-Length: 5");
        }
        if mode == 3 {
            reply.1.push(1);
        }
        if mode == 4 {
            reply.0 = reply
                .0
                .replace("Connection: close", "Transfer-Encoding: chunked");
        }
        let s = Server::new(vec![reply]);
        assert!(run(&f.cache(), &g, &s, &Plenty).is_err());
        assert!(!f.0.join(&g.cache_path).exists());
        assert!(f.cache().ready(&g, &Cancellation::default()).is_err());
    }
}
#[test]
fn corrupted_resume_and_unregistered_targets_are_preserved() {
    let f = Fixture::new();
    let g = game(b"good-content");
    let first = Server::new(vec![response(&g.files[0], 0, b"good")]);
    assert!(run(&f.cache(), &g, &first, &Plenty).is_err());
    drop(first);
    let part = job(&f.0, &g).join("payloads/content.bin");
    fs::write(&part, b"evil").unwrap();
    let s = Server::new(vec![]);
    assert!(run(&f.cache(), &g, &s, &Plenty).is_err());
    assert_eq!(fs::read(&part).unwrap(), b"evil");
    let fresh = Fixture::new();
    fs::create_dir_all(fresh.0.join("Roms/PS")).unwrap();
    fs::write(fresh.0.join(&g.cache_path), b"user-ROM").unwrap();
    assert!(run(&fresh.cache(), &g, &s, &Plenty).is_err());
    assert_eq!(fs::read(fresh.0.join(&g.cache_path)).unwrap(), b"user-ROM");
}
struct SpaceLoss(AtomicUsize);
impl SpaceProbe for SpaceLoss {
    fn observe(&self, _: &AnchoredFs) -> anyhow::Result<Space> {
        Ok(Space {
            free_bytes: if self.0.fetch_add(1, Ordering::Relaxed) < 2 {
                u64::MAX / 2
            } else {
                RESERVE_BYTES
            },
            allocation_unit: 4096,
        })
    }
}
#[test]
fn space_loss_keeps_partial_and_recovers() {
    let f = Fixture::new();
    let bytes = vec![5; 256 * 1024];
    let g = game(&bytes);
    let s = Server::new(vec![response(&g.files[0], 0, &bytes)]);
    assert!(run(&f.cache(), &g, &s, &SpaceLoss(AtomicUsize::new(0))).is_err());
    drop(s);
    let length = fs::metadata(job(&f.0, &g).join("payloads/content.bin"))
        .unwrap()
        .len();
    assert!(length > 0 && length < bytes.len() as u64);
    assert!(f.cache().ready(&g, &Cancellation::default()).is_err());
    let s = Server::new(vec![response(
        &g.files[0],
        length,
        &bytes[length as usize..],
    )]);
    run(&f.cache(), &g, &s, &Plenty).unwrap();
}
#[test]
fn post_rename_pre_index_crash_recovers_only_exact_payload() {
    let f = Fixture::new();
    let g = game(b"good-content");
    let first = Server::new(vec![response(&g.files[0], 0, b"good")]);
    assert!(run(&f.cache(), &g, &first, &Plenty).is_err());
    drop(first);
    let path = job(&f.0, &g).join("job.json");
    let mut journal: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    journal["phase"] = "installing".into();
    fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
    fs::create_dir_all(f.0.join("Roms/PS")).unwrap();
    fs::write(job(&f.0, &g).join("payloads/content.bin"), b"good-content").unwrap();
    fs::rename(
        job(&f.0, &g).join("payloads/content.bin"),
        f.0.join(&g.cache_path),
    )
    .unwrap();
    let s = Server::new(vec![]);
    run(&f.cache(), &g, &s, &Plenty).unwrap();
    assert!(f.cache().ready(&g, &Cancellation::default()).is_ok());
}
#[test]
fn cancellation_timeout_and_duplicate_worker_are_bounded() {
    let g = game(b"good");
    let s = Server::new(vec![(
        response(&g.files[0], 0, b"good").0,
        b"good".to_vec(),
        300,
    )]);
    let cancel = Cancellation::default();
    let trigger = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        trigger.cancel();
    });
    let start = Instant::now();
    assert!(
        download(
            &s.endpoint,
            &g.files[0],
            0,
            &mut Vec::new(),
            &cancel,
            |_| Ok(())
        )
        .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(s);
    let s = Server::new(vec![(
        response(&g.files[0], 0, b"good").0,
        b"good".to_vec(),
        300,
    )]);
    let mut endpoint = s.endpoint.clone();
    endpoint.header_timeout = Duration::from_millis(120);
    assert!(
        download(
            &endpoint,
            &g.files[0],
            0,
            &mut Vec::new(),
            &Cancellation::default(),
            |_| Ok(())
        )
        .is_err()
    );
    drop(s);
    let f = Fixture::new();
    let s = Server::new(vec![(
        response(&g.files[0], 0, b"good").0,
        b"good".to_vec(),
        300,
    )]);
    let config = CacheConfig::bind(f.0.clone(), None).unwrap();
    let worker = Worker::start(config.clone(), g.clone(), s.endpoint.clone()).unwrap();
    assert!(Worker::start(config, g, s.endpoint.clone()).is_err());
    worker.cancel.cancel();
    assert!(
        worker
            .result
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .is_err()
    );
    worker.join().unwrap();
}
#[test]
fn validated_headers_receive_one_first_body_grace() {
    let g = game(b"good");
    let delayed = || {
        scripted_response(
            &g.files[0],
            0,
            Duration::from_millis(180),
            vec![(Duration::ZERO, b"good".to_vec())],
        )
    };
    let old_budget = Server::scripted(vec![delayed()]);
    let mut endpoint = old_budget.endpoint.clone();
    endpoint.idle_timeout = Duration::from_millis(70);
    endpoint.first_body_timeout = endpoint.idle_timeout;
    let mut output = Vec::new();
    assert!(
        download(
            &endpoint,
            &g.files[0],
            0,
            &mut output,
            &Cancellation::default(),
            |_| Ok(())
        )
        .unwrap_err()
        .to_string()
        .contains("HTTP first body timeout")
    );
    assert!(output.is_empty());
    drop(old_budget);

    let first_body_grace = Server::scripted(vec![delayed()]);
    let mut endpoint = first_body_grace.endpoint.clone();
    endpoint.idle_timeout = Duration::from_millis(70);
    endpoint.first_body_timeout = Duration::from_millis(400);
    download(
        &endpoint,
        &g.files[0],
        0,
        &mut output,
        &Cancellation::default(),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(output, b"good");
}
#[test]
fn first_body_grace_expires_without_writing() {
    let g = game(b"good");
    let server = Server::scripted(vec![scripted_response(
        &g.files[0],
        0,
        Duration::from_millis(350),
        vec![(Duration::ZERO, b"good".to_vec())],
    )]);
    let mut endpoint = server.endpoint.clone();
    endpoint.first_body_timeout = Duration::from_millis(120);
    let mut output = Vec::new();
    let started = Instant::now();
    let error = download(
        &endpoint,
        &g.files[0],
        0,
        &mut output,
        &Cancellation::default(),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("HTTP first body timeout"));
    assert!(started.elapsed() < Duration::from_millis(300));
    assert!(output.is_empty());
}
#[test]
fn first_body_byte_switches_to_steady_idle() {
    let g = game(b"good");
    let server = Server::scripted(vec![scripted_response(
        &g.files[0],
        0,
        Duration::from_millis(30),
        vec![
            (Duration::ZERO, b"g".to_vec()),
            (Duration::from_millis(260), b"ood".to_vec()),
        ],
    )]);
    let mut endpoint = server.endpoint.clone();
    endpoint.first_body_timeout = Duration::from_millis(600);
    endpoint.idle_timeout = Duration::from_millis(100);
    let mut output = Vec::new();
    let error = download(
        &endpoint,
        &g.files[0],
        0,
        &mut output,
        &Cancellation::default(),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("HTTP idle timeout"));
    assert_eq!(output, b"g");
}
#[test]
fn callback_latency_does_not_renew_steady_idle() {
    let g = game(b"good");
    let server = Server::scripted(vec![scripted_response(
        &g.files[0],
        0,
        Duration::ZERO,
        vec![
            (Duration::ZERO, b"g".to_vec()),
            (Duration::from_millis(240), b"ood".to_vec()),
        ],
    )]);
    let mut endpoint = server.endpoint.clone();
    endpoint.idle_timeout = Duration::from_millis(100);
    let mut output = Vec::new();
    let error = download(
        &endpoint,
        &g.files[0],
        0,
        &mut output,
        &Cancellation::default(),
        |_| {
            thread::sleep(Duration::from_millis(180));
            Ok(())
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("HTTP idle timeout"));
    assert_eq!(output, b"g");
}
#[test]
fn writer_latency_does_not_renew_final_eof_idle() {
    let g = game(b"good");
    let (header, _, _) = response(&g.files[0], 0, b"good");
    let server = Server::scripted(vec![ResponseScript {
        header_chunks: vec![(Duration::ZERO, header.into_bytes())],
        body_chunks: vec![(Duration::ZERO, b"good".to_vec())],
        close_delay: Duration::from_millis(240),
    }]);
    let mut endpoint = server.endpoint.clone();
    endpoint.idle_timeout = Duration::from_millis(100);
    let mut output = SlowWriter {
        bytes: Vec::new(),
        delay: Duration::from_millis(180),
    };
    let error = download(
        &endpoint,
        &g.files[0],
        0,
        &mut output,
        &Cancellation::default(),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("HTTP idle timeout"));
    assert_eq!(output.bytes, b"good");
}
#[test]
fn header_deadline_is_absolute_and_independent() {
    let g = game(b"good");
    let (header, _, _) = response(&g.files[0], 0, b"good");
    let split = header.len() / 3;
    let server = Server::scripted(vec![ResponseScript {
        header_chunks: vec![
            (
                Duration::from_millis(80),
                header.as_bytes()[..split].to_vec(),
            ),
            (
                Duration::from_millis(80),
                header.as_bytes()[split..split * 2].to_vec(),
            ),
            (
                Duration::from_millis(80),
                header.as_bytes()[split * 2..].to_vec(),
            ),
        ],
        body_chunks: vec![(Duration::ZERO, b"good".to_vec())],
        close_delay: Duration::ZERO,
    }]);
    let mut endpoint = server.endpoint.clone();
    endpoint.header_timeout = Duration::from_millis(190);
    endpoint.first_body_timeout = Duration::from_secs(1);
    let mut output = Vec::new();
    let error = download(
        &endpoint,
        &g.files[0],
        0,
        &mut output,
        &Cancellation::default(),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("HTTP headers too large/slow"));
    assert!(output.is_empty());
}
#[test]
fn cancellation_remains_prompt_during_first_body_wait() {
    let g = game(b"good");
    let server = Server::scripted(vec![scripted_response(
        &g.files[0],
        0,
        Duration::from_millis(500),
        vec![(Duration::ZERO, b"good".to_vec())],
    )]);
    let mut endpoint = server.endpoint.clone();
    endpoint.first_body_timeout = Duration::from_secs(2);
    let cancel = Cancellation::default();
    let trigger = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        trigger.cancel();
    });
    let started = Instant::now();
    let error = download(&endpoint, &g.files[0], 0, &mut Vec::new(), &cancel, |_| {
        Ok(())
    })
    .unwrap_err();
    assert!(error.to_string().contains("Cancelled"));
    assert!(started.elapsed() < Duration::from_millis(250));
}
#[test]
fn cancellation_remains_prompt_during_steady_body_wait() {
    let g = game(b"good");
    let server = Server::scripted(vec![scripted_response(
        &g.files[0],
        0,
        Duration::ZERO,
        vec![
            (Duration::ZERO, b"g".to_vec()),
            (Duration::from_millis(500), b"ood".to_vec()),
        ],
    )]);
    let cancel = Cancellation::default();
    let trigger = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        trigger.cancel();
    });
    let started = Instant::now();
    let mut output = Vec::new();
    let error = download(
        &server.endpoint,
        &g.files[0],
        0,
        &mut output,
        &cancel,
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("Cancelled"));
    assert!(started.elapsed() < Duration::from_millis(250));
    assert_eq!(output, b"g");
}
#[test]
fn range_resume_receives_first_body_grace() {
    let g = game(b"good-content");
    let offset = 4;
    let server = Server::scripted(vec![scripted_response(
        &g.files[0],
        offset,
        Duration::from_millis(180),
        vec![(Duration::ZERO, b"-content".to_vec())],
    )]);
    let mut endpoint = server.endpoint.clone();
    endpoint.idle_timeout = Duration::from_millis(70);
    endpoint.first_body_timeout = Duration::from_millis(400);
    let mut output = Vec::new();
    download(
        &endpoint,
        &g.files[0],
        offset,
        &mut output,
        &Cancellation::default(),
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(output, b"-content");
    let request = &server.requests.lock().unwrap()[0];
    assert!(request.contains("Range: bytes=4-"));
    assert!(request.contains(&format!("If-Range: \"{}\"", g.files[0].sha256)));
}
#[test]
fn final_eof_uses_steady_idle_timeout() {
    let g = game(b"good");
    let (header, _, _) = response(&g.files[0], 0, b"good");
    let server = Server::scripted(vec![ResponseScript {
        header_chunks: vec![(Duration::ZERO, header.into_bytes())],
        body_chunks: vec![(Duration::ZERO, b"good".to_vec())],
        close_delay: Duration::from_millis(260),
    }]);
    let mut endpoint = server.endpoint.clone();
    endpoint.first_body_timeout = Duration::from_millis(600);
    endpoint.idle_timeout = Duration::from_millis(100);
    let mut output = Vec::new();
    let error = download(
        &endpoint,
        &g.files[0],
        0,
        &mut output,
        &Cancellation::default(),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("HTTP idle timeout"));
    assert_eq!(output, b"good");
}
#[test]
fn actual_statvfs_and_symlink_target_guard() {
    let f = Fixture::new();
    let space = FilesystemSpace
        .observe(&AnchoredFs::new(&f.0).unwrap())
        .unwrap();
    assert!(space.free_bytes > RESERVE_BYTES && space.allocation_unit > 0);
    fs::create_dir_all(f.0.join("Roms")).unwrap();
    std::os::unix::fs::symlink(f.0.join("outside"), f.0.join("Roms/PS")).unwrap();
    assert!(CacheConfig::bind(f.0.clone(), None).is_err());
    assert!(!f.0.join("outside").exists());
}

fn multi_game() -> (NetworkGame, Vec<Vec<u8>>) {
    let bodies = vec![
        b"disc-one".to_vec(),
        b"disc-two".to_vec(),
        b"disc1.bin\ndisc2.bin\n".to_vec(),
    ];
    let mut g = game(&bodies[0]);
    g.cache_path = "Roms/PS/Multi.m3u".into();
    g.game_path = g.cache_path.clone();
    g.launch_path = "Roms/PS/Multi.m3u/Multi.m3u".into();
    g.install_kind = "directory".into();
    g.files = bodies
        .iter()
        .enumerate()
        .map(|(index, body)| {
            let id = format!("file{index}");
            Payload {
                id: id.clone(),
                relative_path: if index < 2 {
                    format!("disc{}.bin", index + 1)
                } else {
                    "Multi.m3u".into()
                },
                bytes: body.len() as u64,
                sha256: sha(body),
                role: if index < 2 {
                    "rom".into()
                } else {
                    "playlist".into()
                },
                http_route: format!("/v1/files/{}/{}/{id}", g.id, g.bundle_revision),
            }
        })
        .collect();
    g.discs = vec!["file0".into(), "file1".into()];
    (g, bodies)
}
#[test]
fn directory_group_installs_atomically_with_all_discs() {
    let f = Fixture::new();
    let (g, bodies) = multi_game();
    let s = Server::new(
        g.files
            .iter()
            .zip(&bodies)
            .map(|(file, body)| response(file, 0, body))
            .collect(),
    );
    run(&f.cache(), &g, &s, &Plenty).unwrap();
    assert!(
        f.cache()
            .ready(&g, &Cancellation::default())
            .unwrap()
            .permit_launch(&g, LaunchRoute::Play)
    );
    assert_eq!(fs::read(f.0.join(&g.launch_path)).unwrap(), bodies[2]);
    assert!(!job(&f.0, &g).join("payloads").exists());
}
#[test]
fn unexpected_staging_directory_never_promotes() {
    let f = Fixture::new();
    let (g, bodies) = multi_game();
    let first = Server::new(vec![response(&g.files[0], 0, &bodies[0][..2])]);
    assert!(run(&f.cache(), &g, &first, &Plenty).is_err());
    drop(first);
    fs::create_dir(job(&f.0, &g).join("payloads/user-directory")).unwrap();
    let s = Server::new(vec![
        response(&g.files[0], 2, &bodies[0][2..]),
        response(&g.files[1], 0, &bodies[1]),
        response(&g.files[2], 0, &bodies[2]),
    ]);
    assert!(run(&f.cache(), &g, &s, &Plenty).is_err());
    assert!(!f.0.join(&g.cache_path).exists());
    assert!(job(&f.0, &g).join("payloads/user-directory").is_dir());
}
struct CancelDuringWrite {
    cancel: Cancellation,
    observations: AtomicUsize,
}
impl SpaceProbe for CancelDuringWrite {
    fn observe(&self, _: &AnchoredFs) -> anyhow::Result<Space> {
        if self.observations.fetch_add(1, Ordering::Relaxed) == 2 {
            self.cancel.cancel();
        }
        Ok(Space {
            free_bytes: u64::MAX / 2,
            allocation_unit: 4096,
        })
    }
}
#[test]
fn cancel_during_download_keeps_verified_resumable_checkpoint() {
    let f = Fixture::new();
    let bytes = vec![9; 2 * 1024 * 1024];
    let g = game(&bytes);
    let s = Server::new(vec![response(&g.files[0], 0, &bytes)]);
    let cancel = Cancellation::default();
    let (tx, _) = mpsc::sync_channel(8);
    let probe = CancelDuringWrite {
        cancel: cancel.clone(),
        observations: AtomicUsize::new(0),
    };
    assert!(
        f.cache()
            .run(&g, &s.endpoint, &cancel, &probe, &tx)
            .is_err()
    );
    drop(s);
    let part = fs::read(job(&f.0, &g).join("payloads/content.bin")).unwrap();
    assert!(!part.is_empty() && part.len() < bytes.len());
    assert!(f.cache().ready(&g, &Cancellation::default()).is_err());
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(job(&f.0, &g).join("job.json")).unwrap()).unwrap();
    assert_eq!(
        metadata["partials"][0]["prefix_sha256"].as_str().unwrap(),
        sha(&part)
    );
    let s = Server::new(vec![response(
        &g.files[0],
        part.len() as u64,
        &bytes[part.len()..],
    )]);
    run(&f.cache(), &g, &s, &Plenty).unwrap();
}
#[test]
fn resume_rejects_200_or_wrong_content_range() {
    for mode in 0..2 {
        let f = Fixture::new();
        let g = game(b"good-content");
        let first = Server::new(vec![response(&g.files[0], 0, b"good")]);
        assert!(run(&f.cache(), &g, &first, &Plenty).is_err());
        drop(first);
        let mut reply = response(&g.files[0], 4, b"-content");
        if mode == 0 {
            reply.0 = reply.0.replace("206 OK", "200 OK");
        } else {
            reply.0 = reply.0.replace("bytes 4-11/12", "bytes 3-11/12");
        }
        let s = Server::new(vec![reply]);
        assert!(run(&f.cache(), &g, &s, &Plenty).is_err());
        assert_eq!(
            fs::read(job(&f.0, &g).join("payloads/content.bin")).unwrap(),
            b"good"
        );
        assert!(f.cache().ready(&g, &Cancellation::default()).is_err());
    }
}

fn write_repeated(path: &Path, byte: u8, chunks: usize) -> String {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = fs::File::create(path).unwrap();
    let chunk = [byte; WINDOW];
    let mut hash = Sha256::new();
    for _ in 0..chunks {
        file.write_all(&chunk).unwrap();
        hash.update(chunk);
    }
    digest_hex(hash.finalize().as_slice())
}
fn streamed_game(byte: u8, chunks: usize) -> NetworkGame {
    let mut g = game(b"x");
    let chunk = [byte; WINDOW];
    let mut hash = Sha256::new();
    for _ in 0..chunks {
        hash.update(chunk);
    }
    g.files[0].bytes = (WINDOW * chunks) as u64;
    g.files[0].sha256 = digest_hex(hash.finalize().as_slice());
    g
}
fn checkpoint_fixture(root: &Path, g: &NetworkGame, prefix_chunks: usize, byte: u8) {
    let hash = write_repeated(
        &job(root, g).join("payloads/content.bin"),
        byte,
        prefix_chunks,
    );
    let value = serde_json::json!({"version":1,"game":g,"phase":"downloading","partials":[{"id":g.files[0].id,"bytes":WINDOW*prefix_chunks,"prefix_sha256":hash}]});
    fs::write(
        job(root, g).join("job.json"),
        serde_json::to_vec(&value).unwrap(),
    )
    .unwrap();
}
fn assert_lease(root: &Path, preexisting: bool) {
    if preexisting {
        assert_eq!(fs::read(root.join("stay_awake")).unwrap(), b"preexisting");
    } else {
        assert!(!root.join("stay_awake").exists());
    }
}
#[test]
fn cancel_mid_prefix_hash_never_returns_ready_and_releases_only_own_lease() {
    for preexisting in [false, true] {
        let f = Fixture::new();
        let g = streamed_game(7, 5);
        checkpoint_fixture(&f.0, &g, 4, 7);
        if preexisting {
            fs::write(f.0.join("stay_awake"), b"preexisting").unwrap();
        }
        let before = fs::read(job(&f.0, &g).join("job.json")).unwrap();
        let cancel = Cancellation::default();
        cancel.cancel_after_verification_reads(2);
        let (tx, _) = mpsc::sync_channel(8);
        let s = Server::new(vec![]);
        let result = f.cache().run(&g, &s.endpoint, &cancel, &Plenty, &tx);
        assert!(result.unwrap_err().to_string().contains("Cancelled"));
        assert_eq!(cancel.verification_reads(), 2);
        assert_eq!(
            fs::metadata(job(&f.0, &g).join("payloads/content.bin"))
                .unwrap()
                .len(),
            4 * WINDOW as u64
        );
        assert_eq!(fs::read(job(&f.0, &g).join("job.json")).unwrap(), before);
        assert!(!f.0.join(&g.cache_path).exists());
        assert_lease(&f.0, preexisting);
    }
}
#[test]
fn cancel_mid_ready_hash_never_returns_launch_continuation() {
    for preexisting in [false, true] {
        let f = Fixture::new();
        let g = streamed_game(9, 4);
        write_repeated(&f.0.join(&g.cache_path), 9, 4);
        let index =
            f.0.join(".allium/state/network-library/installed/fixture.json");
        fs::create_dir_all(index.parent().unwrap()).unwrap();
        let original = serde_json::to_vec(&serde_json::json!({"version":1,"game":g})).unwrap();
        fs::write(&index, &original).unwrap();
        if preexisting {
            fs::write(f.0.join("stay_awake"), b"preexisting").unwrap();
        }
        let cancel = Cancellation::default();
        cancel.cancel_after_verification_reads(2);
        let (tx, _) = mpsc::sync_channel(8);
        let s = Server::new(vec![]);
        assert!(
            f.cache()
                .run(&g, &s.endpoint, &cancel, &Plenty, &tx)
                .unwrap_err()
                .to_string()
                .contains("Cancelled")
        );
        assert_eq!(cancel.verification_reads(), 2);
        assert_eq!(fs::read(index).unwrap(), original);
        assert_lease(&f.0, preexisting);
        assert!(f.cache().ready(&g, &cancel).is_err());
    }
}
#[test]
fn controlled_job_and_ready_scratch_recovery() {
    let f = Fixture::new();
    let g = game(b"good-content");
    let first = Server::new(vec![response(&g.files[0], 0, b"good")]);
    assert!(run(&f.cache(), &g, &first, &Plenty).is_err());
    drop(first);
    let journal = job(&f.0, &g).join("job.json");
    fs::rename(&journal, journal.with_extension("new")).unwrap();
    let s = Server::new(vec![response(&g.files[0], 4, b"-content")]);
    run(&f.cache(), &g, &s, &Plenty).unwrap();
    assert!(!journal.with_extension("new").exists());
    drop(s);
    let index =
        f.0.join(".allium/state/network-library/installed/fixture.json");
    fs::rename(&index, index.with_extension("new")).unwrap();
    let s = Server::new(vec![]);
    assert!(
        run(&f.cache(), &g, &s, &Plenty)
            .unwrap()
            .permit_launch(&g, LaunchRoute::Play)
    );
    assert!(!index.with_extension("new").exists());
}
#[test]
fn unknown_torn_or_changed_scratch_is_preserved() {
    for mode in 0..4 {
        let f = Fixture::new();
        let g = game(b"good-content");
        let first = Server::new(vec![response(&g.files[0], 0, b"good")]);
        assert!(run(&f.cache(), &g, &first, &Plenty).is_err());
        drop(first);
        let journal = job(&f.0, &g).join("job.json");
        let old = fs::read(&journal).unwrap();
        let mut candidate: serde_json::Value = serde_json::from_slice(&old).unwrap();
        if mode == 1 {
            candidate["game"]["id"] = "unknown".into();
        }
        if mode == 2 {
            candidate["partials"][0]["prefix_sha256"] = "a".repeat(64).into();
        }
        if mode == 3 {
            candidate["unexpected_personal_data"] = true.into();
        }
        let scratch = journal.with_extension("new");
        let bytes = if mode == 0 {
            b"{torn".to_vec()
        } else {
            serde_json::to_vec(&candidate).unwrap()
        };
        fs::write(&scratch, &bytes).unwrap();
        let s = Server::new(vec![]);
        assert!(run(&f.cache(), &g, &s, &Plenty).is_err());
        assert_eq!(fs::read(&scratch).unwrap(), bytes);
        assert_eq!(fs::read(&journal).unwrap(), old);
        assert!(!f.0.join(&g.cache_path).exists());
    }
}
#[test]
fn explicit_bad_sha_reset_and_retry_preserves_saves_and_art() {
    let f = Fixture::new();
    let g = game(b"good");
    fs::create_dir_all(f.0.join("Saves")).unwrap();
    fs::write(f.0.join("Saves/user.srm"), b"save").unwrap();
    fs::create_dir_all(f.0.join("Roms/PS/Imgs")).unwrap();
    fs::write(f.0.join("Roms/PS/Imgs/Fixture.png"), b"art").unwrap();
    let bad = Server::new(vec![response(&g.files[0], 0, b"evil")]);
    assert!(run(&f.cache(), &g, &bad, &Plenty).is_err());
    drop(bad);
    f.cache()
        .reset_failed_payload(&g, "disc", &Cancellation::default())
        .unwrap();
    assert_eq!(
        fs::metadata(job(&f.0, &g).join("payloads/content.bin"))
            .unwrap()
            .len(),
        0
    );
    let good = Server::new(vec![response(&g.files[0], 0, b"good")]);
    run(&f.cache(), &g, &good, &Plenty).unwrap();
    assert!(!good.requests.lock().unwrap()[0].contains("Range:"));
    assert_eq!(fs::read(f.0.join("Saves/user.srm")).unwrap(), b"save");
    assert_eq!(
        fs::read(f.0.join("Roms/PS/Imgs/Fixture.png")).unwrap(),
        b"art"
    );
}
#[test]
fn bad_sha_reset_rejects_changed_payload_and_cancels_mid_hash() {
    let f = Fixture::new();
    let g = streamed_game(9, 4);
    checkpoint_fixture(&f.0, &g, 4, 7);
    let path = job(&f.0, &g).join("payloads/content.bin");
    let before = fs::read(job(&f.0, &g).join("job.json")).unwrap();
    let cancel = Cancellation::default();
    cancel.cancel_after_verification_reads(2);
    assert!(
        f.cache()
            .reset_failed_payload(&g, "disc", &cancel)
            .unwrap_err()
            .to_string()
            .contains("Cancelled")
    );
    assert_eq!(cancel.verification_reads(), 2);
    assert_eq!(fs::metadata(&path).unwrap().len(), 4 * WINDOW as u64);
    assert_eq!(fs::read(job(&f.0, &g).join("job.json")).unwrap(), before);
    assert_lease(&f.0, false);
    write_repeated(&path, 8, 4);
    assert!(
        f.cache()
            .reset_failed_payload(&g, "disc", &Cancellation::default())
            .is_err()
    );
    assert_eq!(fs::metadata(path).unwrap().len(), 4 * WINDOW as u64);
}
#[test]
fn interrupted_explicit_retry_intent_recovers_without_deleting_unknown_files() {
    let f = Fixture::new();
    let g = game(b"good");
    let bad = Server::new(vec![response(&g.files[0], 0, b"evil")]);
    assert!(run(&f.cache(), &g, &bad, &Plenty).is_err());
    drop(bad);
    let journal = job(&f.0, &g).join("job.json");
    let mut candidate: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    candidate["retry_file"] = retry_intent_value(&f.0, &g);
    candidate["partials"][0]["bytes"] = 0.into();
    candidate["partials"][0]["prefix_sha256"] = sha(b"").into();
    fs::write(
        journal.with_extension("new"),
        serde_json::to_vec(&candidate).unwrap(),
    )
    .unwrap();
    let s = Server::new(vec![response(&g.files[0], 0, b"good")]);
    run(&f.cache(), &g, &s, &Plenty).unwrap();
    assert_eq!(fs::read(f.0.join(&g.cache_path)).unwrap(), b"good");
    assert!(!journal.with_extension("new").exists());
}

fn retry_intent_value(root: &Path, g: &NetworkGame) -> serde_json::Value {
    use std::os::unix::fs::MetadataExt;
    let path = job(root, g).join("payloads/content.bin");
    let metadata = fs::metadata(&path).unwrap();
    let (bytes, hash) =
        hash_reader(&mut fs::File::open(path).unwrap(), &Cancellation::default()).unwrap();
    serde_json::json!({"file_id":"disc","failed_bytes":bytes,"failed_sha256":hash,"device":metadata.dev(),"inode":metadata.ino()})
}
fn commit_retry_crash_fixture(root: &Path, g: &NetworkGame) {
    let journal = job(root, g).join("job.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    value["retry_file"] = retry_intent_value(root, g);
    value["partials"][0]["bytes"] = 0.into();
    value["partials"][0]["prefix_sha256"] = sha(b"").into();
    let scratch = journal.with_extension("new");
    fs::write(&scratch, serde_json::to_vec(&value).unwrap()).unwrap();
    fs::rename(scratch, journal).unwrap();
}
#[test]
fn committed_retry_crash_preserves_changed_or_replaced_payload() {
    for mode in 0..3 {
        let f = Fixture::new();
        let g = streamed_game(9, 4);
        checkpoint_fixture(&f.0, &g, 4, 7);
        commit_retry_crash_fixture(&f.0, &g);
        let path = job(&f.0, &g).join("payloads/content.bin");
        if mode == 0 {
            write_repeated(&path, 8, 4);
        } else {
            let replacement = job(&f.0, &g).join("replacement.bin");
            write_repeated(&replacement, if mode == 1 { 8 } else { 7 }, 4);
            fs::rename(replacement, &path).unwrap();
        }
        let before = fs::read(&path).unwrap();
        let metadata = fs::read(job(&f.0, &g).join("job.json")).unwrap();
        let s = Server::new(vec![]);
        assert!(run(&f.cache(), &g, &s, &Plenty).is_err());
        assert!(
            fs::read(&path).unwrap() == before,
            "Changed/replaced payload was truncated from {} bytes",
            before.len()
        );
        assert_eq!(fs::read(job(&f.0, &g).join("job.json")).unwrap(), metadata);
        assert_lease(&f.0, false);
    }
}
#[test]
fn committed_retry_crash_resumes_exact_old_payload_or_completed_empty_reset() {
    for empty in [false, true] {
        let f = Fixture::new();
        let g = game(b"good");
        checkpoint_fixture(&f.0, &g, 0, 7);
        let path = job(&f.0, &g).join("payloads/content.bin");
        fs::write(&path, b"evil").unwrap();
        let journal = job(&f.0, &g).join("job.json");
        let value = serde_json::json!({"version":1,"game":g,"phase":"downloading","partials":[{"id":"disc","bytes":4,"prefix_sha256":sha(b"evil")}]});
        fs::write(&journal, serde_json::to_vec(&value).unwrap()).unwrap();
        commit_retry_crash_fixture(&f.0, &g);
        if empty {
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap()
                .set_len(0)
                .unwrap();
        }
        let s = Server::new(vec![response(&g.files[0], 0, b"good")]);
        run(&f.cache(), &g, &s, &Plenty).unwrap();
        assert_eq!(fs::read(f.0.join(&g.cache_path)).unwrap(), b"good");
        assert!(!s.requests.lock().unwrap()[0].contains("Range:"));
    }
}
#[test]
fn committed_retry_crash_cancels_mid_verification_without_truncating() {
    for preexisting in [false, true] {
        let f = Fixture::new();
        let g = streamed_game(9, 4);
        checkpoint_fixture(&f.0, &g, 4, 7);
        commit_retry_crash_fixture(&f.0, &g);
        if preexisting {
            fs::write(f.0.join("stay_awake"), b"preexisting").unwrap();
        }
        let cancel = Cancellation::default();
        cancel.cancel_after_verification_reads(2);
        let (tx, _) = mpsc::sync_channel(8);
        let s = Server::new(vec![]);
        let before = fs::read(job(&f.0, &g).join("job.json")).unwrap();
        assert!(
            f.cache()
                .run(&g, &s.endpoint, &cancel, &Plenty, &tx)
                .unwrap_err()
                .to_string()
                .contains("Cancelled")
        );
        assert_eq!(cancel.verification_reads(), 2);
        assert_eq!(
            fs::metadata(job(&f.0, &g).join("payloads/content.bin"))
                .unwrap()
                .len(),
            4 * WINDOW as u64
        );
        assert_eq!(fs::read(job(&f.0, &g).join("job.json")).unwrap(), before);
        assert_lease(&f.0, preexisting);
    }
}

fn replace_fixture_parent(parent: &Path, held: &Path, alternate: &Path, symlink: bool) {
    fs::rename(parent, held).unwrap();
    if symlink {
        std::os::unix::fs::symlink(alternate, parent).unwrap();
    } else {
        fs::create_dir(parent).unwrap();
    }
}
#[test]
fn dirfd_create_race_never_redirects_to_replacement_or_saves() {
    for symlink in [false, true] {
        let f = Fixture::new();
        let fsys = AnchoredFs::new(&f.0).unwrap();
        let parent = f.0.join("Roms/PS");
        fsys.mkdirs(&parent).unwrap();
        let saves = f.0.join("Saves");
        fs::create_dir(&saves).unwrap();
        fs::write(saves.join("user.srm"), b"save").unwrap();
        let held = f.0.join("Roms/PS-held");
        let p = parent.clone();
        let h = held.clone();
        let a = saves.clone();
        fsys.test_before_next_operation(move || replace_fixture_parent(&p, &h, &a, symlink));
        assert!(fsys.create(&parent.join("Fixture.bin")).is_err());
        assert!(!saves.join("Fixture.bin").exists());
        assert_eq!(fs::read(saves.join("user.srm")).unwrap(), b"save");
        if !symlink {
            assert!(!parent.join("Fixture.bin").exists());
        }
        assert!(fsys.mkdirs(&f.0.join("new-job")).is_err());
    }
}
#[test]
fn dirfd_held_part_write_and_truncate_races_touch_only_original_file() {
    for truncate in [false, true] {
        let f = Fixture::new();
        let fsys = AnchoredFs::new(&f.0).unwrap();
        let parent = f.0.join("parts");
        fsys.mkdirs(&parent).unwrap();
        let file_path = parent.join("content.bin");
        let mut file = fsys.create(&file_path).unwrap();
        file.write_all(b"old").unwrap();
        let saves = f.0.join("Saves");
        fs::create_dir(&saves).unwrap();
        fs::write(saves.join("content.bin"), b"foreign-save").unwrap();
        let held = f.0.join("parts-held");
        let p = parent.clone();
        let h = held.clone();
        let a = saves.clone();
        fsys.test_before_next_operation(move || replace_fixture_parent(&p, &h, &a, true));
        if truncate {
            assert!(file.set_len(0).is_err());
            assert_eq!(fs::metadata(held.join("content.bin")).unwrap().len(), 0);
        } else {
            file.write_all(b"-owned").unwrap();
            assert_eq!(fs::read(held.join("content.bin")).unwrap(), b"old-owned");
        }
        assert_eq!(
            fs::read(saves.join("content.bin")).unwrap(),
            b"foreign-save"
        );
        assert!(file.validate().is_err());
    }
}
#[test]
fn dirfd_install_race_never_overwrites_alternate_game_or_saves() {
    for directory in [false, true] {
        let f = Fixture::new();
        let fsys = AnchoredFs::new(&f.0).unwrap();
        let staged = f.0.join("staged");
        let target_parent = f.0.join("Roms/PS");
        fsys.mkdirs(&target_parent).unwrap();
        if directory {
            fsys.mkdirs(&staged).unwrap();
            fsys.create(&staged.join("disc.bin"))
                .unwrap()
                .write_all(b"owned")
                .unwrap();
        } else {
            fsys.create(&staged).unwrap().write_all(b"owned").unwrap();
        }
        let saves = f.0.join("Saves");
        fs::create_dir(&saves).unwrap();
        fs::write(saves.join("Fixture.bin"), b"save").unwrap();
        let held = f.0.join("Roms/PS-held");
        let p = target_parent.clone();
        let h = held.clone();
        let a = saves.clone();
        fsys.test_before_next_operation(move || replace_fixture_parent(&p, &h, &a, true));
        assert!(
            fsys.rename_no_replace(&staged, &target_parent.join("Fixture.bin"))
                .is_err()
        );
        assert_eq!(fs::read(saves.join("Fixture.bin")).unwrap(), b"save");
        let committed = held.join("Fixture.bin");
        let owned = if committed.exists() {
            committed
        } else {
            staged
        };
        assert_eq!(
            fs::read(if directory {
                owned.join("disc.bin")
            } else {
                owned
            })
            .unwrap(),
            b"owned"
        );
    }
}
#[test]
fn dirfd_source_ancestor_race_never_moves_alternate_payload() {
    let f = Fixture::new();
    let fsys = AnchoredFs::new(&f.0).unwrap();
    let parent = f.0.join("parts");
    fsys.mkdirs(&parent).unwrap();
    let source = parent.join("disc.bin");
    fsys.create(&source).unwrap().write_all(b"owned").unwrap();
    fsys.mkdirs(&f.0.join("Roms/PS")).unwrap();
    let saves = f.0.join("Saves");
    fs::create_dir(&saves).unwrap();
    fs::write(saves.join("disc.bin"), b"save").unwrap();
    let held = f.0.join("parts-held");
    let p = parent.clone();
    let h = held.clone();
    let a = saves.clone();
    fsys.test_before_next_operation(move || replace_fixture_parent(&p, &h, &a, true));
    let target = f.0.join("Roms/PS/Game.bin");
    assert!(fsys.rename_no_replace(&source, &target).is_err());
    assert_eq!(fs::read(saves.join("disc.bin")).unwrap(), b"save");
    if target.exists() {
        assert_eq!(fs::read(target).unwrap(), b"owned");
    }
}
#[test]
fn dirfd_metadata_ancestor_race_updates_only_owned_old_directory() {
    let f = Fixture::new();
    let fsys = AnchoredFs::new(&f.0).unwrap();
    let parent = f.0.join(".allium/state/network-library/installed");
    fsys.mkdirs(&parent).unwrap();
    let target = parent.join("fixture.json");
    let source = target.with_extension("new");
    let old = fsys.create(&target).unwrap();
    old.set_len(0).unwrap();
    let identity = old.identity();
    fsys.create(&source)
        .unwrap()
        .write_all(b"owned-generated-metadata")
        .unwrap();
    let saves = f.0.join("Saves");
    fs::create_dir(&saves).unwrap();
    fs::write(saves.join("fixture.json"), b"save").unwrap();
    let held = f.0.join(".allium/state/network-library/installed-held");
    let p = parent.clone();
    let h = held.clone();
    let a = saves.clone();
    fsys.test_before_next_operation(move || replace_fixture_parent(&p, &h, &a, true));
    assert!(
        fsys.replace_metadata(&source, &target, Some(identity))
            .is_err()
    );
    assert_eq!(fs::read(saves.join("fixture.json")).unwrap(), b"save");
    assert_eq!(
        fs::read(held.join("fixture.json")).unwrap(),
        b"owned-generated-metadata"
    );
}
#[test]
fn dirfd_lease_cleanup_uses_old_parent_and_preserves_replacement_identity() {
    let f = Fixture::new();
    let fsys = AnchoredFs::new(&f.0).unwrap();
    let parent = f.0.join("lease");
    fsys.mkdirs(&parent).unwrap();
    let path = parent.join("stay_awake");
    let mut lease = fsys.create(&path).unwrap();
    lease.write_all(b"owned").unwrap();
    let saves = f.0.join("Saves");
    fs::create_dir(&saves).unwrap();
    fs::write(saves.join("stay_awake"), b"preexisting").unwrap();
    let held = f.0.join("lease-held");
    replace_fixture_parent(&parent, &held, &saves, true);
    lease.remove_owned().unwrap();
    assert!(!held.join("stay_awake").exists());
    assert_eq!(fs::read(saves.join("stay_awake")).unwrap(), b"preexisting");
    let fsys = AnchoredFs::new(&f.0).unwrap();
    let path = f.0.join("owned-lease");
    let owned = fsys.create(&path).unwrap();
    let replacement = f.0.join("replacement");
    fs::write(&replacement, b"foreign").unwrap();
    fs::rename(replacement, &path).unwrap();
    assert!(owned.remove_owned().is_err());
    assert_eq!(fs::read(path).unwrap(), b"foreign");
}
#[test]
fn dirfd_root_replacement_rejects_stale_config_and_latches_session() {
    let f = Fixture::new();
    let config = CacheConfig::bind(f.0.clone(), None).unwrap();
    let fsys = AnchoredFs::new(&f.0).unwrap();
    let held = f.0.with_extension("held-root");
    let root = f.0.clone();
    let held_hook = held.clone();
    fsys.test_before_next_operation(move || {
        fs::rename(&root, &held_hook).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("protected-save"), b"save").unwrap();
    });
    assert!(fsys.create(&f.0.join("part.bin")).is_err());
    assert!(!f.0.join("part.bin").exists());
    assert_eq!(fs::read(f.0.join("protected-save")).unwrap(), b"save");
    assert!(Cache::new(config).is_err());
    fs::remove_file(f.0.join("protected-save")).unwrap();
    fs::remove_dir(&f.0).unwrap();
    fs::rename(&held, &f.0).unwrap();
    assert!(fsys.check_root().is_err());
}
#[test]
fn dirfd_repeated_enumeration_and_unknown_metadata_guard() {
    let f = Fixture::new();
    let fsys = AnchoredFs::new(&f.0).unwrap();
    let folder = f.0.join("folder");
    fsys.mkdirs(&folder).unwrap();
    fsys.create(&folder.join("known.bin"))
        .unwrap()
        .write_all(b"owned")
        .unwrap();
    let first = fsys.entries(&folder).unwrap();
    let second = fsys.entries(&folder).unwrap();
    assert_eq!(first, second);
    let g = game(b"good-content");
    let first = Server::new(vec![response(&g.files[0], 0, b"good")]);
    assert!(run(&f.cache(), &g, &first, &Plenty).is_err());
    drop(first);
    let journal = job(&f.0, &g).join("job.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    value["unexpected_personal_data"] = true.into();
    let foreign = serde_json::to_vec(&value).unwrap();
    fs::write(&journal, &foreign).unwrap();
    let s = Server::new(vec![]);
    assert!(run(&f.cache(), &g, &s, &Plenty).is_err());
    assert_eq!(fs::read(journal).unwrap(), foreign);
    assert!(f.cache().ready(&g, &Cancellation::default()).is_err());
}

#[test]
fn dirfd_unexpected_oversized_part_is_preserved() {
    let f = Fixture::new();
    let g = game(b"good-content");
    let first = Server::new(vec![response(&g.files[0], 0, b"good")]);
    assert!(run(&f.cache(), &g, &first, &Plenty).is_err());
    drop(first);
    let part = job(&f.0, &g).join("payloads/content.bin");
    fs::write(&part, b"good-unexpected-user-bytes").unwrap();
    let before = fs::read(&part).unwrap();
    let s = Server::new(vec![]);
    assert!(run(&f.cache(), &g, &s, &Plenty).is_err());
    assert_eq!(fs::read(part).unwrap(), before);
    assert!(f.cache().ready(&g, &Cancellation::default()).is_err());
}

#[test]
fn shared_root_binding_rejects_ancestor_replacement_between_workers() {
    for component in ["Roms/PS", ".allium/state/network-library"] {
        let f = Fixture::new();
        fs::create_dir_all(f.0.join("Roms/PS")).unwrap();
        fs::create_dir_all(f.0.join(".allium/state/network-library")).unwrap();
        let config = CacheConfig::bind(f.0.clone(), None).unwrap();
        let cache = Cache::new(config.clone()).unwrap();
        let g = game(b"good");
        let first = Server::new(vec![response(&g.files[0], 0, b"good")]);
        run(&cache, &g, &first, &Plenty).unwrap();
        drop(first);
        drop(cache);
        let parent = f.0.join(component);
        let held = parent.with_extension("held");
        fs::rename(&parent, &held).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("foreign-user-file"), b"preserve").unwrap();
        assert!(Cache::new(config.clone()).is_err());
        let s = Server::new(vec![]);
        let worker = Worker::start(config.clone(), g.clone(), s.endpoint.clone()).unwrap();
        assert!(
            worker
                .result
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        worker.join().unwrap();
        assert_eq!(
            fs::read(parent.join("foreign-user-file")).unwrap(),
            b"preserve"
        );
        fs::remove_file(parent.join("foreign-user-file")).unwrap();
        fs::remove_dir(&parent).unwrap();
        fs::rename(&held, &parent).unwrap();
        assert!(Cache::new(config).is_err());
    }
}
#[test]
fn verified_staging_identity_rejects_same_or_changed_byte_replacement_before_commit() {
    for directory in [false, true] {
        for same_bytes in [false, true] {
            let f = Fixture::new();
            let (g, bodies) = if directory {
                multi_game()
            } else {
                (game(b"good"), vec![b"good".to_vec()])
            };
            let cache = f.cache();
            let staged = if directory {
                job(&f.0, &g).join("payloads/disc1.bin")
            } else {
                job(&f.0, &g).join("payloads/content.bin")
            };
            let replacement = job(&f.0, &g).join("foreign-replacement.bin");
            let body = if same_bytes {
                bodies[0].clone()
            } else {
                vec![8; bodies[0].len()]
            };
            let path = staged.clone();
            let replace = replacement.clone();
            let expected = body.clone();
            cache.test_after_stage_verification(move || {
                fs::write(&replace, &body).unwrap();
                fs::rename(replace, &path).unwrap();
            });
            let server = Server::new(
                g.files
                    .iter()
                    .zip(&bodies)
                    .map(|(file, body)| response(file, 0, body))
                    .collect(),
            );
            assert!(run(&cache, &g, &server, &Plenty).is_err());
            assert!(!f.0.join(&g.cache_path).exists());
            assert_eq!(fs::read(staged).unwrap(), expected);
            assert!(cache.ready(&g, &Cancellation::default()).is_err());
        }
    }
}
#[test]
fn verified_staging_directory_replacement_is_preserved_before_commit() {
    let f = Fixture::new();
    let (g, bodies) = multi_game();
    let cache = f.cache();
    let staged = job(&f.0, &g).join("payloads");
    let held = job(&f.0, &g).join("payloads-held");
    let stage = staged.clone();
    let original = held.clone();
    let contents = bodies.clone();
    cache.test_after_stage_verification(move || {
        fs::rename(&stage, &original).unwrap();
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("disc1.bin"), &contents[0]).unwrap();
        fs::write(stage.join("disc2.bin"), &contents[1]).unwrap();
        fs::write(stage.join("Multi.m3u"), &contents[2]).unwrap();
    });
    let server = Server::new(
        g.files
            .iter()
            .zip(&bodies)
            .map(|(file, body)| response(file, 0, body))
            .collect(),
    );
    assert!(run(&cache, &g, &server, &Plenty).is_err());
    assert!(!f.0.join(&g.cache_path).exists());
    assert_eq!(fs::read(staged.join("disc1.bin")).unwrap(), bodies[0]);
    assert_eq!(fs::read(held.join("disc1.bin")).unwrap(), bodies[0]);
}

#[test]
fn explicit_adoption_unload_preserves_saves_art_history_and_redownloads() {
    let f = Fixture::new();
    let g = game(b"fake-rom");
    fs::create_dir_all(f.0.join("Roms/PS/Imgs")).unwrap();
    fs::create_dir_all(f.0.join("Saves")).unwrap();
    fs::write(f.0.join(&g.game_path), b"fake-rom").unwrap();
    for path in [
        "Saves/fixture.srm",
        "Roms/PS/Imgs/Fixture.png",
        "history.db",
    ] {
        fs::write(f.0.join(path), b"preserve").unwrap();
    }
    let cache = f.cache();
    assert!(
        cache
            .unload_registered(&g, &Cancellation::default())
            .is_err()
    );
    cache.adopt_existing(&g, &Cancellation::default()).unwrap();
    cache
        .unload_registered(&g, &Cancellation::default())
        .unwrap();
    assert!(!f.0.join(&g.game_path).exists());
    assert!(!cache.has_index(&g).unwrap());
    for path in [
        "Saves/fixture.srm",
        "Roms/PS/Imgs/Fixture.png",
        "history.db",
    ] {
        assert_eq!(fs::read(f.0.join(path)).unwrap(), b"preserve");
    }
    let s = Server::new(vec![response(&g.files[0], 0, b"fake-rom")]);
    assert!(run(&cache, &g, &s, &Plenty).is_ok());
    cache
        .unload_registered(&g, &Cancellation::default())
        .unwrap();
}
#[test]
fn adoption_refuses_same_game_partial_then_download_resume_remains_exact() {
    let f = Fixture::new();
    let bytes = vec![7u8; 128 * 1024];
    let g = game(&bytes);
    let cache = f.cache();
    let first = Server::new(vec![response(&g.files[0], 0, &bytes[..4096])]);
    assert!(run(&cache, &g, &first, &Plenty).is_err());
    drop(first);
    let partial = job(&f.0, &g).join("payloads/content.bin");
    let partial_before = fs::read(&partial).unwrap();
    fs::create_dir_all(f.0.join("Roms/PS")).unwrap();
    fs::write(f.0.join(&g.cache_path), &bytes).unwrap();
    assert!(
        cache
            .adopt_existing(&g, &Cancellation::default())
            .unwrap_err()
            .to_string()
            .contains("Managed partial")
    );
    assert_eq!(fs::read(&partial).unwrap(), partial_before);
    assert_eq!(fs::read(f.0.join(&g.cache_path)).unwrap(), bytes);
    assert!(!cache.has_index(&g).unwrap());

    fs::remove_file(f.0.join(&g.cache_path)).unwrap();
    let next = Server::new(vec![response(&g.files[0], 4096, &bytes[4096..])]);
    assert!(run(&cache, &g, &next, &Plenty).is_ok());
    assert!(next.requests.lock().unwrap()[0].contains("Range: bytes=4096-\r\n"));
}

#[test]
fn adoption_refuses_unknown_other_revision_job_hash_mismatch_and_unexpected_file() {
    let f = Fixture::new();
    let g = game(b"good");
    fs::create_dir_all(f.0.join("Roms/PS")).unwrap();
    fs::write(f.0.join(&g.cache_path), b"bad!").unwrap();
    let cache = f.cache();
    assert!(cache.adopt_existing(&g, &Cancellation::default()).is_err());
    assert_eq!(fs::read(f.0.join(&g.cache_path)).unwrap(), b"bad!");
    assert!(!cache.has_index(&g).unwrap());

    fs::write(f.0.join(&g.cache_path), b"good").unwrap();
    let unknown =
        f.0.join(".allium/state/network-library/jobs/fixture/old-revision/unknown");
    fs::create_dir_all(unknown.parent().unwrap()).unwrap();
    fs::write(&unknown, b"preserve").unwrap();
    assert!(cache.adopt_existing(&g, &Cancellation::default()).is_err());
    assert_eq!(fs::read(&unknown).unwrap(), b"preserve");
    assert!(!cache.has_index(&g).unwrap());

    let f = Fixture::new();
    let (g, bodies) = multi_game();
    for (path, body) in g.targets().unwrap().iter().zip(&bodies) {
        fs::create_dir_all(f.0.join(path).parent().unwrap()).unwrap();
        fs::write(f.0.join(path), body).unwrap();
    }
    let unexpected = f.0.join(&g.cache_path).join("notes.bin");
    fs::write(&unexpected, b"preserve").unwrap();
    let cache = f.cache();
    assert!(cache.adopt_existing(&g, &Cancellation::default()).is_err());
    assert_eq!(fs::read(unexpected).unwrap(), b"preserve");
    assert!(!cache.has_index(&g).unwrap());
}
#[test]
fn adoption_allows_only_empty_current_revision_job_placeholder() {
    fn target(f: &Fixture, g: &NetworkGame) {
        fs::create_dir_all(f.0.join("Roms/PS")).unwrap();
        fs::write(f.0.join(&g.cache_path), b"good").unwrap();
    }

    let f = Fixture::new();
    let g = game(b"good");
    target(&f, &g);
    let current = job(&f.0, &g);
    fs::create_dir_all(&current).unwrap();
    let cache = f.cache();
    cache.adopt_existing(&g, &Cancellation::default()).unwrap();
    assert!(cache.has_index(&g).unwrap());
    assert!(current.is_dir());

    for name in ["arbitrary".to_owned(), "a".repeat(64)] {
        let f = Fixture::new();
        let g = game(b"good");
        target(&f, &g);
        let unknown =
            f.0.join(".allium/state/network-library/jobs/fixture")
                .join(&name);
        fs::create_dir_all(&unknown).unwrap();
        let cache = f.cache();
        assert!(cache.adopt_existing(&g, &Cancellation::default()).is_err());
        assert!(unknown.is_dir());
        assert_eq!(fs::read(f.0.join(&g.cache_path)).unwrap(), b"good");
        assert!(!cache.has_index(&g).unwrap());
    }

    let f = Fixture::new();
    let g = game(b"good");
    target(&f, &g);
    let jobs = f.0.join(".allium/state/network-library/jobs/fixture");
    fs::create_dir_all(&jobs).unwrap();
    let unknown_file = jobs.join("unknown-file");
    fs::write(&unknown_file, b"preserve").unwrap();
    let cache = f.cache();
    assert!(cache.adopt_existing(&g, &Cancellation::default()).is_err());
    assert_eq!(fs::read(&unknown_file).unwrap(), b"preserve");
    assert!(!cache.has_index(&g).unwrap());

    let f = Fixture::new();
    let g = game(b"good");
    target(&f, &g);
    let jobs = f.0.join(".allium/state/network-library/jobs/fixture");
    let outside = f.0.join("outside-preserve");
    fs::create_dir_all(&jobs).unwrap();
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, jobs.join("job-link")).unwrap();
    let cache = f.cache();
    assert!(cache.adopt_existing(&g, &Cancellation::default()).is_err());
    assert!(outside.is_dir());
    assert!(
        fs::symlink_metadata(jobs.join("job-link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(!cache.has_index(&g).unwrap());
}
#[test]
fn unload_resume_unknown_and_cancel_preserve_registered_payloads() {
    let f = Fixture::new();
    let (g, bodies) = multi_game();
    let s = Server::new(
        g.files
            .iter()
            .zip(&bodies)
            .map(|(p, b)| response(p, 0, b))
            .collect(),
    );
    let cache = f.cache();
    run(&cache, &g, &s, &Plenty).unwrap();
    let resume = f.0.join(".allium/state/current_game");
    fs::write(
        &resume,
        serde_json::to_vec(&serde_json::json!({"path":f.0.join(&g.game_path)})).unwrap(),
    )
    .unwrap();
    assert!(
        cache
            .unload_registered(&g, &Cancellation::default())
            .is_err()
    );
    fs::write(
        &resume,
        serde_json::to_vec(&serde_json::json!({"path":f.0.join(&g.cache_path).join("disc2.bin")}))
            .unwrap(),
    )
    .unwrap();
    assert!(
        cache
            .unload_registered(&g, &Cancellation::default())
            .is_err()
    );
    fs::remove_file(resume).unwrap();
    let unexpected = f.0.join(&g.cache_path).join("unknown.bin");
    fs::write(&unexpected, b"preserve").unwrap();
    assert!(
        cache
            .unload_registered(&g, &Cancellation::default())
            .is_err()
    );
    assert_eq!(fs::read(&unexpected).unwrap(), b"preserve");
    fs::remove_file(unexpected).unwrap();
    let cancel = Cancellation::default();
    cancel.cancel();
    assert!(cache.unload_registered(&g, &cancel).is_err());
    assert!(cache.has_index(&g).unwrap());
    cache
        .unload_registered(&g, &Cancellation::default())
        .unwrap();
    assert!(!cache.target_exists(&g).unwrap());
}
#[test]
fn interrupted_unload_rechecks_original_inode_and_preserves_replacement() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    let (g, bodies) = multi_game();
    let s = Server::new(
        g.files
            .iter()
            .zip(&bodies)
            .map(|(p, b)| response(p, 0, b))
            .collect(),
    );
    let cache = f.cache();
    run(&cache, &g, &s, &Plenty).unwrap();
    let targets = g.targets().unwrap();
    let leaves = targets
        .iter()
        .map(|p| {
            let m = fs::metadata(f.0.join(p)).unwrap();
            serde_json::json!({"path":p,"device":m.dev(),"inode":m.ino()})
        })
        .collect::<Vec<_>>();
    let m = fs::metadata(f.0.join(&g.cache_path)).unwrap();
    let intent = serde_json::json!({"version":1,"game":g,"unload_files":leaves,"unload_dirs":[{"path":g.cache_path,"device":m.dev(),"inode":m.ino()}]});
    let installed = f.0.join(".allium/state/network-library/installed");
    fs::write(
        installed.join("fixture.unloading.json"),
        serde_json::to_vec(&intent).unwrap(),
    )
    .unwrap();
    fs::remove_file(installed.join("fixture.json")).unwrap();
    fs::remove_file(f.0.join(&targets[0])).unwrap();
    let original = f.0.join(&targets[1]);
    let held = original.with_extension("held");
    fs::rename(&original, &held).unwrap();
    fs::write(&original, &bodies[1]).unwrap();
    assert!(
        cache
            .unload_registered(&g, &Cancellation::default())
            .is_err()
    );
    assert_eq!(fs::read(&original).unwrap(), bodies[1]);
    fs::remove_file(&original).unwrap();
    fs::rename(&held, &original).unwrap();
    cache
        .unload_registered(&g, &Cancellation::default())
        .unwrap();
    assert!(!cache.target_exists(&g).unwrap());
}

#[test]
fn unload_mid_verification_cancel_preserves_payload_index_and_existing_lease() {
    let f = Fixture::new();
    let data = vec![b'f'; 3 * WINDOW];
    let g = game(&data);
    let s = Server::new(vec![response(&g.files[0], 0, &data)]);
    let cache = f.cache();
    run(&cache, &g, &s, &Plenty).unwrap();
    fs::write(f.0.join("stay_awake"), b"existing-owner").unwrap();
    let cancel = Cancellation::default();
    cancel.cancel_after_verification_reads(1);
    assert!(
        cache
            .unload_registered(&g, &cancel)
            .unwrap_err()
            .to_string()
            .contains("Cancelled")
    );
    assert!(cache.has_index(&g).unwrap());
    assert!(!cache.unloading(&g).unwrap());
    assert_eq!(fs::read(f.0.join(&g.game_path)).unwrap(), data);
    assert_eq!(fs::read(f.0.join("stay_awake")).unwrap(), b"existing-owner");
}
#[test]
fn unload_committed_or_scratch_intent_blocks_ready_then_recovers_exact_remaining_payload() {
    use std::os::unix::fs::MetadataExt;
    for scratch in [false, true] {
        let f = Fixture::new();
        let g = game(b"fake-rom");
        let s = Server::new(vec![response(&g.files[0], 0, b"fake-rom")]);
        let cache = f.cache();
        run(&cache, &g, &s, &Plenty).unwrap();
        let m = fs::metadata(f.0.join(&g.game_path)).unwrap();
        let intent = serde_json::json!({"version":1,"game":g,"unload_files":[{"path":g.game_path,"device":m.dev(),"inode":m.ino()}],"unload_dirs":[]});
        let installed = f.0.join(".allium/state/network-library/installed");
        let path = installed.join(if scratch {
            "fixture.unloading.new"
        } else {
            "fixture.unloading.json"
        });
        fs::write(&path, serde_json::to_vec(&intent).unwrap()).unwrap();
        assert!(!cache.has_index(&g).unwrap());
        assert!(cache.ready(&g, &Cancellation::default()).is_err());
        let endpoint = Server::new(vec![]);
        assert!(
            run(&cache, &g, &endpoint, &Plenty)
                .unwrap_err()
                .to_string()
                .contains("Interrupted unload")
        );
        cache
            .unload_registered(&g, &Cancellation::default())
            .unwrap();
        assert!(!cache.target_exists(&g).unwrap());
        assert!(!cache.unloading(&g).unwrap());
    }
}
