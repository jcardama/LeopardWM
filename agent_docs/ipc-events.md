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

`lwm subscribe` consumes the `Subscribed` ack frame internally and only forwards subsequent `IpcEvent` frames to stdout. Custom clients speaking the wire protocol directly see the ack on the wire and must handle the parser mode-switch themselves (see [Critical: parser mode-switch after Subscribed](#critical-parser-mode-switch-after-subscribed) below).

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
{ "type": "workspace_changed", "monitor": 65537, "old_index": 0, "new_index": 1, "name": "code" }
```

`monitor` is the Win32 `HMONITOR` value (i64). `old_index` and `new_index` are 0-based; CLI displays as 1-based. The initial snapshot delivers one frame per monitor with `old_index == new_index == current`.

`name` is the display name of the new workspace, or `null` if unnamed (set via `[workspaces].names` in config). Bars should render the name when present and fall back to `new_index + 1` otherwise. Field is omitted-safe: older daemons don't send it, so treat a missing key as `null`.

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
    {
      "window_ids": [1223496256],
      "width_px": 1267,
      "height_weights": [1.0],
      "mode": { "type": "vertical" }
    },
    {
      "window_ids": [13764602, 1246800],
      "width_px": 1267,
      "height_weights": [0.6, 0.4],
      "mode": { "type": "tabbed", "active_idx": 0 }
    }
  ]
}
```

Carries enough column structure to render without a follow-up `QueryWorkspace`.

Per-column fields:

- `window_ids`: top-to-bottom in `vertical` mode; tab order in `tabbed` mode.
- `width_px`: intrinsic column width in pixels (monitor-independent strip dimensions).
- `height_weights`: per-window height fraction in `vertical` mode (parallel to `window_ids`, sums to ~1.0). Empty means equal distribution. Ignored in `tabbed` mode since only one window is visible.
- `mode`: tagged enum. `{"type":"vertical"}` (default) stacks all non-minimized windows top to bottom. `{"type":"tabbed","active_idx":N}` shows only `window_ids[N]` filling the column rect; bars should render a tab strip listing all `window_ids`. Missing in v1 payloads — bars deserializing the wire format should default to `vertical` when absent.

Sender-side dedup ensures only structurally-distinct layouts emit; mid-animation frames between two settled layouts are suppressed.

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

## Yasb integration

[Yasb](https://github.com/amnweb/yasb) ships komorebi and GlazeWM widgets in-tree but does not have LeopardWM widgets yet (will be revisited once we have user demand for it). Two paths today:

### Pure config (no plugin code)

Use Yasb's `custom` widget to run `lwm subscribe` as a subprocess and render the most recent line. Add this to your Yasb `config.yaml`:

```yaml
widgets:
  leopardwm_workspace:
    type: "yasb.custom.CustomWidget"
    options:
      label: "<span>WS {data}</span>"
      class_name: "leopardwm-workspace"
      exec_options:
        run_cmd: ["lwm", "subscribe", "--events", "workspace"]
        run_interval: 0   # 0 means run once, keep stdout open
        return_format: "json"
      callbacks:
        on_left: "do_nothing"
```

`run_interval: 0` keeps the subscription open for the life of the bar. `return_format: "json"` parses each newline-delimited frame; `{data}` in the label references the parsed event. Replace `workspace` with any comma-separated filter from the [Event kinds table](#event-kinds).

This won't render a per-workspace strip or a title with executable icon — just whatever a single `{data}` placeholder can show. For richer rendering, write a real Python plugin (sketch below) or wait until a first-class LeopardWM widget set lands in Yasb.

### Plugin sketch (Python)

A real Yasb plugin would consume the stream and update multiple labels:

```python
import json
import subprocess

proc = subprocess.Popen(["lwm", "subscribe", "--events", "workspace,focused_window,layout"],
                        stdout=subprocess.PIPE, text=True, bufsize=1)
for line in proc.stdout:
    event = json.loads(line)
    if event["type"] == "workspace_changed":
        update_workspace_widget(new_index=event["new_index"])
    elif event["type"] == "focused_window_changed":
        update_title_widget(title=event.get("title"))
    elif event["type"] == "layout_changed":
        update_layout_widget(columns=event["columns"], focused=event["focused_column"])
```

## Adding new IPC events (daemon developers)

The pub/sub surface is part of the public LeopardWM contract. When adding daemon state that bars would want to observe, wire it into the IPC event stream rather than expecting consumers to poll:

1. **Add the event variant** to `IpcEvent` and `EventKind` in `crates/ipc/src/lib.rs`. Pick a filter name (snake_case) for `EventKind` and a `#[serde(tag = "type")]` discriminator for `IpcEvent`.
2. **Broadcast on every state-mutation site** that changes the observable value, using `self.broadcast_event(IpcEvent::Foo { ... })` from `AppState`. Both OS-driven paths (`event_handler.rs`) and command-driven paths (`command_handler.rs`, `helpers.rs::sync_foreground_window`, drag finalization) need coverage — a bar should see the same event regardless of what caused the change.
3. **Include the new kind in the connection snapshot** at `main.rs::handle_subscribe` so reconnecting subscribers receive the current value, not just future changes.
4. **Update this document**: add a row to the [Event kinds table](#event-kinds), write the schema under [Event schemas](#event-schemas), bump the `crates/ipc/src/lib.rs` round-trip test to cover the new variant.
5. **Watch the broadcast capacity** (256). Events that fire at animation-frame rates need sender-side dedup (see how `LayoutChanged` collapses mid-transition frames).

Check `MEMORY.md` → "IPC bar-integration validation deferred" before building reference consumers. Real bar work is deferred until first user demand; bug-fix work uncovered by inspection still lands.

## Limitations (v1)

- **Per-monitor filter not supported** — `--events` filters by kind only. Filter by `monitor` field client-side.
- **`WindowCreated` / `Destroyed` events not emitted** — focus + layout cover most bar UX needs. Add via the issue tracker if you need fine-grained window lifecycle.
- **No WebSocket bridge** — the daemon serves only the named pipe. Browser-based bars need a thin bridge component.
- **Stream mode is uni-directional** — after Subscribe, the pipe only flows daemon→client. Open a second pipe for command queries.
- **Daemon shutdown** delivers EOF (`BrokenPipe` on next read). Reconnect with backoff if your bar should survive daemon restarts.

## Validation history

- **2026-05-18**: Manual walk against v0.1.16 daemon confirmed `WorkspaceChanged`, `LayoutChanged`, `ConfigReloaded`, `Heartbeat`, snapshot delivery, and filter behavior all match docs. Two issues surfaced and were both fixed in v0.1.17: (1) `LayoutChanged.columns[].mode` was emitted but undocumented (docs updated); (2) `FocusedWindowChanged` did not fire for command-initiated focus changes (`lwm focus left/right`, workspace switches) because `sync_foreground_window` was pre-updating the OS-side dedup baseline before Windows could fire `EVENT_SYSTEM_FOREGROUND`. Daemon now tracks `last_broadcast_focused_hwnd` independently of `previous_focused_hwnd`, and `sync_foreground_window` broadcasts through the same dedup helper as the OS event handler. All focus changes — OS-driven, command-driven, recovery-path — now emit through one gate.
