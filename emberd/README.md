# emberd

Lightweight in-VM daemon for [Ember](https://github.com/aljoscha/ember) VMs.
Serves JSON-lines requests over vsock or Unix domain sockets, providing
structured host-guest communication for the
[Thermite](https://github.com/jasonhernandez/thermite) orchestrator.

## Why

Without emberd, Thermite communicates with VMs via SSH — shelling out for
every exec, file read, and status check. This works but adds latency and
fragility at scale. emberd replaces SSH with a direct vsock channel:
structured requests in, structured responses out, no shell parsing.

Thermite's `EmberdClient` speaks this protocol natively. When emberd is
running in a VM, Thermite automatically routes through vsock. When it's
not (legacy images, crash), Thermite falls back to SSH transparently.

## Protocol

JSON lines over vsock port 1024. One request per line, one response per line.

### ping

```
-> {"op":"ping"}
<- {"ok":true,"uptime_seconds":123.45}
```

### exec

```
-> {"op":"exec","command":"echo hello","env":{"FOO":"bar"}}
<- {"exit_code":0,"stdout":"hello\n","stderr":""}
```

### read_file

```
-> {"op":"read_file","path":"/tmp/result.json"}
<- {"data":"eyJzdGF0dXMiOiAiZG9uZSJ9"}
```

Content is base64-encoded.

### write_file

```
-> {"op":"write_file","path":"/tmp/config.json","data":"eyJ0YXNrIjogIlNFQy0yMDAifQ=="}
<- {"ok":true}
```

### agent_status

```
-> {"op":"agent_status"}
<- {"running":true,"pid":9999,"task_id":"SEC-200"}
```

Scans `/proc` for a process matching `thermite-entrypoint` and reads
`/tmp/thermite-task-id` for the task ID.

## Build

```bash
# From the ember repo root:
cargo build -p emberd            # debug
cargo build -p emberd --release  # release
```

emberd is a workspace member with minimal dependencies (clap, serde_json,
base64). The vsock listener uses nix and only compiles on Linux. On macOS,
emberd builds in UDS-only mode for testing.

## Usage

```bash
# Production (inside a Linux VM, listens on vsock port 1024)
emberd

# Custom port
emberd --port 2048

# Testing (Unix domain socket, works on any platform)
emberd --uds /tmp/emberd.sock
```

## Testing

```bash
cargo test -p emberd
```

15 tests covering all operations, error handling, and a full UDS
integration roundtrip.

## Image integration

emberd is baked into Ember VM images via Dockerfile and starts on boot:

```ini
# /etc/systemd/system/emberd.service
[Service]
Type=simple
ExecStart=/usr/local/bin/emberd --port 1024
Restart=always
```

Build and stage for image builds:

```bash
make emberd-image   # builds for Linux, copies to images/emberd
```

## Architecture

```
Host (Thermite)                    Guest VM (emberd)
+-----------------+                +------------------+
| EmberdClient    |                |  emberd          |
|  VsockTransport |--> UDS --> Firecracker/AVF --> AF_VSOCK |
|  (or SSH        |    bridge     |  port 1024       |
|   fallback)     |               |  JSON lines      |
+-----------------+                +------------------+
```

- **Host side**: Thermite connects to `<state_dir>/vms/<name>/vsock.sock`
- **Bridge**: Firecracker or ember-vz (AVF) bridges UDS to vsock
- **Guest side**: emberd listens on `AF_VSOCK` port 1024
- **Fallback**: If emberd isn't running, Thermite uses SSH automatically
