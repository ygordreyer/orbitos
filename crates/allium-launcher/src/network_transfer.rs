//! Bounded HTTP/1.1 streaming transport for registered payloads.
#![allow(dead_code)]
use crate::network_library::Payload;
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub fn digest_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
pub const WINDOW: usize = 64 * 1024;
const READ_POLL: Duration = Duration::from_millis(100);
#[derive(Clone, Copy)]
pub enum VerificationReport {
    Started,
    Progress,
}
#[derive(Clone)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
    progress: Option<Arc<dyn Fn(u64, u64, VerificationReport) + Send + Sync>>,
    #[cfg(test)]
    read_gate: Arc<std::sync::Mutex<(usize, usize)>>,
    #[cfg(test)]
    verify_events: Arc<std::sync::Mutex<Vec<(&'static str, &'static str)>>>,
    #[cfg(test)]
    sync_hook: Option<Arc<dyn Fn() + Send + Sync>>,
}
impl Default for Cancellation {
    fn default() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            progress: None,
            #[cfg(test)]
            read_gate: Arc::new(std::sync::Mutex::new((usize::MAX, 0))),
            #[cfg(test)]
            verify_events: Default::default(),
            #[cfg(test)]
            sync_hook: None,
        }
    }
}
impl Cancellation {
    pub fn verification_event(&self, event: &'static str, cause: &'static str) {
        common::perf_trace::network_event(event, cause, None);
        #[cfg(test)]
        self.verify_events.lock().unwrap().push((event, cause));
    }
    pub fn sync_with_heartbeat<T>(
        &self,
        total: u64,
        sync: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        std::thread::scope(|scope| {
            let (stop, wait) = std::sync::mpsc::channel::<()>();
            scope.spawn(move || {
                while matches!(
                    wait.recv_timeout(Duration::from_millis(100)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    if let Some(callback) = &self.progress {
                        callback(0, total, VerificationReport::Progress);
                    }
                    self.verification_event("verify_progress", "sync_wait");
                }
            });
            self.verification_event("verify_sync_begin", "streamed");
            #[cfg(test)]
            if let Some(hook) = &self.sync_hook {
                hook();
            }
            let result = sync();
            self.verification_event("verify_sync_end", "streamed");
            drop(stop);
            result
        })
    }

    pub fn reporting(
        &self,
        callback: impl Fn(u64, u64, VerificationReport) + Send + Sync + 'static,
    ) -> Self {
        let mut cancel = self.clone();
        cancel.progress = Some(Arc::new(callback));
        cancel
    }
    pub fn verification(&self, total: u64) -> VerificationProgress<'_> {
        VerificationProgress::new(self, total)
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub fn check(&self) -> Result<()> {
        ensure!(!self.cancelled.load(Ordering::Acquire), "Cancelled");
        Ok(())
    }
    pub fn check_verification_read(&self) -> Result<()> {
        #[cfg(test)]
        {
            let mut gate = self.read_gate.lock().unwrap();
            if gate.1 >= gate.0 {
                self.cancel();
            } else {
                gate.1 += 1;
            }
        }
        self.check()
    }
    #[cfg(test)]
    pub fn test_sync_hook(&mut self, hook: impl Fn() + Send + Sync + 'static) {
        self.sync_hook = Some(Arc::new(hook));
    }
    #[cfg(test)]
    pub fn test_verify_events(&self) -> Vec<&'static str> {
        self.verify_events
            .lock()
            .unwrap()
            .iter()
            .map(|(event, _)| *event)
            .collect()
    }
    #[cfg(test)]
    pub fn test_verify_results(&self) -> Vec<(&'static str, &'static str)> {
        self.verify_events.lock().unwrap().clone()
    }
    #[cfg(test)]
    pub fn cancel_after_verification_reads(&self, reads: usize) {
        *self.read_gate.lock().unwrap() = (reads, 0);
    }
    #[cfg(test)]
    pub fn verification_reads(&self) -> usize {
        self.read_gate.lock().unwrap().1
    }
}
pub struct VerificationProgress<'a> {
    cancel: &'a Cancellation,
    done: u64,
    total: u64,
    last: Instant,
    succeeded: bool,
}
impl<'a> VerificationProgress<'a> {
    fn new(cancel: &'a Cancellation, total: u64) -> Self {
        cancel.verification_event("verify_started", "hash");
        let progress = Self {
            cancel,
            done: 0,
            total,
            last: Instant::now(),
            succeeded: false,
        };
        progress.report(VerificationReport::Started);
        progress
    }
    fn report(&self, report: VerificationReport) {
        if let Some(callback) = &self.cancel.progress {
            callback(self.done, self.total, report);
        }
    }
    pub fn complete(&mut self) {
        self.succeeded = true;
    }
    pub fn advance(&mut self, bytes: u64) {
        self.done += bytes;
        if self.done == self.total || self.last.elapsed() >= Duration::from_millis(100) {
            self.report(VerificationReport::Progress);
            self.cancel.verification_event("verify_progress", "hash");
            self.last = Instant::now();
        }
    }
}
impl Drop for VerificationProgress<'_> {
    fn drop(&mut self) {
        self.report(VerificationReport::Progress);
        self.cancel.verification_event(
            "verify_finished",
            if self.succeeded {
                "complete"
            } else {
                "interrupted"
            },
        );
    }
}

