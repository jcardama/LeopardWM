# LeopardWM IPC Events (pub/sub)

Bars and other external tools can subscribe to LeopardWM state changes over the same Windows named pipe used for commands. The daemon pushes JSON-encoded events as workspaces switch, focus changes, layouts settle, and config reloads.

## Quick start

```powershell
# All events as newline-delimited JSON
lwm subscribe

# Only what you care about
lwm subscribe --events workspace,focused_window | jq
```

Press Ctrl+C to disconnect. The daemon does not need to know who is listening; reconnect any time.

## Wire format

- **Pipe**: `\\.\pipe\leopardwm_<scope>` where `<scope>` is the lowercased `USERDOMAIN\USERNAME` (e.g. `\\.\pipe\leopardwm_my-pc_jose`). Use `leopardwm_ipc::preferred_pipe_name()` from Rust or hard-code per the docs in `crates/ipc/src/lib.rs:11-71`.
- **Framing**: newline-delimited JSON (`\n`), one logical message per line. UTF-8.
- **Per-frame size cap**: 64 KiB (`MAX_IPC_MESSAGE_SIZE` in `crates/ipc/src/lib.rs:22`).

## Connection lifecycle

```
client                                    daemon
  ──────────────────────────────────────►
  open pipe (named-pipe connect)
  send `{"type":"subscribe","events":[...]}`\n
                                          ──►  read command
                                               atomically subscribe to broadcaster
                                               + build snapshot under AppState mutex
  ◄──  read `{"status":"subscribed",...}`\n      write Subscribed ack
  ◄──  read snapshot frames (one per kind)       write snapshot
       SWITCH PARSER: subsequent frames are IpcEvent, NOT IpcResponse
  ◄──  read `{"type":"workspace_changed",...}`\n  on every state change matching filter
  ◄──  read `{"type":"heartbeat","uptime_seconds":...}`\n  every 30s of silence
  ...
  close pipe (Ctrl+C / drop)              detect via write error → drop receiver
```

### Critical: parser mode-switch after Subscribed

The first frame on the wire (the ack) deserializes as `IpcResponse` — its serde tag is `status`. **Every subsequent frame** deserializes as `IpcEvent` — its serde tag is `type`. They share the JSON-line wire format but **incompatible discriminator fields**, so a client that keeps parsing event frames as `IpcResponse` will fail.

Rust clients can use the typed approach (read first as `IpcResponse`, switch to `IpcEvent` for the rest). Other clients should branch on the presence of `"status"` vs `"type"` at the JSON-object level.

### Lagged recovery

If a subscriber falls more than 256 events behind (the broadcast capacity), the daemon delivers an `IpcEvent::Lagged { skipped: N }` frame. The recommended recovery is to **close the pipe and re-`Subscribe`** — the daemon's snapshot is atomically taken under the AppState mutex so the new subscription delivers a complete current state. After Subscribe, the pipe is in stream mode and cannot be used to issue command queries; if you need a query while subscribed, open a second pipe.

## Event kinds

| Kind | Filter name | Meaning |
|---|---|---|
| `WorkspaceChanged` | `workspace` | Active workspace on a monitor changed |
| `FocusedWindowChanged` | `focused_window` | Focused window changed (or was cleared) |
| `LayoutChanged` | `layout` | Column structure on the focused workspace settled |
| `ConfigReloaded` | `config` | `lwm reload` completed |
| `Heartbeat` | `heartbeat` | Liveness signal every 30s of silence |
| `Lagged` | (always delivered) | Broadcast buffer overflow; reconnect for fresh snapshot |

The `events` field of `Subscribe` accepts any subset of filter names (comma-separated on the CLI). An empty set means "all kinds".

## Event schemas

### `WorkspaceChanged`

```json
{ "type": "workspace_changed", "monitor": 65537, "old_index": 0, "new_index": 1 }
```

`monitor` is the Win32 `HMONITOR` value (i64). `old_index` and `new_index` are 0-based; CLI displays as 1-based. The initial snapshot delivers one frame per monitor with `old_index == new_index == current`.

### `FocusedWindowChanged`

```json
{
  "type": "focused_window_changed",
  "monitor": 65537,
  "hwnd": 1223496256,
  "title": "Beeper",
  "class_name": "Chrome_WidgetWin_1",
  "executable": "Beeper.exe"
}
```

`hwnd: null`, `title: null`, `class_name: null`, `executable: null` when focus was cleared (e.g. focus moved to taskbar / settings).

### `LayoutChanged`

