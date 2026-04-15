//! emberd — lightweight in-VM daemon for Ember VMs.
//!
//! Listens on vsock port 1024 (production, Linux) or a Unix domain socket
//! (testing) and serves JSON-lines requests. Matches the protocol expected
//! by Thermite's `EmberdClient` (`daemon_client.py`).
//!
//! Operations: ping, exec, read_file, write_file, agent_status.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use clap::Parser;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::OnceLock;
use std::time::Instant;

#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd};

/// Process start time, used as fallback when /proc/uptime is unavailable.
static START_TIME: OnceLock<Instant> = OnceLock::new();

/// Default vsock port for worker communication (matches Thermite VsockChannel.WORKER).
const DEFAULT_PORT: u32 = 1024;

#[derive(Parser)]
#[command(
    name = "emberd",
    version,
    about = "Lightweight in-VM daemon for Ember VMs"
)]
struct Args {
    /// vsock port to listen on (Linux only).
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u32,

    /// Listen on a Unix domain socket instead of vsock (for testing).
    #[arg(long)]
    uds: Option<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    START_TIME.get_or_init(Instant::now);
    let args = Args::parse();
    eprintln!("emberd v{}", env!("CARGO_PKG_VERSION"));

    if let Some(ref path) = args.uds {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        eprintln!("listening on UDS: {path}");
        accept_loop_uds(listener)
    } else {
        #[cfg(target_os = "linux")]
        {
            listen_vsock(args.port)
        }
        #[cfg(not(target_os = "linux"))]
        {
            eprintln!("vsock requires Linux. Use --uds for testing.");
            std::process::exit(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Listeners
// ---------------------------------------------------------------------------

fn accept_loop_uds(listener: UnixListener) -> Result<(), Box<dyn std::error::Error>> {
    for stream in listener.incoming() {
        let stream = stream?;
        std::thread::spawn(move || {
            if let Err(e) = handle_connection(stream) {
                eprintln!("connection error: {e}");
            }
        });
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn listen_vsock(port: u32) -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::{
        accept, bind, listen, socket, AddressFamily, SockFlag, SockType, VsockAddr,
    };

    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )?;
    // VMADDR_CID_ANY = 0xFFFFFFFF — accept connections from any CID.
    let addr = VsockAddr::new(0xFFFFFFFF, port);
    bind(fd.as_raw_fd(), &addr)?;
    listen(&fd, 128)?;
    eprintln!("listening on vsock port {port}");

    loop {
        let client_fd = accept(fd.as_raw_fd())?;
        std::thread::spawn(move || {
            // Safety: client_fd is a valid open fd returned by accept().
            let file = unsafe { std::fs::File::from_raw_fd(client_fd) };
            if let Err(e) = handle_connection(file) {
                eprintln!("connection error: {e}");
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Connection handler — generic over any Read + Write stream
// ---------------------------------------------------------------------------

fn handle_connection<S: Read + Write>(stream: S) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                let resp = json!({"error": format!("invalid JSON: {e}")});
                write_response(reader.get_mut(), &resp)?;
                continue;
            }
        };

        let resp = dispatch(&req);
        write_response(reader.get_mut(), &resp)?;
    }

    Ok(())
}

fn write_response<W: Write>(w: &mut W, resp: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *w, resp)?;
    w.write_all(b"\n")?;
    w.flush()
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn dispatch(req: &Value) -> Value {
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");
    match op {
        "ping" => op_ping(),
        "exec" => op_exec(req),
        "read_file" => op_read_file(req),
        "write_file" => op_write_file(req),
        "agent_status" => op_agent_status(),
        _ => json!({"error": format!("unknown op: {op}")}),
    }
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

fn op_ping() -> Value {
    let uptime = read_proc_uptime()
        .unwrap_or_else(|| START_TIME.get().map_or(0.0, |t| t.elapsed().as_secs_f64()));
    json!({"ok": true, "uptime_seconds": uptime})
}

fn op_exec(req: &Value) -> Value {
    let Some(command) = req.get("command").and_then(Value::as_str) else {
        return json!({"error": "missing 'command' field"});
    };

    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(command);

    if let Some(env) = req.get("env").and_then(Value::as_object) {
        for (k, v) in env {
            if let Some(v_str) = v.as_str() {
                cmd.env(k, v_str);
            }
        }
    }

    match cmd.output() {
        Ok(output) => json!({
            "exit_code": output.status.code().unwrap_or(-1),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        }),
        Err(e) => json!({
            "exit_code": -1,
            "stdout": "",
            "stderr": format!("exec error: {e}"),
        }),
    }
}

fn op_read_file(req: &Value) -> Value {
    let Some(path) = req.get("path").and_then(Value::as_str) else {
        return json!({"error": "missing 'path' field"});
    };
    match std::fs::read(path) {
        Ok(data) => json!({"data": B64.encode(data)}),
        Err(e) => json!({"error": format!("read_file: {e}")}),
    }
}

fn op_write_file(req: &Value) -> Value {
    let Some(path) = req.get("path").and_then(Value::as_str) else {
        return json!({"error": "missing 'path' field"});
    };
    let Some(data_b64) = req.get("data").and_then(Value::as_str) else {
        return json!({"error": "missing 'data' field"});
    };
    let data = match B64.decode(data_b64) {
        Ok(d) => d,
        Err(e) => return json!({"error": format!("base64 decode: {e}")}),
    };
    match std::fs::write(path, data) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"error": format!("write_file: {e}")}),
    }
}

fn op_agent_status() -> Value {
    let pid = find_agent_pid();
    let task_id = std::fs::read_to_string("/tmp/thermite-task-id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    match pid {
        Some(pid) => json!({
            "running": true,
            "pid": pid,
            "task_id": task_id,
        }),
        None => json!({
            "running": false,
            "pid": null,
            "task_id": null,
        }),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn read_proc_uptime() -> Option<f64> {
    std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Scan /proc for a process whose cmdline contains "thermite-entrypoint".
fn find_agent_pid() -> Option<u32> {
    let proc_dir = std::fs::read_dir("/proc").ok()?;
    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if cmdline.contains("thermite-entrypoint") {
            return Some(pid);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_returns_ok_and_uptime() {
        START_TIME.get_or_init(Instant::now);
        let resp = dispatch(&json!({"op": "ping"}));
        assert_eq!(resp["ok"], true);
        assert!(resp["uptime_seconds"].as_f64().unwrap() >= 0.0);
    }

    #[test]
    fn exec_echo() {
        let resp = dispatch(&json!({"op": "exec", "command": "echo hello"}));
        assert_eq!(resp["exit_code"], 0);
        assert_eq!(resp["stdout"], "hello\n");
        assert_eq!(resp["stderr"], "");
    }

    #[test]
    fn exec_with_env() {
        let resp = dispatch(&json!({
            "op": "exec",
            "command": "echo $TEST_VAR",
            "env": {"TEST_VAR": "hello_from_env"}
        }));
        assert_eq!(resp["exit_code"], 0);
        assert_eq!(resp["stdout"], "hello_from_env\n");
    }

    #[test]
    fn exec_nonzero_exit() {
        let resp = dispatch(&json!({"op": "exec", "command": "exit 42"}));
        assert_eq!(resp["exit_code"], 42);
    }

    #[test]
    fn exec_missing_command() {
        let resp = dispatch(&json!({"op": "exec"}));
        assert!(resp["error"].as_str().unwrap().contains("command"));
    }

    #[test]
    fn exec_stderr() {
        let resp = dispatch(&json!({"op": "exec", "command": "echo err >&2"}));
        assert_eq!(resp["exit_code"], 0);
        assert_eq!(resp["stderr"], "err\n");
    }

    #[test]
    fn read_write_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        let path_str = path.to_str().unwrap();

        let original = b"hello world\xfe\xff";
        let encoded = B64.encode(original);

        // Write
        let resp = dispatch(&json!({
            "op": "write_file",
            "path": path_str,
            "data": encoded,
        }));
        assert_eq!(resp["ok"], true);

        // Read back
        let resp = dispatch(&json!({"op": "read_file", "path": path_str}));
        let decoded = B64.decode(resp["data"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, original);
    }

    #[test]
    fn read_file_not_found() {
        let resp = dispatch(&json!({"op": "read_file", "path": "/tmp/emberd-nonexistent-file"}));
        assert!(resp["error"].as_str().unwrap().contains("read_file"));
    }

    #[test]
    fn write_file_missing_path() {
        let resp = dispatch(&json!({"op": "write_file", "data": "aGVsbG8="}));
        assert!(resp["error"].as_str().unwrap().contains("path"));
    }

    #[test]
    fn write_file_bad_base64() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.txt");
        let resp = dispatch(&json!({
            "op": "write_file",
            "path": path.to_str().unwrap(),
            "data": "not-valid-base64!!!",
        }));
        assert!(resp["error"].as_str().unwrap().contains("base64"));
    }

    #[test]
    fn agent_status_not_running() {
        // No thermite-entrypoint process is running during tests.
        let resp = dispatch(&json!({"op": "agent_status"}));
        assert_eq!(resp["running"], false);
        assert!(resp["pid"].is_null());
    }

    #[test]
    fn unknown_op() {
        let resp = dispatch(&json!({"op": "foobar"}));
        assert!(resp["error"].as_str().unwrap().contains("unknown op"));
    }

    #[test]
    fn missing_op() {
        let resp = dispatch(&json!({"hello": "world"}));
        assert!(resp["error"].as_str().unwrap().contains("unknown op"));
    }

    // -- Integration test via UDS --

    #[test]
    fn uds_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("emberd.sock");

        let listener = UnixListener::bind(&sock_path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream).unwrap();
        });

        let stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
        let mut writer = &stream;
        let mut reader = BufReader::new(&stream);

        // ping
        writer.write_all(b"{\"op\":\"ping\"}\n").unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(resp["ok"], true);

        // exec
        line.clear();
        writer
            .write_all(b"{\"op\":\"exec\",\"command\":\"echo uds_test\"}\n")
            .unwrap();
        writer.flush().unwrap();
        reader.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(resp["exit_code"], 0);
        assert_eq!(resp["stdout"], "uds_test\n");

        // read/write roundtrip
        let tmp = dir.path().join("uds_file.txt");
        let tmp_str = tmp.to_str().unwrap();
        let b64 = B64.encode(b"uds roundtrip data");

        line.clear();
        let msg = format!("{{\"op\":\"write_file\",\"path\":\"{tmp_str}\",\"data\":\"{b64}\"}}\n");
        writer.write_all(msg.as_bytes()).unwrap();
        writer.flush().unwrap();
        reader.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(resp["ok"], true);

        line.clear();
        let msg = format!("{{\"op\":\"read_file\",\"path\":\"{tmp_str}\"}}\n");
        writer.write_all(msg.as_bytes()).unwrap();
        writer.flush().unwrap();
        reader.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        let decoded = B64.decode(resp["data"].as_str().unwrap()).unwrap();
        assert_eq!(decoded, b"uds roundtrip data");

        // Close connection to let handle_connection exit
        drop(stream);
        handle.join().unwrap();
    }

    #[test]
    fn invalid_json_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("bad.sock");

        let listener = UnixListener::bind(&sock_path).unwrap();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle_connection(stream).unwrap();
        });

        let stream = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
        let mut writer = &stream;
        let mut reader = BufReader::new(&stream);

        writer.write_all(b"not json\n").unwrap();
        writer.flush().unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let resp: Value = serde_json::from_str(&line).unwrap();
        assert!(resp["error"].as_str().unwrap().contains("invalid JSON"));

        drop(stream);
        handle.join().unwrap();
    }
}
