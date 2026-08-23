//! AgentOS IPC transport.
//!
//! Every AgentOS system service exposes a Unix-domain socket under
//! `/run/agentos/<name>.sock` and speaks a deliberately simple protocol:
//!
//! * Requests:  `{"id":<u64>,"method":"<name>","params":{...}}\n`
//! * Responses: `{"id":<u64>,"ok":true,"result":{...}}\n`
//!              `{"id":<u64>,"ok":false,"error":{"code":<i32>,"message":"..."}}\n`
//! * Server-pushed events (e.g. the agent bus):
//!              `{"event":"<name>","params":{...}}\n`
//!
//! Why this instead of D-Bus/gRPC/other: it is kernel-native (no broker to
//! install or crash), permissions are enforced by the filesystem on the socket
//! node, it is language-agnostic, and you can debug any service with
//! `echo '{"id":1,"method":"ping"}' | socat - UNIX-CONNECT:/run/agentos/core.sock`.
//! The cost (no discovery, no rich typing on the wire) is acceptable for
//! Phase 1 and the envelope is trivially upgradable later.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub const ERR_UNKNOWN_METHOD: i32 = -32601;
pub const ERR_INVALID_PARAMS: i32 = -32602;
pub const ERR_INTERNAL: i32 = -32603;
pub const ERR_PERMISSION: i32 = -32000;
pub const ERR_NOT_FOUND: i32 = -32004;
pub const ERR_INVALID_STATE: i32 = -32005;
/// Maximum newline-delimited JSON frame accepted by a service.
/// IPC is local, but a malformed process must not allocate unbounded memory.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcRequest {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        RpcError {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RpcResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        RpcResponse {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: u64, code: i32, message: impl Into<String>) -> Self {
        RpcResponse {
            id,
            ok: false,
            result: None,
            error: Some(RpcError::new(code, message)),
        }
    }
}

/// A server-pushed event frame (used by the agent bus).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub event: String,
    pub params: serde_json::Value,
}

/// Request handler. Must be quick: each connection is served by a thread and
/// handlers share service state behind the service's own locks.
pub type Handler = Arc<dyn Fn(RpcRequest) -> RpcResponse + Send + Sync + 'static>;

/// Remove a stale socket file if it exists and nothing is listening on it.
pub fn clean_stale_socket(path: &Path) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a Unix socket", path.display()),
        ));
    }
    match UnixStream::connect(path) {
        Ok(_) => {
            // Something answered: refuse to clobber a live socket.
            Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("socket {} is live", path.display()),
            ))
        }
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            std::fs::remove_file(path)
        }
        Err(error) => Err(error),
    }
}

/// Apply the image's socket policy after binding. The default is group-readable
/// and group-writable (0660), not world-readable. The initramfs exports
/// `AGENTOS_SOCKET_GID=1000` so the unprivileged desktop/runtime group can
/// reach the service endpoints. Development runs can omit that variable.
pub fn set_socket_permissions(path: &Path) -> std::io::Result<()> {
    let mode = std::env::var("AGENTOS_SOCKET_MODE")
        .ok()
        .and_then(|value| u32::from_str_radix(value.trim_start_matches('0'), 8).ok())
        .unwrap_or(0o660);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    if let Some(gid) = std::env::var("AGENTOS_SOCKET_GID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
    {
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket path contains NUL")
        })?;
        // -1 leaves the uid unchanged. Failure is returned so an image cannot
        // silently boot with a weaker-than-declared socket boundary.
        let result = unsafe { libc::chown(c_path.as_ptr(), u32::MAX, gid) };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A request/response server on a Unix socket. Runs forever; call from a
/// dedicated thread if the process has other work.
pub fn serve(socket: &Path, handler: Handler) -> std::io::Result<()> {
    clean_stale_socket(socket)?;
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(socket)?;
    set_socket_permissions(socket)?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let handler = handler.clone();
                std::thread::spawn(move || {
                    let _ = serve_connection(stream, handler);
                });
            }
            Err(e) => {
                agentos_log("warn", &format!("accept error: {e}"));
            }
        }
    }
    Ok(())
}

fn serve_connection(stream: UnixStream, handler: Handler) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Ok(()); // client closed
        }
        if n > MAX_FRAME_BYTES {
            let response = RpcResponse::err(0, ERR_INVALID_PARAMS, "request frame exceeds 1 MiB");
            let mut output = serde_json::to_string(&response)?;
            output.push('\n');
            stream.write_all(output.as_bytes())?;
            return Ok(());
        }
        let resp = match serde_json::from_str::<RpcRequest>(&line) {
            Ok(req) => {
                let mut r = handler(req);
                r.id = r.id.max(0);
                r
            }
            Err(e) => RpcResponse::err(0, ERR_INVALID_PARAMS, format!("malformed request: {e}")),
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        stream.write_all(out.as_bytes())?;
        stream.flush()?;
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: AtomicU64,
}

impl Client {
    pub fn connect(socket: &Path) -> std::io::Result<Client> {
        Client::connect_timeout(socket, Duration::from_secs(2))
    }

