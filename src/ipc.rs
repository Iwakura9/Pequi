//! Versioned JSON Lines IPC over a private Unix socket.
//!
//! A connection carries one request and one response.  This deliberately keeps
//! the protocol easy to bound: a client that sends only a partial line cannot
//! occupy a connection needed by another client.  Reads and writes have a
//! 500ms socket timeout; a future hardening pass may add an absolute deadline
//! for trickle clients as well.

use crate::engine::{
    AppliedSnapshot, EngineClient, EngineError, EngineEvent, EngineStatus, MockEngine,
    DEFAULT_TIMEOUT,
};
use crate::preset::Preset;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

/// Wire protocol version.
pub const PROTOCOL_VERSION: u32 = 1;
/// Maximum request or response JSON payload, excluding its trailing newline.
pub const MAX_MESSAGE_BYTES: usize = 512 * 1024;
/// Maximum number of events returned by one poll.
pub const MAX_EVENTS: usize = 128;
const SOCKET_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_CLIENTS: usize = 32;

/// A versioned request. IDs are echoed by the response so clients can match
/// replies even when they issue requests concurrently from different clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub version: u32,
    pub command: Command,
}

impl Request {
    pub fn new(id: u64, command: Command) -> Self {
        Self {
            id,
            version: PROTOCOL_VERSION,
            command,
        }
    }
}

/// Commands understood by the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    Apply {
        preset: Preset,
        expected_revision: u64,
    },
    Bypass {
        bypassed: bool,
        expected_revision: u64,
    },
    Status,
    Output {
        output: String,
    },
    Events,
    Subscribe,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResponseData {
    Applied(AppliedSnapshot),
    Status(EngineStatus),
    Events(Vec<EngineEvent>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    pub version: u32,
    pub data: Option<ResponseData>,
    pub error: Option<EngineError>,
}

impl Response {
    fn success(id: u64, data: ResponseData) -> Self {
        Self {
            id,
            version: PROTOCOL_VERSION,
            data: Some(data),
            error: None,
        }
    }

    fn error(id: u64, error: EngineError) -> Self {
        Self {
            id,
            version: PROTOCOL_VERSION,
            data: None,
            error: Some(error),
        }
    }

    pub fn into_result(self) -> Result<ResponseData, EngineError> {
        match (self.data, self.error) {
            (Some(data), None) => Ok(data),
            (None, Some(error)) => Err(error),
            _ => Err(EngineError::Failure("invalid IPC response".into())),
        }
    }
}

#[cfg(unix)]
pub struct IpcServer<E: EngineClient + 'static = MockEngine> {
    listener: UnixListener,
    path: PathBuf,
    engine: Arc<Mutex<E>>,
}

#[cfg(unix)]
impl<E: EngineClient + 'static> IpcServer<E> {
    /// Bind an explicitly supplied absolute socket path.
    pub fn bind(path: impl AsRef<Path>, engine: Arc<Mutex<E>>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPC socket path must be absolute",
            ));
        }
        ensure_private_parent(path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "IPC socket has no parent")
        })?)?;
        prepare_socket_path(&path)?;
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path,
            engine,
        })
    }

    /// Serve until `stop` is set. At most 32 client handlers are live at once.
    pub fn serve(self, stop: Arc<AtomicBool>) -> io::Result<()> {
        let active = Arc::new(AtomicUsize::new(0));
        let mut handlers: Vec<JoinHandle<()>> = Vec::new();
        while !stop.load(Ordering::Acquire) {
            handlers.retain(|handler| !handler.is_finished());
            match self.listener.accept() {
                Ok((stream, _)) => {
                    let previous = active.fetch_add(1, Ordering::AcqRel);
                    if previous >= MAX_CLIENTS {
                        active.fetch_sub(1, Ordering::AcqRel);
                        drop(stream);
                        continue;
                    }
                    let engine = Arc::clone(&self.engine);
                    let active_count = Arc::clone(&active);
                    handlers.push(thread::spawn(move || {
                        handle_client(stream, engine);
                        active_count.fetch_sub(1, Ordering::AcqRel);
                    }));
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error),
            }
        }
        for handler in handlers {
            let _ = handler.join();
        }
        Ok(())
    }
}

