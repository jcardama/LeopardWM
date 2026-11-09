# Yasb workspace buttons

## Stock GlazeWM workspaces widget

[Yasb](https://github.com/amnweb/yasb) can use its stock GlazeWM workspaces widget with LeopardWM's opt-in compatibility endpoint. Enable **GlazeWM compatibility** in Settings, or add this to LeopardWM's config and run `lwm reload`:

```toml
[glazewm_compat]
enabled = true
port = 6123
```

Add the widget to Yasb's existing `widgets` mapping and add `leopard_workspaces` to the appropriate bar's widget list:

```yaml
widgets:
  leopard_workspaces:
    type: "glazewm.workspaces.GlazewmWorkspacesWidget"
    options: {}
```

The default Yasb URI is `ws://localhost:6123`. If you changed LeopardWM's port, set `glazewm_server_uri` in the widget's options to match, for example `ws://localhost:7123`. The endpoint binds IPv4 loopback only, accepts connections only from the daemon's Windows user, rejects handshakes carrying an `Origin` header, and supports workspace focus commands only. The named-pipe IPC remains unchanged.

Only populated workspaces and the displayed workspace appear. Their internal names combine the screen-position monitor ordinal and workspace number, such as `1-3`; display names use `[workspaces].names` or the workspace number. The global wheel commands act on the focused monitor rather than crossing monitors. Other GlazeWM widgets and commands are not supported.

LeopardWM and GlazeWM cannot share port 6123. If GlazeWM already holds it, LeopardWM logs a warning and continues without the endpoint. If LeopardWM holds it, GlazeWM fails to start. Disable the endpoint or choose another port to run both.

Style the widget in Yasb's `styles.css`, not with HTML. The widget exposes `.glazewm-workspaces`, `.ws-btn` and its `empty`, `populated`, `active_empty`, `active_populated`, `focused_empty`, and `focused_populated` states, plus `.label`, `.icon`, `.icon-N`, and `.offline-status`. Yasb supports `{name}` and `{display_name}` label templates, `app_icons`, `offline_label`, and `hide_if_offline`.

## Any bar

Bars such as Zebar or Rainmeter can use the CLI if they can run a process and consume its output:

```powershell
lwm subscribe --events workspace_state
lwm query workspaces
lwm workspace 3 --monitor '\\.\DISPLAY2'
```

The subscription streams newline-delimited JSON until disconnected; the query prints a one-shot snapshot in the same frame format. Use separate processes for the stream, query, and commands. A workspace snapshot is a begin frame, zero or more chunk frames, and a matching end frame. Accumulate records and replace displayed state only after the matching end. On a `lagged` event, EOF, or an incomplete transaction, discard partial state and reconnect for a fresh snapshot. Snapshot workspace indices are zero-based; the workspace command takes 1–9 and transfers focus to the selected monitor. Obtain the exact `monitor_device_name` from a current snapshot because display names can change.

Other filters include `workspace`, `focused_window`, `layout`, `config`, and `heartbeat`. The default subscription excludes `workspace_state`. See the [IPC event contract](../agent_docs/ipc-events.md) for schemas and recovery details and the [CLI reference](cli.md) for commands. A bar must support subprocess output and JSON parsing; this does not provide a bar-specific adapter.

## Alternative: Custom widget and CLI

The following polling recipe uses Yasb's built-in Custom widget to show nine buttons for **one explicitly selected monitor**. Use a matching LeopardWM CLI and daemon with `lwm query workspaces` support (introduced in 0.2.10), and [put `lwm.exe` on the PATH inherited by Yasb](../README.md#put-lwm-on-the-path). Restart Yasb after a PATH change. This alternative does not require the compatibility endpoint.

## 1. Save the query adapter

Create `%APPDATA%\yasb\leopardwm-workspaces.ps1` with the following content. It converts the CLI's newline-delimited snapshot into the single JSON value Yasb expects. Only a successfully completed query is displayed as workspace state. If the adapter runs but the daemon is unavailable, the query is unsupported, or the monitor is missing, it returns nine `?` labels with a "LeopardWM unavailable" tooltip instead of stale workspace state. This deliberately keeps the buttons visible; it does not rely on Yasb hiding empty output.

```powershell
param(
    [Parameter(Mandatory = $true)]
    [string]$Monitor
)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)

try {
    $lines = @(& lwm query workspaces 2>$null)
    if ($LASTEXITCODE -ne 0) { throw 'lwm query workspaces failed' }
    $frames = @($lines | ForEach-Object { $_ | ConvertFrom-Json })
    if ($frames.Count -lt 2 -or
        $frames[0].type -ne 'workspace_snapshot_begin' -or
        $frames[-1].type -ne 'workspace_snapshot_end' -or
        $frames[0].revision -ne $frames[-1].revision) {
        throw 'Incomplete workspace snapshot'
    }
    $records = @($frames | Where-Object {
        $_.type -eq 'workspace_snapshot_chunk'
    } | ForEach-Object { $_.records } | Where-Object {
        $_.monitor_device_name -eq $Monitor
    })
    $display = @($records | Where-Object { $_.kind -eq 'monitor' })
    $workspaces = @($records | Where-Object {
        $_.kind -eq 'workspace'
    } | Sort-Object workspace_index)
    if ($display.Count -ne 1 -or $workspaces.Count -ne 9) {
        throw 'Target monitor is absent or its workspace list is incomplete'
    }
    $items = @($workspaces | ForEach-Object {
        $index = $_.workspace_index
        $number = $index + 1
        $count = @($records | Where-Object {
            $_.kind -eq 'window' -and $_.workspace_index -eq $index
        }).Count
        $label = "$number"
        if ($count -gt 0) { $label += '*' }
        if ($index -eq $display[0].active_workspace_index) {
            $label = "[$label]"
        }
        $name = if ($_.name) { $_.name } else { "Workspace $number" }
        [pscustomobject]@{
            label = $label
            tooltip = "$name - $count managed windows on $Monitor"
        }
    })
    ConvertTo-Json -InputObject $items -Compress
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    $unavailable = @(1..9 | ForEach-Object {
        [pscustomobject]@{
            label = '?'
            tooltip = "LeopardWM unavailable on $Monitor"
        }
    })
    ConvertTo-Json -InputObject $unavailable -Compress
    exit 1
}
```

The CLI checks the snapshot transaction and exits unsuccessfully on incomplete, mismatched, or error frames. The adapter checks completion before rendering, then derives occupancy from `window` records; it does not treat `query workspaces` as one JSON object or parse a partial snapshot. Minimized windows, inactive tabs, and floating windows count as occupied. Sticky windows count only in their reported owning workspace; hidden scratchpads do not count.

## 2. Add the widgets to `config.yaml`

Merge these entries into your existing top-level `widgets:` mapping. The YAML anchors share the common Custom widget settings; each button has its own label, tooltip, and click command.

This example targets `\\.\DISPLAY1`. To find the current device names, inspect the `monitor` records from `lwm query workspaces` in a terminal. Replace `\\.\DISPLAY1` in the shared `run_cmd` **and all nine click commands** with your target's exact `monitor_device_name`. Display numbers are not persistent hardware identities; recheck them after changing display topology.

```yaml
widgets:
  lwm_1: &lwm_widget
    type: "yasb.custom.CustomWidget"
    options: &lwm_options
      label: "{data[0][label]}"
      label_placeholder: "..."
      class_name: "leopardwm-workspaces"
      tooltip: true
      tooltip_label: "{data[0][tooltip]}"
      exec_options:
        run_cmd: 'powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command & $env:APPDATA/yasb/leopardwm-workspaces.ps1 -Monitor \\.\DISPLAY1'
        run_interval: 5000
        run_once: false
        return_format: "json"
        hide_empty: false
        use_shell: false
        encoding: "utf-8"
      callbacks:
        on_left: 'exec lwm workspace 1 --monitor \\.\DISPLAY1'
        on_middle: "do_nothing"
        on_right: "do_nothing"
  lwm_2:
    <<: *lwm_widget
    options:
      <<: *lwm_options
      label: "{data[1][label]}"
      tooltip_label: "{data[1][tooltip]}"
      callbacks: {on_left: 'exec lwm workspace 2 --monitor \\.\DISPLAY1', on_middle: "do_nothing", on_right: "do_nothing"}
  lwm_3:
    <<: *lwm_widget
    options:
      <<: *lwm_options
      label: "{data[2][label]}"
      tooltip_label: "{data[2][tooltip]}"
      callbacks: {on_left: 'exec lwm workspace 3 --monitor \\.\DISPLAY1', on_middle: "do_nothing", on_right: "do_nothing"}
  lwm_4:
    <<: *lwm_widget
    options:
      <<: *lwm_options
      label: "{data[3][label]}"
      tooltip_label: "{data[3][tooltip]}"
      callbacks: {on_left: 'exec lwm workspace 4 --monitor \\.\DISPLAY1', on_middle: "do_nothing", on_right: "do_nothing"}
  lwm_5:
    <<: *lwm_widget
    options:
      <<: *lwm_options
      label: "{data[4][label]}"
      tooltip_label: "{data[4][tooltip]}"
      callbacks: {on_left: 'exec lwm workspace 5 --monitor \\.\DISPLAY1', on_middle: "do_nothing", on_right: "do_nothing"}
  lwm_6:
    <<: *lwm_widget
    options:
      <<: *lwm_options
      label: "{data[5][label]}"
      tooltip_label: "{data[5][tooltip]}"
      callbacks: {on_left: 'exec lwm workspace 6 --monitor \\.\DISPLAY1', on_middle: "do_nothing", on_right: "do_nothing"}
  lwm_7:
    <<: *lwm_widget
    options:
      <<: *lwm_options
      label: "{data[6][label]}"
      tooltip_label: "{data[6][tooltip]}"
      callbacks: {on_left: 'exec lwm workspace 7 --monitor \\.\DISPLAY1', on_middle: "do_nothing", on_right: "do_nothing"}
  lwm_8:
    <<: *lwm_widget
    options:
      <<: *lwm_options
      label: "{data[7][label]}"
      tooltip_label: "{data[7][tooltip]}"
      callbacks: {on_left: 'exec lwm workspace 8 --monitor \\.\DISPLAY1', on_middle: "do_nothing", on_right: "do_nothing"}
  lwm_9:
    <<: *lwm_widget
    options:
      <<: *lwm_options
      label: "{data[8][label]}"
      tooltip_label: "{data[8][tooltip]}"
      callbacks: {on_left: 'exec lwm workspace 9 --monitor \\.\DISPLAY1', on_middle: "do_nothing", on_right: "do_nothing"}
```

In the selected bar's existing `widgets.left`, `widgets.center`, or `widgets.right` list, add the names in order. For example, the relevant part of an existing bar becomes:

```yaml
bars:
  status-bar:
    # Keep this bar's existing screen, alignment, size, and other settings.
    widgets:
      left: ["lwm_1", "lwm_2", "lwm_3", "lwm_4", "lwm_5", "lwm_6", "lwm_7", "lwm_8", "lwm_9"]
      center: []
      right: []
```

Keep your other widget names in those lists. Do not add a second top-level `widgets:` or `bars:` key to an existing config.

## 3. Add styling

Append to your Yasb `styles.css`:

```css
.leopardwm-workspaces .widget-container {
    padding: 0 2px;
}
.leopardwm-workspaces .label {
    padding: 0 5px;
    color: #cdd6f4;
    font-weight: bold;
}
.leopardwm-workspaces .label:hover {
    color: #89b4fa;
}
```

A strip might read `[1*] 2 3* 4 5 6 7 8 9`: brackets indicate the active workspace **on the selected monitor**, and `*` indicates at least one managed window. Hover shows the configured workspace name (or its number) and window count. Clicking a button selects that workspace on the explicit monitor, even if another monitor currently has focus. Command numbers are one-based; snapshot indices are zero-based. The click deliberately transfers focus to the target monitor.

## If Yasb cannot find lwm

If changing the PATH is not an option, use the full path to `lwm.exe` in both the adapter and callbacks. Find it with `where.exe lwm` in a terminal where `lwm` works. A default MSI install uses `C:\Program Files\LeopardWM\bin\lwm.exe`; for a zip, use `lwm.exe` in the extracted folder.

Replace the adapter's query line with this, using your full path:

```powershell
$lines = @(& 'C:\Program Files\LeopardWM\bin\lwm.exe' query workspaces 2>$null)
```

Use the same path in all nine callbacks, keeping each workspace number and your target monitor. For example:

```yaml
on_left: 'exec "C:\Program Files\LeopardWM\bin\lwm.exe" workspace 1 --monitor \\.\DISPLAY1'
```

The double quotes around a path containing spaces are required. Backslashes are literal in YAML single-quoted strings.

## Trade-offs and troubleshooting

- This is a **polling recipe**, not a native Yasb integration. Each of the nine Custom widgets independently queries every five seconds (nine queries per interval per bar). Labels can lag a click or other change by up to an interval during normal operation, and buttons can briefly reflect different snapshots. Increase `run_interval` on the shared options to reduce polling. A first-class Yasb widget consuming `lwm subscribe --events workspace_state` would avoid repeated queries and update the strip atomically.
- Custom widgets have one callback set per widget and cannot turn a returned JSON array into independently clickable buttons. Nine static widgets provide direct switching without a plugin. They do not provide GlazeWM/komorebi's dynamic active/occupied CSS classes or application icons; the text markers provide that state instead.
- **Do not use `lwm subscribe` as `run_cmd`.** Yasb's Custom worker reads stdout to EOF and then parses one JSON value. A subscription never finishes normally, and `query workspaces` emits multiple JSON frames, so both need an appropriate adapter. `run_interval: 0` does not enable streaming.
- The command uses PowerShell's `-Command & $env:APPDATA/...` with `use_shell: false`, not a quoted `-File` path: Yasb currently splits `run_cmd` on spaces before starting the process. This keeps the script path usable even when APPDATA contains spaces. `-ExecutionPolicy Bypass` applies only to this PowerShell process; it does not change the persisted execution policy. Organizational policy may still block scripts.
- On caught failures the adapter prints a valid nine-element JSON array of `?` labels and "LeopardWM unavailable" tooltips, writes a diagnostic to stderr, and exits 1. Yasb's [`CustomWorker.run`](https://github.com/amnweb/yasb/blob/main/src/core/widgets/yasb/custom.py) parses stdout with `json.loads` and emits the result without checking the process exit code; stderr is discarded. `_handle_exec_data` replaces the previous data, then `_update_label` formats the labels. Empty stdout becomes `None`, and JSON `null` also becomes `None`; with these indexed templates, formatting fails before the `hide_empty` check and the exception handler displays the literal template instead of hiding the button. The adapter therefore supplies renderable failure data, with `hide_empty: false`. If PowerShell or the script cannot start at all, or no worker result arrives, this fallback cannot run: Yasb may retain its startup placeholder or previous label, so those labels are not a liveness guarantee. Run the adapter manually to see its error: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$env:APPDATA\yasb\leopardwm-workspaces.ps1" -Monitor '\\.\DISPLAY1'`. Check that Yasb inherited the correct PATH and that the CLI and daemon support this query. Command/query timeouts can delay updates beyond the normal interval; Custom has no subprocess timeout of its own.
- For multiple monitors, use separate widget names and a separate bar assignment per target, changing the monitor in both query and click commands. Do not reuse this DISPLAY1 strip on every monitor expecting Yasb to retarget it automatically. Reusing it still controls DISPLAY1.

The adapter and YAML command arguments have been checked offline with synthetic CLI snapshots, including missing/offline state and a script path containing spaces. The click commands have been checked against a live Yasb bar with LeopardWM 0.3.4, including switching to an empty workspace. With an older `lwm` on the PATH, clicking an empty workspace's button can flash it and return to the previous workspace.

## Upstream references

Checked against Yasb's current documentation and `main` source:

- [Custom widget configuration](https://github.com/amnweb/yasb/wiki/%28Widget%29-Custom): JSON label templates, `exec_options`, and command callbacks.
- [Custom widget implementation](https://github.com/amnweb/yasb/blob/main/src/core/widgets/yasb/custom.py) and [validation schema](https://github.com/amnweb/yasb/blob/main/src/core/validation/widgets/yasb/custom.py): stdout-to-EOF reading, JSON parsing, command splitting, and supported configuration fields.
- [Bar configuration](https://github.com/amnweb/yasb/wiki/Configuration) and [base widget](https://github.com/amnweb/yasb/blob/main/src/core/widgets/base.py): widget lists, callback parsing, and CSS selectors.
- [GlazeWM workspace widget](https://github.com/amnweb/yasb/blob/main/src/core/widgets/glazewm/workspaces.py) and [client](https://github.com/amnweb/yasb/blob/main/src/core/widgets/services/glazewm/client.py): WebSocket queries/subscriptions and workspace focus commands.
- [Komorebi workspace widget](https://github.com/amnweb/yasb/blob/main/src/core/widgets/komorebi/workspaces.py), [event listener](https://github.com/amnweb/yasb/blob/main/src/core/widgets/services/komorebi/event_listener.py), and [client](https://github.com/amnweb/yasb/blob/main/src/core/widgets/services/komorebi/client.py): named-pipe state notifications, `komorebic state`, and targeted switching.

For LeopardWM's snapshot fields, compatibility, and ownership rules, see the [IPC workspace-state contract](../agent_docs/ipc-events.md#complete-workspace-state). The polling adapter above supersedes that document's older Custom-widget subscription sketch, which is incompatible with Yasb's stdout-to-EOF worker.