    pub fn connect_timeout(socket: &Path, _timeout: Duration) -> std::io::Result<Client> {
        // Unix domain connects are local and effectively immediate; the
        // timeout is kept in the API for symmetry with future transports.
        let stream = UnixStream::connect(socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Client {
            stream,
            reader,
            next_id: AtomicU64::new(1),
        })
    }

    /// Issue a call and return the `result` object.
    pub fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = RpcRequest {
            id,
            method: method.to_string(),
            params,
        };
        let mut line =
            serde_json::to_string(&req).map_err(|e| RpcError::new(ERR_INTERNAL, e.to_string()))?;
        line.push('\n');
        self.stream.write_all(line.as_bytes()).map_err(io_to_rpc)?;
        self.stream.flush().map_err(io_to_rpc)?;

        let mut resp_line = String::new();
        self.reader.read_line(&mut resp_line).map_err(io_to_rpc)?;
        if resp_line.is_empty() {
            return Err(RpcError::new(ERR_INTERNAL, "service closed connection"));
        }
        // The bus may interleave event frames; skip them while waiting for
        // our response id. Other response ids are also ignored so a future
        // multiplexed client can share a connection safely.
        loop {
            let frame: serde_json::Value = serde_json::from_str(&resp_line)
                .map_err(|e| RpcError::new(ERR_INTERNAL, format!("bad response: {e}")))?;
            if frame.get("event").is_some() {
                resp_line.clear();
                let n = self.reader.read_line(&mut resp_line).map_err(io_to_rpc)?;
                if n == 0 {
                    return Err(RpcError::new(
                        ERR_INTERNAL,
                        "service closed connection mid-call",
                    ));
                }
                continue;
            }
            let resp: RpcResponse = serde_json::from_value(frame)
                .map_err(|e| RpcError::new(ERR_INTERNAL, format!("bad response: {e}")))?;
            if resp.id == id {
                if resp.ok {
                    return Ok(resp.result.unwrap_or(serde_json::Value::Null));
                }
                return Err(resp
                    .error
                    .unwrap_or_else(|| RpcError::new(ERR_INTERNAL, "unknown error")));
            }
            resp_line.clear();
            let n = self.reader.read_line(&mut resp_line).map_err(io_to_rpc)?;
            if n == 0 {
                return Err(RpcError::new(
                    ERR_INTERNAL,
                    "service closed connection mid-call",
                ));
            }
        }
    }
}

fn io_to_rpc(e: std::io::Error) -> RpcError {
    RpcError::new(ERR_INTERNAL, format!("ipc io error: {e}"))
}

/// One-shot convenience call: connect, call, close.
pub fn call(
    socket: &Path,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, RpcError> {
    let mut c = Client::connect(socket).map_err(io_to_rpc)?;
    c.call(method, params)
}

fn agentos_log(level: &str, msg: &str) {
    eprintln!("[agentos-ipc/{level}] {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn wire_format_roundtrip() {
        let req: RpcRequest =
            serde_json::from_str(r#"{"id":7,"method":"ping","params":{"a":1}}"#).unwrap();
        assert_eq!(req.method, "ping");
        let s =
            serde_json::to_string(&RpcResponse::ok(7, serde_json::json!({"pong":true}))).unwrap();
        assert!(s.contains(r#""ok":true"#));
        let back: RpcResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, 7);
    }

    #[test]
    fn malformed_request_yields_error_frame() {
        let resp = serde_json::from_str::<RpcRequest>("nonsense")
            .err()
            .map(|_| RpcResponse::err(0, ERR_INVALID_PARAMS, "malformed request"))
            .unwrap();
        assert!(!resp.ok);
    }

    #[test]
    fn server_and_client_over_real_socket() {
        let dir = std::env::temp_dir().join(format!("agentos-ipc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("t.sock");
        let hits = Arc::new(AtomicUsize::new(0));
        let h2 = hits.clone();
        let handler: Handler = Arc::new(move |req| {
            let n = h2.fetch_add(1, Ordering::Relaxed);
            match req.method.as_str() {
                "ping" => RpcResponse::ok(req.id, serde_json::json!({"pong":true,"n":n})),
                "boom" => RpcResponse::err(req.id, ERR_INVALID_PARAMS, "expected boom"),
                _ => RpcResponse::err(req.id, ERR_UNKNOWN_METHOD, "no such method"),
            }
        });
        let server_sock = sock.clone();
        std::thread::spawn(move || {
            let _ = serve(&server_sock, handler);
        });
        // wait for socket to appear
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut c = Client::connect(&sock).unwrap();
        let r = c.call("ping", serde_json::json!({})).unwrap();
        assert_eq!(r["pong"], serde_json::json!(true));
        let e = c.call("boom", serde_json::json!({})).unwrap_err();
        assert_eq!(e.code, ERR_INVALID_PARAMS);
        let e2 = c.call("nope", serde_json::json!({})).unwrap_err();
        assert_eq!(e2.code, ERR_UNKNOWN_METHOD);
        let r2 = c.call("ping", serde_json::json!({})).unwrap();
        assert_eq!(r2["n"], serde_json::json!(3));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