#[cfg(unix)]
impl<E: EngineClient + 'static> Drop for IpcServer<E> {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn handle_client(mut stream: UnixStream, engine: Arc<Mutex<impl EngineClient + 'static>>) {
    let _ = stream.set_read_timeout(Some(SOCKET_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_TIMEOUT));
    let request = read_request(&stream);
    let response = match request {
        Ok(request) if request.version != PROTOCOL_VERSION => Response::error(
            request.id,
            EngineError::Failure(format!(
                "unsupported IPC protocol version {}",
                request.version
            )),
        ),
        Ok(request) => dispatch(request, &engine),
        Err(error) => Response::error(0, error),
    };
    let Ok(mut encoded) = serde_json::to_vec(&response) else {
        return;
    };
    if encoded.len() > MAX_MESSAGE_BYTES {
        let Ok(fallback) = serde_json::to_vec(&Response::error(
            response.id,
            EngineError::Failure("IPC response exceeds maximum size".into()),
        )) else {
            return;
        };
        encoded = fallback;
    }
    encoded.push(b'\n');
    let _ = stream.write_all(&encoded);
}

#[cfg(unix)]
fn read_request(stream: &UnixStream) -> Result<Request, EngineError> {
    let reader = BufReader::new(stream);
    let mut limited = reader.take((MAX_MESSAGE_BYTES + 1) as u64);
    let mut line = String::new();
    let read = limited.read_line(&mut line).map_err(map_io_error)?;
    if read == 0 {
        return Err(EngineError::Failure("empty IPC request".into()));
    }
    if line.len() > MAX_MESSAGE_BYTES || !line.ends_with('\n') {
        return Err(EngineError::Failure(
            "IPC request exceeds maximum size or is incomplete".into(),
        ));
    }
    line.pop();
    if line.ends_with('\r') {
        line.pop();
    }
    serde_json::from_str(&line)
        .map_err(|error| EngineError::Failure(format!("malformed IPC request: {error}")))
}

#[cfg(unix)]
fn dispatch<E: EngineClient>(request: Request, engine: &Arc<Mutex<E>>) -> Response {
    let id = request.id;
    let mut engine = match engine.try_lock() {
        Ok(engine) => engine,
        Err(std::sync::TryLockError::WouldBlock) => {
            return Response::error(id, EngineError::Timeout)
        }
        Err(std::sync::TryLockError::Poisoned(_)) => {
            return Response::error(id, EngineError::Failure("engine lock poisoned".into()))
        }
    };
    let result = match request.command {
        Command::Apply {
            preset,
            expected_revision,
        } => engine
            .apply(preset, expected_revision, DEFAULT_TIMEOUT)
            .map(ResponseData::Applied),
        Command::Bypass {
            bypassed,
            expected_revision,
        } => engine
            .bypass(bypassed, expected_revision, DEFAULT_TIMEOUT)
            .map(ResponseData::Applied),
        Command::Status => engine.status(DEFAULT_TIMEOUT).map(ResponseData::Status),
        Command::Output { output } => engine
            .select_output(output, DEFAULT_TIMEOUT)
            .map(ResponseData::Status),
        Command::Events | Command::Subscribe => {
            let events = engine.events().into_iter().take(MAX_EVENTS).collect();
            Ok(ResponseData::Events(events))
        }
    };
    match result {
        Ok(data) => Response::success(id, data),
        Err(error) => Response::error(id, error),
    }
}

#[cfg(unix)]
fn map_io_error(error: io::Error) -> EngineError {
    if matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ) {
        EngineError::Timeout
    } else {
        EngineError::Failure(format!("IPC I/O error: {error}"))
    }
}

#[cfg(unix)]
fn ensure_private_parent(parent: &Path) -> io::Result<()> {
    if !parent.exists() {
        fs::create_dir_all(parent)?;
    }
    let mut current = PathBuf::from("/");
    for component in parent.components() {
        if let std::path::Component::Normal(part) = component {
            current.push(part);
            let metadata = fs::symlink_metadata(&current)?;
            if metadata.file_type().is_symlink() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "IPC socket parent must not contain symlinks",
                ));
            }
        }
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "IPC socket parent is not a directory",
        ));
    }
    if metadata.uid() != libc_getuid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC socket parent has the wrong owner",
        ));
    }
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn prepare_socket_path(path: &Path) -> io::Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC socket path must not be a symlink",
        ));
    }
    if metadata.uid() != libc_getuid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "IPC socket has the wrong owner",
        ));
    }
    if metadata.file_type().is_socket() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "IPC socket is already in use",
            ));
        }
        fs::remove_file(path)
    } else {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "IPC socket path is not a socket",
        ))
    }
}