```json
{
  "type": "layout_changed",
  "monitor": 65537,
  "workspace_index": 0,
  "focused_column": 1,
  "columns": [
    { "window_ids": [1223496256], "width_px": 1267, "height_weights": [1.0] },
    { "window_ids": [13764602, 1246800], "width_px": 1267, "height_weights": [0.6, 0.4] }
  ]
}
```

Carries enough column structure to render without a follow-up `QueryWorkspace`. `width_px` is the intrinsic column width in pixels (monitor-independent — the strip dimensions). `height_weights` is the per-window height fraction within a stacked column (parallel to `window_ids`, sums to ~1.0); empty means equal distribution. Sender-side dedup ensures only structurally-distinct layouts emit; mid-animation frames between two settled layouts are suppressed.

### `ConfigReloaded`

```json
{ "type": "config_reloaded" }
```

No payload. Subscribers should re-read `lwm config show` or re-render any config-derived UI.

### `Heartbeat`

```json
{ "type": "heartbeat", "uptime_seconds": 12345 }
```

Sent after 30s of silence on a stream. `uptime_seconds` is the time since the *subscriber* connected, not the daemon's lifetime — useful for long-lived connections to detect "did we just reconnect" vs "are we drifting".

### `Lagged`

```json
{ "type": "lagged", "skipped": 42 }
```

Sent when the broadcast buffer dropped events for this subscriber. Always delivered regardless of filter. **Recovery**: close and re-Subscribe.

## Sample clients

### Rust (using `tokio::net::windows::named_pipe`)

```rust
use leopardwm_ipc::{IpcCommand, IpcResponse, IpcEvent, EventKind, preferred_pipe_name};
use std::collections::BTreeSet;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::ClientOptions;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pipe = ClientOptions::new().open(preferred_pipe_name())?;
    let (reader, mut writer) = tokio::io::split(pipe);
    let mut buf = BufReader::new(reader);

    let cmd = IpcCommand::Subscribe { events: BTreeSet::new() };  // all kinds
    writer.write_all((serde_json::to_string(&cmd)? + "\n").as_bytes()).await?;

    let mut line = String::new();
    buf.read_line(&mut line).await?;
    let _ack: IpcResponse = serde_json::from_str(line.trim())?;

    loop {
        line.clear();
        if buf.read_line(&mut line).await? == 0 { break; }
        let event: IpcEvent = serde_json::from_str(line.trim())?;
        println!("{:?}", event);
    }
    Ok(())
}
```

### Python (using `pywin32`)

```python
import json
import win32file

PIPE = r"\\.\pipe\leopardwm_<your-scope>"

handle = win32file.CreateFile(
    PIPE, win32file.GENERIC_READ | win32file.GENERIC_WRITE,
    0, None, win32file.OPEN_EXISTING, 0, None,
)

# Subscribe (empty events = all kinds)
win32file.WriteFile(handle, b'{"type":"subscribe","events":[]}\n')

# Read frames line by line
buf = b""
while True:
    err, data = win32file.ReadFile(handle, 4096)
    if not data: break
    buf += data
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        msg = json.loads(line)
        print(msg)
```

### PowerShell (one-liner for ad-hoc inspection)

```powershell
lwm subscribe | ForEach-Object { ConvertFrom-Json $_ | Format-Table -AutoSize }
```

## Yasb integration recipe

[Yasb](https://github.com/amnweb/yasb) widgets can shell out to `lwm subscribe` and re-render on each event. Sketch (a real widget would be a Python plugin):

```python
import json
import subprocess

proc = subprocess.Popen(["lwm", "subscribe", "--events", "workspace,focused_window"],
                        stdout=subprocess.PIPE, text=True, bufsize=1)
for line in proc.stdout:
    event = json.loads(line)
    if event["type"] == "workspace_changed":
        update_workspace_widget(new_index=event["new_index"])
    elif event["type"] == "focused_window_changed":
        update_title_widget(title=event.get("title"))
```

## Limitations (v1)

- **Per-monitor filter not supported** — `--events` filters by kind only. Filter by `monitor` field client-side.
- **`WindowCreated` / `Destroyed` events not emitted** — focus + layout cover most bar UX needs. Add via the issue tracker if you need fine-grained window lifecycle.
- **No WebSocket bridge** — the daemon serves only the named pipe. Browser-based bars need a thin bridge component.
- **Stream mode is uni-directional** — after Subscribe, the pipe only flows daemon→client. Open a second pipe for command queries.
- **Daemon shutdown** delivers EOF (`BrokenPipe` on next read). Reconnect with backoff if your bar should survive daemon restarts.
