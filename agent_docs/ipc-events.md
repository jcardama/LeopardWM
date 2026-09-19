# LeopardWM IPC Events (pub/sub)

Bars and other external tools can subscribe to LeopardWM state changes over the same Windows named pipe used for commands. The daemon pushes JSON-encoded events as workspaces switch, focus changes, layouts settle, and config reloads.

## Quick start

```powershell
# Legacy event kinds as newline-delimited JSON
lwm subscribe

# Only what you care about
lwm subscribe --events workspace,focused_window | jq

# Complete workspace membership on all monitors
lwm subscribe --events workspace_state
lwm query workspaces
```

Press Ctrl+C to disconnect. The daemon does not need to know who is listening; reconnect any time.

`lwm subscribe` consumes the `Subscribed` ack frame internally and only forwards subsequent `IpcEvent` frames to stdout. Custom clients speaking the wire protocol directly see the ack on the wire and must handle the parser mode-switch themselves (see [Critical: parser mode-switch after Subscribed](#critical-parser-mode-switch-after-subscribed) below).

## Wire format

- **Pipe**: `\\.\pipe\leopardwm_<scope>` where `<scope>` is the lowercased `USERDOMAIN\USERNAME` (e.g. `\\.\pipe\leopardwm_my-pc_jose`). Use `leopardwm_ipc::preferred_pipe_name()` from Rust or hard-code per the docs in `crates/ipc/src/lib.rs:11-71`.
- **Framing**: newline-delimited JSON (`\n`), one logical message per line. UTF-8.
- **Per-frame size cap**: 64 KiB (`MAX_IPC_MESSAGE_SIZE` in `crates/ipc/src/lib.rs`).

## Protocol versions and capability checks

The current IPC protocol is v4; the minimum supported version remains v1.
Version 2 added tabbed-column data and commands. Version 3 adds the one-shot
`QueryHotkeys` command (`{"type":"query_hotkeys"}`) and `HotkeyList` response
(`status: "hotkey_list"`) with binding records, scroll modifier, and issues.
Version 4 adds opt-in complete workspace-state snapshots and monitor-targeted
workspace switching.
See [the hotkey query contract](shortcut-guide.md#ipc-contract) for ordering,
collision resolution, and the distinction between configuration and runtime
registration health.

These additions preserve older clients because the wire changes are additive and
an empty subscription filter retains the legacy event set. A protocol number alone
does not prove workspace-state support: a subscriber must also confirm that the
`subscribed` response acknowledges `workspace_state`. One-shot consumers require a
successful `workspace_state_ready` response. Queries need a separate pipe while
subscribed.

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
| `WorkspaceSnapshotBegin/Chunk/End/Error` | `workspace_state` | Complete replacement state for every monitor and workspace |
| `WorkspaceChanged` | `workspace` | Active workspace on a monitor changed |
| `FocusedWindowChanged` | `focused_window` | Focused window changed (or was cleared) |
| `LayoutChanged` | `layout` | Column structure on the focused workspace settled |
| `ConfigReloaded` | `config` | `lwm reload` completed |
| `Heartbeat` | `heartbeat` | Liveness signal every 30s of silence |
| `Lagged` | (always delivered) | Broadcast buffer overflow; reconnect for fresh snapshot |

The `events` field of `Subscribe` accepts any subset of filter names (comma-separated on the CLI). An empty set preserves the legacy kinds (`workspace`, `focused_window`, `layout`, `config`, `heartbeat`). Complete workspace state requires explicitly including `workspace_state`; it can be combined with legacy filters.

## Complete workspace state

This opt-in extension reuses `Subscribe`, `IpcEvent`, and the existing transport.
It provides initial membership and replacement snapshots, including changes on
inactive workspaces. It is intended for bars and other state consumers.

```powershell
lwm subscribe --events workspace_state
lwm subscribe --events workspace_state,focused_window
lwm query workspaces
lwm workspace 2 --monitor '\\.\DISPLAY2'
```

`query workspaces` opens a separate pipe and prints one complete snapshot using
the same event frames as a subscription, then exits. The CLI consumes its initial
`workspace_state_ready` response. On the wire, send
`{"type":"query_workspace_state"}`; the first response is
`{"status":"workspace_state_ready","protocol_version":4}`.
The CLI gives the acknowledgment and each complete newline-terminated query frame
a five-second read deadline. A timeout fails the query, including a stalled partial
frame. The deadline resets for each frame; long-lived subscriptions have no query
read deadline or total lifetime limit.

A workspace subscription retains the existing `subscribed` response and echoes
`workspace_state` in `events`. After either response, switch to the event parser:

```json
{"type":"workspace_snapshot_begin","protocol_version":4,"session_id":"opaque-daemon-session","revision":42,"focused_monitor_device_name":"\\\\.\\DISPLAY2"}
{"type":"workspace_snapshot_chunk","revision":42,"records":[{"kind":"monitor","monitor_device_name":"\\\\.\\DISPLAY2","monitor_id":65537,"active_workspace_index":1},{"kind":"workspace","monitor_device_name":"\\\\.\\DISPLAY2","workspace_index":1,"name":"code"},{"kind":"window","monitor_device_name":"\\\\.\\DISPLAY2","workspace_index":1,"hwnd":123456,"is_floating":true,"is_sticky":false}]}
{"type":"workspace_snapshot_end","revision":42}
```

The example abbreviates the records. A real snapshot includes:

- One `monitor` record per connected display. `monitor_device_name` is the exact
  Windows display device name accepted by targeted commands. `monitor_id` is its
  transient Win32 HMONITOR value, retained for correlation with older events.
- Nine `workspace` records per monitor, including empty, lazily unallocated slots.
  Workspace indices are **zero-based**. Names use the existing global
  `[workspaces].names` configuration; unnamed slots contain `null`.
- One `window` record per managed window in its owning workspace, including
  inactive workspaces, floating windows, minimized windows, and inactive tabs.
  Sticky windows report their actual current ownership with `is_sticky: true`;
  consumers must not count them once on every workspace. Hidden scratchpads and
  drag placeholders are excluded; shown scratchpads are ordinary floating members.
  A dragged window temporarily detached for a preview retains source ownership
  until the drop commits its new workspace.
- Deterministic order: monitors by device name, workspaces by index, and windows
  within each workspace by HWND. HWNDs are transient and can be reused; consumers
  must invalidate cached metadata appropriately. Icons, titles, geometry and
  executable lookup are consumer concerns and are absent from this state model.

A snapshot starts with `workspace_snapshot_begin`, contains zero or more
`workspace_snapshot_chunk` frames, and ends with `workspace_snapshot_end`. Every
chunk/end has the begin's revision. Each UTF-8 JSON frame, **including its newline**,
is at most 64 KiB; there is no single-frame limit on the whole snapshot. The daemon
preflights the entire transaction before emitting its begin. An unencodable record
produces `{"type":"workspace_snapshot_error","message":"..."}` and closes the pipe.

Consumers must accumulate a replacement model and install it **only after the
matching end**. A new session invalidates all previous revision assumptions.
Revisions start at zero and advance only when membership, names, monitor topology,
active workspace or focused monitor changes. Geometry/animation and focus changes
within the same monitor do not advance this revision. These are complete
replacements, so a consumer does not need intervening revisions to reconstruct state.

Initial capture and receiver creation happen under the same AppState lock. Live
transactions are broadcast contiguously under that lock; heartbeat and legacy
frames do not interleave a transaction. Pipe writes happen outside the lock and
have a ten-second deadline. Each broadcaster retains 256 **frames**, not snapshots.
Workspace-enabled subscriptions (including mixed filters) use a separate broadcaster
that also carries legacy events. Workspace snapshots never consume legacy-only
subscribers' buffer capacity. Ordinary event processing projects workspace state
only while a workspace-enabled receiver exists; query/subscribe capture always
refreshes it under the lock.
For workspace subscribers, any overflow emits `lagged` and closes the pipe. Discard
partial state and reconnect for a fresh initial snapshot; also discard partial
state on EOF, error, timeout or an unexpected transaction boundary. Initial snapshots
are written directly and can exceed the live broadcast capacity. Very large live
transactions can therefore require reconnecting to obtain a complete initial view.
Legacy-only streams retain their prior lag behavior.

### Targeted switching and compatibility

The wire command is
`{"type":"switch_workspace_on_monitor","monitor_device_name":"\\\\.\\DISPLAY2","index":2}`.
Unlike snapshot indices, the command index is **one-based (1–9)**, matching existing
`switch_workspace`. The daemon validates both fields before changing state. A
successful command selects that monitor/workspace and restores eligible window
focus, including when the workspace is already active. Empty destinations select
the monitor/workspace without inventing a window to focus. Supplying `--monitor`
therefore transfers global monitor focus to that target; omitting it preserves the
existing focused-monitor behavior. Other monitors retain
their active workspace indices. Device names are topology identifiers, not durable
hardware serial numbers; use the latest snapshot after display reconfiguration.

The legacy `query_workspace`, `switch_workspace`, and default subscription wire
contracts remain unchanged. Old daemons reject the new filter/commands; clients
must report unsupported capability rather than silently use incomplete legacy data.
Workspace IPC is protocol **v4**; v3 remains the hotkey-query addition. Do not infer
workspace-state support from the numeric version alone; require the acknowledged
`workspace_state` filter (or successful one-shot handshake).

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

    // Empty preserves legacy kinds. Include WorkspaceState explicitly for snapshots.
    let cmd = IpcCommand::Subscribe { events: BTreeSet::new() };
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

# Empty events preserve legacy kinds; include "workspace_state" for snapshots.
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

The workspace-state extension centralizes its broadcast gate in
`AppState::publish_workspace_state_if_changed` after each completed main-loop
event, including OS window events, hotkeys/commands, settings, drag finalization
and topology changes. Subscription/query capture also synchronizes this gate
before attaching the receiver. Future mutation paths that bypass the main event
loop must explicitly invoke the gate under the same state lock.

Check `MEMORY.md` → "IPC bar-integration validation deferred" before building reference consumers. Real bar work is deferred until first user demand; bug-fix work uncovered by inspection still lands.

## Limitations

- **Per-monitor filter not supported** — `--events` filters by kind only. Filter legacy events by `monitor`, or workspace-state records by `monitor_device_name`, client-side.
- **No individual `WindowCreated` / `Destroyed` events** — opt-in workspace-state snapshots report complete membership after lifecycle changes. Consumers compare replacements if they need their own deltas.
- **No WebSocket bridge** — the daemon serves only the named pipe. Browser-based bars need a thin bridge component.
- **Stream mode is uni-directional** — after Subscribe, the pipe only flows daemon→client. Open a second pipe for command queries.
- **Daemon shutdown** delivers EOF (`BrokenPipe` on next read). Reconnect with backoff if your bar should survive daemon restarts.

## Validation history

- **2026-05-18**: Manual walk against v0.1.16 daemon confirmed `WorkspaceChanged`, `LayoutChanged`, `ConfigReloaded`, `Heartbeat`, snapshot delivery, and filter behavior all match docs. Two issues surfaced and were both fixed in v0.1.17: (1) `LayoutChanged.columns[].mode` was emitted but undocumented (docs updated); (2) `FocusedWindowChanged` did not fire for command-initiated focus changes (`lwm focus left/right`, workspace switches) because `sync_foreground_window` was pre-updating the OS-side dedup baseline before Windows could fire `EVENT_SYSTEM_FOREGROUND`. Daemon now tracks `last_broadcast_focused_hwnd` independently of `previous_focused_hwnd`, and `sync_foreground_window` broadcasts through the same dedup helper as the OS event handler. All focus changes — OS-driven, command-driven, recovery-path — now emit through one gate.