#[cfg(unix)]
fn libc_getuid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct SocketClient {
    path: PathBuf,
    next_id: Arc<AtomicU64>,
}

#[cfg(unix)]
impl SocketClient {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPC socket path must be absolute",
            ));
        }
        Ok(Self {
            path,
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    fn request(&self, command: Command, timeout: Duration) -> Result<ResponseData, EngineError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, command);
        let mut stream = UnixStream::connect(&self.path).map_err(|_| EngineError::Disconnected)?;
        stream
            .set_read_timeout(Some(timeout))
            .map_err(map_io_error)?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(map_io_error)?;
        let mut encoded = serde_json::to_vec(&request)
            .map_err(|error| EngineError::Failure(format!("encoding IPC request: {error}")))?;
        if encoded.len() > MAX_MESSAGE_BYTES {
            return Err(EngineError::Failure(
                "IPC request exceeds maximum size".into(),
            ));
        }
        encoded.push(b'\n');
        stream.write_all(&encoded).map_err(map_io_error)?;
        let reader = BufReader::new(stream);
        let mut limited = reader.take((MAX_MESSAGE_BYTES + 1) as u64);
        let mut line = String::new();
        let read = limited.read_line(&mut line).map_err(map_io_error)?;
        if read == 0 {
            return Err(EngineError::Failure("empty IPC response".into()));
        }
        if line.len() > MAX_MESSAGE_BYTES || !line.ends_with('\n') {
            return Err(EngineError::Failure(
                "invalid or oversized IPC response".into(),
            ));
        }
        line.pop();
        let response: Response = serde_json::from_str(&line)
            .map_err(|error| EngineError::Failure(format!("malformed IPC response: {error}")))?;
        if response.id != id || response.version != PROTOCOL_VERSION {
            return Err(EngineError::Failure(
                "IPC response ID or version mismatch".into(),
            ));
        }
        response.into_result()
    }
}

#[cfg(unix)]
impl EngineClient for SocketClient {
    fn apply(
        &mut self,
        preset: Preset,
        expected_revision: u64,
        timeout: Duration,
    ) -> Result<AppliedSnapshot, EngineError> {
        match self.request(
            Command::Apply {
                preset,
                expected_revision,
            },
            timeout,
        )? {
            ResponseData::Applied(snapshot) => Ok(snapshot),
            _ => Err(EngineError::Failure("unexpected Apply response".into())),
        }
    }

    fn status(&mut self, timeout: Duration) -> Result<EngineStatus, EngineError> {
        match self.request(Command::Status, timeout)? {
            ResponseData::Status(status) => Ok(status),
            _ => Err(EngineError::Failure("unexpected Status response".into())),
        }
    }

    fn bypass(
        &mut self,
        bypassed: bool,
        expected_revision: u64,
        timeout: Duration,
    ) -> Result<AppliedSnapshot, EngineError> {
        match self.request(
            Command::Bypass {
                bypassed,
                expected_revision,
            },
            timeout,
        )? {
            ResponseData::Applied(snapshot) => Ok(snapshot),
            _ => Err(EngineError::Failure("unexpected Bypass response".into())),
        }
    }

    fn select_output(
        &mut self,
        output: String,
        timeout: Duration,
    ) -> Result<EngineStatus, EngineError> {
        match self.request(Command::Output { output }, timeout)? {
            ResponseData::Status(status) => Ok(status),
            _ => Err(EngineError::Failure("unexpected Output response".into())),
        }
    }

    fn events(&mut self) -> Vec<EngineEvent> {
        match self.request(Command::Events, DEFAULT_TIMEOUT) {
            Ok(ResponseData::Events(events)) => events,
            _ => Vec::new(),
        }
    }
}