#[derive(Clone)]
pub struct Endpoint {
    pub address: SocketAddr,
    pub connect_timeout: Duration,
    pub header_timeout: Duration,
    pub first_body_timeout: Duration,
    pub idle_timeout: Duration,
}
impl Endpoint {
    pub fn lan(address: SocketAddr) -> Result<Self> {
        let ip = address.ip();
        ensure!(
            address.port() != 0
                && (ip.is_loopback() || matches!(ip, std::net::IpAddr::V4(v) if v.is_private())),
            "Endpoint must be explicit private LAN/loopback IP"
        );
        Ok(Self {
            address,
            connect_timeout: Duration::from_secs(2),
            header_timeout: Duration::from_secs(10),
            first_body_timeout: Duration::from_secs(120),
            idle_timeout: Duration::from_secs(10),
        })
    }
}
struct TimedStream {
    stream: TcpStream,
}
impl TimedStream {
    fn read_until(
        &mut self,
        buffer: &mut [u8],
        cancel: &Cancellation,
        deadline: Instant,
        timeout_message: &str,
    ) -> Result<usize> {
        loop {
            cancel.check()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(!remaining.is_zero(), "{timeout_message}");
            self.stream
                .set_read_timeout(Some(remaining.min(READ_POLL)))?;
            match self.stream.read(buffer) {
                Ok(size) => return Ok(size),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    fn headers(&mut self, cancel: &Cancellation, timeout: Duration) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(1024);
        let deadline = deadline_after(timeout)?;
        while !data.ends_with(b"\r\n\r\n") {
            ensure!(data.len() < 16384, "HTTP headers too large/slow");
            let mut byte = [0];
            ensure!(
                self.read_until(&mut byte, cancel, deadline, "HTTP headers too large/slow")? == 1,
                "Truncated HTTP headers"
            );
            data.push(byte[0]);
        }
        Ok(data)
    }
}
fn deadline_after(timeout: Duration) -> Result<Instant> {
    Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| anyhow::anyhow!("HTTP timeout overflow"))
}
fn parse_headers(bytes: &[u8]) -> Result<(u16, HashMap<String, String>)> {
    let text = std::str::from_utf8(bytes)?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .unwrap_or_default()
        .split(' ')
        .collect::<Vec<_>>();
    ensure!(
        status.len() >= 2 && status[0] == "HTTP/1.1",
        "Unsupported HTTP response"
    );
    let status = status[1].parse::<u16>()?;
    let mut headers = HashMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("Malformed HTTP header"))?;
        ensure!(
            !key.is_empty() && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'),
            "Invalid header name"
        );
        ensure!(
            headers
                .insert(key.to_ascii_lowercase(), value.trim().to_owned())
                .is_none(),
            "Duplicate HTTP header"
        );
    }
    ensure!(
        !headers.contains_key("transfer-encoding") && !headers.contains_key("content-encoding"),
        "Encoded/chunked payload rejected"
    );
    Ok((status, headers))
}
fn validate_response(
    status: u16,
    headers: &HashMap<String, String>,
    file: &Payload,
    offset: u64,
) -> Result<()> {
    let expected_status = if offset == 0 { 200 } else { 206 };
    ensure!(
        status == expected_status,
        "Unexpected HTTP status; resume identity may have changed"
    );
    ensure!(
        headers.get("etag") == Some(&format!("\"{}\"", file.sha256)),
        "SHA ETag mismatch"
    );
    ensure!(
        headers
            .get("content-length")
            .and_then(|v| v.parse::<u64>().ok())
            == Some(file.bytes - offset),
        "Content-Length mismatch"
    );
    if offset > 0 {
        ensure!(
            headers.get("content-range")
                == Some(&format!(
                    "bytes {}-{}/{}",
                    offset,
                    file.bytes - 1,
                    file.bytes
                )),
            "Content-Range mismatch"
        );
    } else {
        ensure!(
            !headers.contains_key("content-range"),
            "Unexpected Content-Range"
        );
    }
    Ok(())
}
pub fn hash_reader(reader: &mut impl Read, cancel: &Cancellation) -> Result<(u64, String)> {
    let (bytes, hash, mut verification) = hash_reader_progress(reader, cancel, 0)?;
    verification.complete();
    Ok((bytes, hash))
}
pub fn hash_reader_progress<'a>(
    reader: &mut impl Read,
    cancel: &'a Cancellation,
    total: u64,
) -> Result<(u64, String, VerificationProgress<'a>)> {
    let mut progress = cancel.verification(total);
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0; WINDOW];
    loop {
        cancel.check_verification_read()?;
        let size = reader.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
        progress.advance(size as u64);
        count = count
            .checked_add(size as u64)
            .ok_or_else(|| anyhow::anyhow!("Length overflow"))?;
    }
    Ok((count, digest_hex(hash.finalize().as_slice()), progress))
}
#[cfg(test)]
thread_local! {
    static TEST_DOWNLOADS: std::cell::RefCell<std::collections::VecDeque<u8>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
    static TEST_LAST_BYTE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
pub fn test_downloads(bytes: Vec<u8>) {
    TEST_DOWNLOADS.with(|b| *b.borrow_mut() = bytes.into());
    TEST_LAST_BYTE.with(|t| t.set(None));
}
#[cfg(test)]
pub fn test_last_byte() -> Option<Instant> {
    TEST_LAST_BYTE.with(|t| t.get())
}

/// Callback persists checkpoints and checks free space before every write.
pub fn download(
    endpoint: &Endpoint,
    file: &Payload,
    offset: u64,
    writer: &mut impl Write,
    cancel: &Cancellation,
    mut before_write: impl FnMut(u64) -> Result<()>,
) -> Result<()> {
    ensure!(
        offset < file.bytes
            && file.http_route.starts_with("/v1/files/")
            && !file.http_route.contains(['\r', '\n', ' ', '?', '#']),
        "Invalid payload request"
    );
    cancel.check()?;
    #[cfg(test)]
    if let Some(byte) = TEST_DOWNLOADS.with(|b| b.borrow_mut().pop_front()) {
        let buffer = [byte; WINDOW];
        let mut remaining = file.bytes - offset;
        while remaining > 0 {
            cancel.check()?;
            let size = remaining.min(WINDOW as u64) as usize;
            before_write(size as u64)?;
            writer.write_all(&buffer[..size])?;
            remaining -= size as u64;
        }
        TEST_LAST_BYTE.with(|t| t.set(Some(Instant::now())));
        return Ok(());
    }
    let stream = TcpStream::connect_timeout(&endpoint.address, endpoint.connect_timeout)?;
    stream.set_write_timeout(Some(endpoint.connect_timeout))?;
    let mut connection = TimedStream { stream };
    let range = if offset > 0 {
        format!(
            "Range: bytes={offset}-\r\nIf-Range: \"{}\"\r\n",
            file.sha256
        )
    } else {
        String::new()
    };
    cancel.check()?;
    write!(
        connection.stream,
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept-Encoding: identity\r\n{}\r\n",
        file.http_route, endpoint.address, range
    )?;
    let headers = connection.headers(cancel, endpoint.header_timeout)?;
    let (status, headers) = parse_headers(&headers)?;
    validate_response(status, &headers, file, offset)?;
    let mut remaining = file.bytes - offset;
    let mut buffer = [0; WINDOW];
    let mut body_deadline = deadline_after(endpoint.first_body_timeout)?;
    let mut timeout_message = "HTTP first body timeout";
    while remaining > 0 {
        let limit = remaining.min(WINDOW as u64) as usize;
        let size =
            connection.read_until(&mut buffer[..limit], cancel, body_deadline, timeout_message)?;
        ensure!(size > 0, "Truncated HTTP body");
        body_deadline = deadline_after(endpoint.idle_timeout)?;
        timeout_message = "HTTP idle timeout";
        before_write(size as u64)?;
        cancel.check()?;
        writer.write_all(&buffer[..size])?;
        remaining -= size as u64;
    }
    let mut extra = [0];
    ensure!(
        connection.read_until(&mut extra, cancel, body_deadline, "HTTP idle timeout")? == 0,
        "HTTP body exceeds declared length"
    );
    Ok(())
}
