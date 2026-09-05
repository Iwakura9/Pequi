#![cfg(unix)]

use peq::chain::silent_preset;
use peq::engine::{EngineClient, MockEngine, DEFAULT_TIMEOUT};
use peq::ipc::{Command, IpcServer, Request, Response, SocketClient, MAX_MESSAGE_BYTES};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn socket_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "peq-ipc-{label}-{}-{nonce}/daemon.sock",
        std::process::id()
    ))
}

fn running(
    label: &str,
) -> (
    PathBuf,
    Arc<Mutex<MockEngine>>,
    Arc<AtomicBool>,
    JoinHandle<()>,
) {
    let path = socket_path(label);
    let engine = Arc::new(Mutex::new(MockEngine::default()));
    let server = IpcServer::bind(&path, Arc::clone(&engine)).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || server.serve(stop).unwrap())
    };
    for _ in 0..100 {
        if path.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    (path, engine, stop, thread)
}

fn finish(path: PathBuf, stop: Arc<AtomicBool>, thread: JoinHandle<()>) {
    stop.store(true, Ordering::Release);
    thread.join().unwrap();
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn roundtrip_apply_status_and_bypass() {
    let (path, _, stop, thread) = running("roundtrip");
    let mut client = SocketClient::connect(&path).unwrap();
    let applied = client
        .apply(silent_preset("test"), 0, DEFAULT_TIMEOUT)
        .unwrap();
    assert_eq!(applied.revision, 1);
    assert_eq!(client.status(DEFAULT_TIMEOUT).unwrap().revision, 1);
    assert!(client.bypass(true, 1, DEFAULT_TIMEOUT).unwrap().bypassed);
    assert_eq!(client.events().len(), 2);
    finish(path, stop, thread);
}

#[test]
fn malformed_version_and_oversized_requests_are_rejected() {
    let (path, _, stop, thread) = running("invalid");
    let oversized = vec![b'x'; MAX_MESSAGE_BYTES + 1];
    let requests = vec![
        b"not json\n".to_vec(),
        serde_json::to_vec(&Request {
            id: 7,
            version: 99,
            command: Command::Status,
        })
        .unwrap(),
        oversized,
    ];
    for payload in requests {
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.write_all(&payload).unwrap();
        if payload.len() <= MAX_MESSAGE_BYTES {
            stream.write_all(b"\n").unwrap();
        }
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        assert!(serde_json::from_str::<Response>(reply.trim())
            .unwrap()
            .error
            .is_some());
    }
    finish(path, stop, thread);
}

#[test]
fn slow_partial_client_does_not_block_healthy_status() {
    let (path, _, stop, thread) = running("slow");
    let mut slow = UnixStream::connect(&path).unwrap();
    slow.write_all(b"{\"id\":1,").unwrap();
    let started = std::time::Instant::now();
    let mut client = SocketClient::connect(&path).unwrap();
    assert!(client.status(DEFAULT_TIMEOUT).unwrap().connected);
    assert!(started.elapsed() < Duration::from_millis(450));
    drop(slow);
    finish(path, stop, thread);
}

#[test]
fn socket_is_private() {
    let (path, _, stop, thread) = running("mode");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    finish(path, stop, thread);
}
