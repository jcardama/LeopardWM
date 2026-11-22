# CLI command reference

`leopardwm-cli` is the canonical binary name; `lwm` is the short alias for the same CLI. Use matching CLI and daemon releases. Run `lwm --help` or `lwm <command> --help` for built-in help; `lwm --version` prints the version. Commands use kebab-case; the action names in `[hotkeys]` use snake_case and can differ. See the [README hotkey reference](../README.md#default-hotkeys).

Arguments in the tables are required unless marked optional. Commands that take subcommands are listed along with each full path.

## Record a problem

| Command | Arguments and effect |
|---|---|
| `lwm record` | Choose a recording action. |
| `lwm record start` | Capture debug logs for ten minutes, or for optional `--timeout <SECS>` clamped to 1–3600 seconds. |
| `lwm record stop` | Stop the active recording; stopping while idle is harmless. |
| `lwm record status` | Show whether recording is active and the most recent path. |

The file `%LOCALAPPDATA%\leopardwm\recordings\leopardwm-recording.log` is overwritten on each start and is limited to 20 MiB. A second start while recording is an error. Recording stops on timeout or daemon exit and does not change the normal log level. The capture is not redacted and may contain sensitive information; review it before sharing.

## Focus and scrolling

| Command | Arguments and effect |
|---|---|
| `lwm focus` | Choose a focus direction. |
| `lwm focus left` | Focus the column or floating window to the left. |
| `lwm focus right` | Focus the column or floating window to the right. |
| `lwm focus up` | Focus the window above. |
| `lwm focus down` | Focus the window below. |
| `lwm focus start` | Focus the first column or floating window. |
| `lwm focus end` | Focus the last column or floating window. |
| `lwm focus-floating` | Focus the remembered floating window, or the most recently added floating window if none is remembered. |
| `lwm focus-tiling` | Focus the tiled layer, remembering the floating window being left. |
| `lwm switch-focus-floating-tiling` | Switch focus between the floating and tiled layers. |
| `lwm nudge-left` | Move a floating window left, or shift tiled windows left, by `move_step`. |
| `lwm nudge-right` | Move a floating window right, or shift tiled windows right, by `move_step`. |
| `lwm focus-row` | `<TARGET>` selects `up`, `down`, or a row number from 1 to 4. |
| `lwm scroll` | Choose a viewport scroll direction. |
| `lwm scroll left` | Scroll left; optional `-p, --pixels <PIXELS>` defaults to 100. |
| `lwm scroll right` | Scroll right; optional `-p, --pixels <PIXELS>` defaults to 100. |

At a tiled horizontal strip edge, `[behavior] focus_crosses_monitors` and `move_crosses_monitors` can respectively focus the neighboring monitor or move the focused column there. Both default to false and are independent of vertical workspace edge behavior. `lwm move-to-monitor` and the `move_to_monitor_*` hotkeys still move one window.

Tiled vertical focus crosses rows at column edges regardless of `move_window_across_rows`. At a workspace's vertical edge, `[behavior] workspace_edge_wrap` controls `focus up`/`focus down` and `move-window up`/`move-window down`: `"off"` (default) stays on the workspace, `"wrap"` moves to an adjacent workspace and wraps between 1 and 9, and `"clamp"` stops at workspace 1 or 9. See the [README](../README.md#layout-commands) for skip-empty behavior and details.

## Move windows and columns

Move left/right and move-window directions nudge a focused floating window by `move_step`, constrained to its monitor's work area.

| Command | Arguments and effect |
|---|---|
| `lwm move` | Choose a direction for the focused column. |
| `lwm move left` | Move the focused column left, or nudge a floating window left. |
| `lwm move right` | Move the focused column right, or nudge a floating window right. |
| `lwm move start` | Move the focused column to the start of the strip. |
| `lwm move end` | Move the focused column to the end of the strip. |
| `lwm move-window` | Choose a direction for the focused window. |
| `lwm move-window left` | Move the focused window to the column on the left, or nudge it left if floating. |
| `lwm move-window right` | Move the focused window to the column on the right, or nudge it right if floating. |
| `lwm move-window up` | Move the focused window up in its column, or nudge it up if floating. |
| `lwm move-window down` | Move the focused window down in its column, or nudge it down if floating. |
| `lwm move-to-row` | Choose a direction for moving the focused window between rows. |
| `lwm move-to-row up` | Move the focused window to a new column after the focused column in the row above. |
| `lwm move-to-row down` | Move the focused window to a new column after the focused column in the row below. |
| `lwm expel` | Choose a side for a new column. |
| `lwm expel left` | Expel the focused window into a new column on the left. |
| `lwm expel right` | Expel the focused window into a new column on the right. |
| `lwm consume` | Choose an adjacent column to consume from. |
| `lwm consume left` | Pull a window from the left column into the focused column. |
| `lwm consume right` | Pull a window from the right column into the focused column. |

## Workspaces and monitors

| Command | Arguments and effect |
|---|---|
| `lwm workspace` | `<NUMBER>` selects workspace 1–9. Optional `--monitor <DEVICE_NAME>` targets that display and transfers focus to it; otherwise uses the focused monitor. |
| `lwm workspace-next` | Select the next workspace, wrapping from 9 to 1. |
| `lwm workspace-prev` | Select the previous workspace, wrapping from 1 to 9. |
| `lwm move-to-workspace` | `<NUMBER>` moves the focused window to workspace 1–9. |
| `lwm move-to-workspace-next` | Move the focused window to the next workspace, wrapping from 9 to 1. |
| `lwm move-to-workspace-prev` | Move the focused window to the previous workspace, wrapping from 1 to 9. |
| `lwm toggle-overview` | Toggle the overview of non-empty workspaces on the focused monitor. |
| `lwm focus-monitor` | Choose a monitor direction to focus. |
| `lwm focus-monitor left` | Focus the monitor to the left. |
| `lwm focus-monitor right` | Focus the monitor to the right. |
| `lwm focus-monitor up` | Focus the monitor above. |
| `lwm focus-monitor down` | Focus the monitor below. |
| `lwm move-to-monitor` | Choose a monitor direction for the focused window. |
| `lwm move-to-monitor left` | Move the focused window to the monitor on the left. |
| `lwm move-to-monitor right` | Move the focused window to the monitor on the right. |
| `lwm move-to-monitor up` | Move the focused window to the monitor above. |
| `lwm move-to-monitor down` | Move the focused window to the monitor below. |

Next/previous workspace commands always wrap; optional skip-empty behavior is described in the [README](../README.md#layout-commands). The `move-to-workspace` commands and the edge moves stay on the current workspace by default; with `[behavior] follow_window_to_workspace = true` they also switch to the destination and keep the moved window focused. Monitor names come from `lwm query workspaces` and can change with display topology.

## Layout, width and height

With a floating window focused, `set-width` and width/height preset commands size it as a fraction of its monitor's work area; `center-column` centers it in that area. Up/down presets stop at the ends, while the unqualified cycle commands wrap.

| Command | Arguments and effect |
|---|---|
| `lwm resize` | Required `-d, --delta <PIXELS>` changes focused column width in pixels. Use `--delta=-50` for a negative delta. |
| `lwm set-width` | Required `-f, --fraction <FRACTION>` sets column width as a finite fraction of the viewport, or floating width as a fraction of the work area, from 0.1 to 1.0 inclusive. Example: `lwm set-width --fraction 0.5`. |
| `lwm center-column` | Center the focused column, or center a floating window in the work area. |
| `lwm align-column-left` | Align the focused column or floating window to the left edge. |
| `lwm align-column-right` | Align the focused column or floating window to the right edge. |
| `lwm maximize-column` | Toggle the focused column filling the viewport width. |
| `lwm equalize-widths` | Equalize column widths. |
| `lwm cycle-width` | Cycle column or floating window width to the next preset, wrapping at the end. |
| `lwm cycle-width-up` | Cycle column or floating window width up through presets. |
| `lwm cycle-width-down` | Cycle column or floating window width down through presets. |
| `lwm cycle-height` | Cycle window height to the next preset, wrapping at the end, including floating windows. |
| `lwm cycle-height-up` | Cycle window height up through presets, including floating windows. |
| `lwm cycle-height-down` | Cycle window height down through presets, including floating windows. |
| `lwm equalize-heights` | Equalize window heights in the focused column. |

## Window modes

| Command | Effect |
|---|---|
| `lwm close-window` | Close the focused window. |
| `lwm toggle-floating` | Toggle floating for the focused window. |
| `lwm float` | Float the focused window if tiled; otherwise do nothing. |
| `lwm tile` | Tile the focused window if floating; otherwise do nothing. |
| `lwm toggle-fullscreen` | Toggle fullscreen for the focused window. |
| `lwm toggle-sticky` | Pin or unpin the focused window across workspaces. |
| `lwm scratchpad-stash` | Designate and hide the focused window as the scratchpad. If it is already the scratchpad, release it to tiling; if a different scratchpad exists, release that window first. |
| `lwm scratchpad-toggle` | Show the hidden scratchpad centered and floating, or hide it. |
| `lwm toggle-tabbed` | Toggle tabbed mode for the focused column. |
| `lwm toggle-new-window-placement` | Toggle new windows between a new column and the focused column. |
| `lwm toggle-ignore` | Toggle session-only ignore for the actual OS foreground window. |

## Queries and events

| Command | Arguments and effect |
|---|---|
| `lwm query` | Choose a query. |
| `lwm query workspace` | Show the current focused workspace layout. |
| `lwm query workspaces` | Print one complete multi-monitor workspace snapshot as newline-delimited JSON frames. |
| `lwm query focused` | Show focused window information. |
| `lwm query all` | List all managed windows. |
| `lwm query hotkeys` | List effective hotkeys and configuration diagnostics. |
| `lwm status` | Show daemon status. |
| `lwm subscribe` | Stream newline-delimited JSON until disconnected. Optional `--events <KINDS>` is a comma-separated list: `workspace`, `focused_window`, `layout`, `config`, `heartbeat`, `workspace_state`. Without it, the legacy set excludes `workspace_state`. |

See the [bar guide](yasb.md#any-bar) and [event schemas](../agent_docs/ipc-events.md) for snapshot transactions, reconnection and event fields.

## Daemon lifecycle and recovery

| Command | Arguments and effect |
|---|---|
| `lwm run` | Start the daemon if needed and apply layout. `--no-apply` skips the apply; `--wait-ms <MS>` defaults to 5000; `--safe-mode` starts without hotkeys or cloaking; `--no-watchdog` starts directly without crash recovery or auto-restart. |
| `lwm stop` | Stop the daemon. |
| `lwm toggle-pause` | Toggle tiling pause/resume. Alias: `lwm pause`. |
| `lwm refresh` | Re-enumerate windows. |
| `lwm reload` | Reload configuration. |
| `lwm release-all-windows` | Pause tiling and cascade managed windows without removing membership. |
| `lwm ghost` | Manage swap-chain ghost animation. |
| `lwm ghost enable` | Enable ghost animation. |
| `lwm ghost disable` | Disable ghost animation. |
| `lwm ghost status` | Report ghost animation state. |
| `lwm panic-revert` | Ask the daemon to revert managed windows in an emergency. Alias: `lwm recover`. |
| `lwm emergency-uncloak` | Perform local emergency visibility restoration without daemon IPC. Alias: `lwm restore-windows`. |
| `lwm doctor` | Run diagnostic checks. |
| `lwm doctor windows` | Inspect top-level window admission. Optional `--watch <SECONDS>` and `--delay <SECONDS>` each accept 1–120; watch samples every 100 ms. `--include-titles` includes otherwise redacted titles; `--all` includes windows normally filtered out. |
| `lwm collect-logs` | Collect diagnostic logs for a bug report. |

## Configuration and autostart

| Command | Arguments and effect |
|---|---|
| `lwm config` | Choose a configuration operation. |
| `lwm config init` | Generate defaults at `%APPDATA%/leopardwm/config/config.toml`. Optional `-o, --output <PATH>` selects another path, `-f, --force` overwrites, and `-p, --profile <PROFILE>` selects `developer`, `laptop`, or `ultrawide`; without a profile uses defaults. |
| `lwm config reset` | Back up the current config to `config.toml.bak` if it exists, then write defaults. |
| `lwm config backup` | Back up the current config. |
| `lwm config restore` | Restore the backup. |
| `lwm autostart` | Choose a login autostart operation. |
| `lwm autostart enable` | Enable autostart at login. |
| `lwm autostart disable` | Disable autostart at login. |
| `lwm export-shortcut-guide` | Export effective hotkeys as a PowerToys manifest to stdout. Optional `-o, --output <PATH>` writes a file; `--install` installs it. These options cannot be combined. |

## Driving lwm from scripts

Test commands in a terminal first: launching a hidden process can swallow errors. A daemon non-success response exits with code 1; argument and other failures also return nonzero. Check the process exit status instead of assuming a hidden command succeeded.

AutoHotkey v2 uses `RunWait` to wait for the hidden command and return its exit code. Backslashes are literal in these single-quoted strings; the embedded double quotes group the monitor argument:

```ahk
exitCode := RunWait('lwm focus left', , 'Hide')
if exitCode != 0
    MsgBox 'lwm failed with exit code ' exitCode

exitCode := RunWait('lwm workspace 2 --monitor "\\.\DISPLAY2"', , 'Hide')
if exitCode != 0
    MsgBox 'lwm failed with exit code ' exitCode
```

Plain `Run` is fine once a command is known to work, but it does not wait for the exit code and hiding the process hides its errors.

PowerShell also treats backslashes literally in quoted strings. For example:

```powershell
lwm workspace 2 --monitor '\\.\DISPLAY2'
if ($LASTEXITCODE -ne 0) { Write-Error 'lwm failed' }
```

Use the exact current monitor device name from the snapshot, not an assumed display number. These examples change focus or workspace when run.
