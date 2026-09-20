<p align="center">
  <img src="assets/leopardwm.png" alt="LeopardWM" width="128" />
</p>

# LeopardWM

[![CI](https://github.com/jcardama/LeopardWM/actions/workflows/ci.yml/badge.svg)](https://github.com/jcardama/LeopardWM/actions/workflows/ci.yml)
[![License: GPL-3.0](https://img.shields.io/badge/License-GPL--3.0-blue.svg)](LICENSE)
![Platform: Windows 10/11](https://img.shields.io/badge/Platform-Windows%2010%2F11-0078D4)
[![Buy Me a Coffee](https://img.shields.io/badge/Buy%20Me%20a%20Coffee-ffdd00?logo=buy-me-a-coffee&logoColor=000)](https://buymeacoffee.com/jcardama)

A scrollable tiling window manager for Windows.

https://github.com/user-attachments/assets/d367d337-4005-4c1d-bdd5-8a678b34582f

## What Makes It Different

Most Windows tilers use tree or BSP layouts. LeopardWM is **scroll-first**: windows sit on a horizontal strip, and your monitor acts as a viewport that scrolls over them. Navigation stays spatially consistent as windows are added — you move through context instead of constantly rebuilding split trees.

- **Vsync-aligned animations** — smooth scrolling powered by a `DwmFlush`-driven animation engine
- **First-class touchpad gestures** — three-finger swipes drive focus and scroll out of the box
- **Disables Windows 11 Snap Layouts on managed windows** — no more accidental edge-snap when you drag a tile
- **Auto-detected per-window rounded corners and high-contrast/reduced-motion/battery awareness** — system integration that respects user settings
- **WebView2 settings GUI** with Mica backdrop and live theme switching — not just a config file
- **GPL-3.0** — commercial use without a paid license, written in safe Rust

## In Action

**Overview** — zoom out to a map of your non-empty workspaces and jump anywhere

https://github.com/user-attachments/assets/4de8a4f4-1bd9-4a69-bed8-4f6ba3dba0ca

**Workspaces** — per-monitor workspaces; switch between them and move windows across

https://github.com/user-attachments/assets/0c06ac6b-2527-467c-a369-b41ea48c040b

**Tabbed columns** — collapse a column into a tab strip, only the active tab fills the rect

https://github.com/user-attachments/assets/73f6a133-e038-41c5-8b33-79bd67c6c267

**Scratchpad** — stash a window out of the layout and summon it back as a floating overlay

https://github.com/user-attachments/assets/30595d72-0fad-4db8-903c-52307087c00e

**Sticky windows** — pin a window so it follows you across workspaces, tiled or floating

https://github.com/user-attachments/assets/43715787-1501-4e19-b693-f301065e914d

## Design Philosophy

A few deliberate **non-features**, so you know what you're getting:

- **Scroll-first, not multi-layout.** No BSP, no DWindle, no Equal/Stair/UltrawideVerticalStack — and we won't add them. niri (Wayland) and PaperWM (GNOME) stay scrolling-only by choice; the horizontal strip *is* the identity. If you want 9 layout variants, [komorebi](https://github.com/LGUG2Z/komorebi) is the right tool.
- **No Virtual Desktop bridging.** Per-monitor workspaces don't map cleanly to Windows' global Virtual Desktops, and the only library that bridges them (`winvd`) breaks every 3-6 months on Windows feature updates. Instead, `Win+Ctrl+Arrow` is intercepted and routed to LeopardWM's workspace prev/next so the native muscle memory still works.
- **Named-pipe IPC, not WebSocket.** Lower latency, no port allocation, no firewall prompts. If browser-based bar integration becomes a real ask, we'll add a thin bridge rather than make the daemon serve sockets directly.

## Features

- Multi-monitor workspaces with monitor-aware focus and move (9 workspaces per monitor)
- Global hotkeys with live config reload
- **PowerToys Shortcut Guide export** — `lwm query hotkeys` lists effective bindings; `lwm export-shortcut-guide` writes a user manifest (stdout, `--output PATH`, or `--install`)
- Smooth scroll animations with layout transition effects (vsync-locked)
- Touchpad gestures with configurable swipe actions
- Drag-and-drop column reorder (Shift+drag to merge windows)
- **Tabbed columns** — toggle a column between vertical-stack and tab-strip mode (`Ctrl+Alt+T`); only the active tab fills the column rect, the rest sit in a clickable strip above
- **Scratchpad** — stash the focused window out of the layout (`Ctrl+Alt+Shift+S`) and summon it back as a floating, centered overlay on demand (`Ctrl+Alt+S`); stash it again to release it back to tiling
- **Sticky windows** — pin a window (`Ctrl+Alt+Y`) so it follows you across workspaces, keeping its current mode: a tiled window stays tiled (a column you can cycle to), a floating window stays a floating overlay
- **Overview mode** — `Ctrl+Alt+Space` opens a map of the monitor's non-empty workspaces; click a window card to jump to it, click a row to switch workspace, or drive it with arrows/Enter/digits
- **Per-app window rules** — float, ignore, or tile by class/title/executable, plus per-app open behavior: target workspace, initial column width, open maximized
- Floating and fullscreen toggles
- Width and height presets with column equalization, maximize-column, center-column
- Active focus border with auto-detected rounded corners
- System tray with pause, reload, settings, and diagnostics
- WebView-based settings GUI (Mica backdrop, live theme switching, dark mode)
- Safe mode for troubleshooting (`--safe-mode`)
- Built-in diagnostics (`lwm doctor`)
- Workspace persistence and session recovery
- Autostart via Registry, configurable from CLI / Settings / tray
- In-app update notifier — daily check against GitHub Releases, opt-out
- Windows 11 Snap Layouts disabled for managed tiled windows
- Battery-aware: animations auto-disable on battery / power saver
- Respects Windows reduced-motion and high-contrast settings
- DPI-aware gap and border scaling per-monitor

## Installation

### Via package manager (recommended)

```powershell
winget install jcardama.LeopardWM         # Windows Package Manager

scoop bucket add extras                   # Scoop (first time only)
scoop install extras/leopardwm
```

This installs LeopardWM and puts `leopardwm`, `leopardwm-cli`, and `lwm` on your PATH. Use `winget upgrade jcardama.LeopardWM` or `scoop update leopardwm` to install the latest release.

### Via MSI installer

Download `LeopardWM-x.y.z-x86_64.msi` from [GitHub Releases](https://github.com/jcardama/LeopardWM/releases) and run it. Re-running a newer MSI upgrades in place — no manual uninstall needed.

### Via standalone zip

For users who prefer not to install:

1. Download `LeopardWM-x.y.z-x86_64-windows.zip` from [GitHub Releases](https://github.com/jcardama/LeopardWM/releases)
2. Extract to a permanent location
3. Run `leopardwm.exe`
4. (Optional) Enable autostart: `lwm autostart enable`

Current releases are not code-signed, so Windows SmartScreen may show a warning on first install.

## Quick Start (from source)

Prerequisites: [Rust](https://rustup.rs) with the MSVC toolchain (`stable-x86_64-pc-windows-msvc`)

```bash
git clone https://github.com/jcardama/LeopardWM.git
cd LeopardWM
cargo build --release
```

Start the daemon:

```bash
./target/release/leopardwm.exe
```

A default config is created automatically at `%APPDATA%\leopardwm\config\config.toml`. Customize via the tray icon → Settings, or edit the file directly.

## Default Hotkeys

Most hotkeys use `Ctrl+Alt` as the base modifier. Layered pattern: base = focus, +Shift = move, +Win = monitor scope. Every hotkey is rebindable in `config.toml`. Combos Windows reserves (like `Win+Ctrl+Arrow`) can't be bound directly, but the opt-in **Reclaim Windows-reserved shortcuts** setting lets you use them anyway.

| Key | Action |
|---|---|
| `Ctrl+Alt+H/L/J/K` | Focus left / right / down / up |
| `Ctrl+Alt+Home` / `End` | Focus start / end of strip |
| `Ctrl+Alt+Shift+H/L` | Move column left / right |
| `Ctrl+Alt+Shift+Home` / `End` | Move column to start / end of strip |
| `Ctrl+Alt+Shift+J/K` | Move window down / up in column |
| `Ctrl+Alt+[` / `]` | Move window to left / right column |
| `Ctrl+Alt+Shift+[` / `]` | Expel window to new column left / right |
| `Ctrl+Alt+,` / `.` | Consume left / right column's window into the focused column |
| `Ctrl+Alt+Minus` / `Ctrl+Alt+Equals` | Cycle column width down / up |
| `Ctrl+Alt+Shift+Minus` / `Ctrl+Alt+Shift+Equals` | Cycle window height down / up |
| `Ctrl+Alt+0` | Equalize all column widths |
| `Ctrl+Alt+Shift+0` | Equalize window heights in column |
| `Ctrl+Alt+M` | Maximize focused column to viewport width |
| `Ctrl+Alt+C` | Center focused column in viewport |
| `Ctrl+Alt+Win+,`/`.` | Focus monitor left / right |
| `Ctrl+Alt+Win+Shift+,`/`.` | Move window to monitor |
| `Ctrl+Alt+1`...`9` | Switch to workspace 1–9 |
| `Ctrl+Alt+Shift+1`...`9` | Move focused window to workspace 1–9 |
| `Ctrl+Alt+Space` | Toggle workspace overview |
| `Ctrl+Alt+Shift+Left` / `Right` | Workspace prev / next (cycles) |
| `Ctrl+Alt+W` | Close focused window |
| `Ctrl+Alt+F` | Toggle floating |
| `Ctrl+Alt+Shift+F` | Toggle fullscreen |
| `Ctrl+Alt+T` | Toggle tabbed mode on focused column |
| `Ctrl+Alt+S` | Toggle scratchpad (summon / hide) |
| `Ctrl+Alt+Shift+S` | Stash focused window to scratchpad (or release it back to tiling) |
| `Ctrl+Alt+Y` | Toggle sticky (follow across workspaces, keeping tiled/floating mode) |
| `Ctrl+Alt+P` | Toggle pause |
| `Ctrl+Alt+R` | Refresh (re-enumerate windows) |
| `Ctrl+Alt+Shift+R` | Reload config |
| `Win+Ctrl+Escape` | Emergency restore + panic-revert |

> The scratchpad and sticky pins are session-scoped: they are keyed by window handle and reset when the daemon restarts.

## Tabbed columns

Stack multiple windows into a clickable tab strip inside any column. Combine with the scrolling viewport for niri-style tabs that also pan horizontally — a combination no other Windows window manager ships today.

**Basics**
- `Ctrl+Alt+T` on the focused column toggles between vertical stacking (the default) and tabbed mode
- `Ctrl+Alt+J` / `Ctrl+Alt+K` cycle the active tab — same keys as intra-column focus, no new bindings to learn
- Click any tab in the strip to activate it; the click is a real focus change, so the border, foreground state, and IPC events all follow
- Tab titles and icons update live as windows rename themselves or swap notification badges

**Per-tab actions**
- Hover any tab to reveal a close-X at its right edge — click to close the tabbed window
- **Middle-click** does the same as the close-X
- **Right-click** any tab for a context menu: `Close window` / `Untab this window` / `Rename tab…`
- The implicit close gesture (X-button / middle-click) is configurable in Settings → Behavior → "Tab close action" — `close_window` (default, browser-style) or `untab` (rip the tab out into a new vertical column to the right)
- Right-click menu items always carry their literal action — `Close window` always closes regardless of the toggle, `Untab this window` always untabs
- "Rename tab…" opens a modal dialog seeded with the current tab title. Submitting saves a per-window override that survives untab, workspace moves, and daemon restart. Clearing the field removes the override and the live title returns

**Drag-and-drop (Chrome semantics)**
- Drop a window onto a tabbed column from anywhere — body or strip — and it appends as the rightmost tab and becomes active
- The drop-zone ghost spans the whole column rect so the target is unambiguous

**Lifecycle**
- A tabbed column with one window auto-reverts to vertical mode
- Tabbed state (and which tab is active) survives daemon restart, along with any per-tab title overrides
- Tab overrides for windows that no longer exist are pruned automatically at daemon startup
- The strip hides during fullscreen, pause, and on workspaces with no tabbed column

**Customization** — strip height, background, active/inactive text colours, active highlight, opacity, and the tab close action are configurable from the Settings UI or `[appearance]` / `[behavior]` (`tab_strip_height`, `tab_strip_bg`, `tab_strip_active_bg`, `tab_strip_active_text`, `tab_strip_inactive_text`, `tab_strip_opacity`, `tab_close_action`).

## CLI

LeopardWM ships two interchangeable CLI binaries — both invoke the same code:

| Binary | When to use |
|---|---|
| `leopardwm-cli` | Canonical name. Use in docs, scripts, and shared examples. |
| `lwm` | Short alias for daily typing. |

Examples below use whichever is shorter for the line.

### Daemon lifecycle

```bash
lwm run                # start the daemon (idempotent — no-op if already running)
lwm stop               # stop the daemon
lwm status             # show version, monitor count, window count, uptime
```

Matching versions (Recommended): use the CLI and daemon from the same release. Corrected `lwm run` pending-apply handling depends on both sides understanding `apply_pending`. A matching pair reports a still-pending recovery landing as a non-success without emergency visibility restore. An older CLI maps that status to unknown and may invoke emergency restore. Mixed versions are not negotiated or isolated on the pipe; the Apply request and workspace stream protocol version are unchanged.

### Query state

```bash
lwm query workspace    # current workspace placements as JSON
lwm query focused      # focused window info
lwm query all-windows  # every managed window across all workspaces
lwm query hotkeys      # effective bindings plus config diagnostics
```

### PowerToys Shortcut Guide

Export the daemon's effective hotkeys as a PowerToys Shortcut Guide user manifest:

```bash
lwm export-shortcut-guide                 # YAML to stdout
lwm export-shortcut-guide --output PATH   # write a file
lwm export-shortcut-guide --install       # replace the user manifest
```

`--output` and `--install` are mutually exclusive. `--install` writes
`%LOCALAPPDATA%\Microsoft\WinGet\KeyboardShortcuts\LeopardWM.LeopardWM.en-US.yml`.

Use matching CLI and daemon builds; query and export require IPC v3. The
manifest is a snapshot: after changing bindings, reload LeopardWM and export
again, then reopen Shortcut Guide. Query and export report resolved
configuration, not proof that the keyboard hook is installed or active; safe
mode can still list configured bindings.

Each alternative binding is a separate shortcut. Equivalent spellings of the
same physical chord resolve consistently: the lexicographically first valid
configured binding wins, and warnings identify ignored collisions. F13-F24 used
as modifiers cannot be represented by PowerToys and are skipped with a warning;
F13-F24 used as ordinary trigger keys can be exported. Warnings go to stderr so
stdout remains usable YAML.

### Layout commands

Most users drive the layout via hotkeys, but every hotkey has a CLI equivalent — useful for scripting or AutoHotkey integration.

```bash
lwm focus left | right | up | down
lwm move left | right                  # move focused column
lwm move-window up | down              # reorder within a column
lwm workspace 3                        # switch to workspace 3
lwm toggle-floating
lwm toggle-fullscreen
lwm scratchpad-stash                   # stash focused window (or release the scratchpad)
lwm scratchpad-toggle                  # summon / hide the scratchpad
lwm toggle-sticky                      # pin / unpin focused window on every workspace
lwm toggle-ignore                      # session-only ignore for the OS foreground window
```

`lwm toggle-ignore` targets the actual OS foreground window, not LeopardWM's cached
focus. Toggling out unmanages that window for this daemon session only; toggling
it back in re-admits it on the current monitor's active workspace. Persistent
Ignore rules still win. The action is in the hotkey catalog with no default
shortcut.

### Release all managed windows

```bash
lwm release-all-windows
```

This pauses tiling, clears LeopardWM's active decoration, globally attempts to
restore any top-level windows parked at LeopardWM's off-screen sentinel, and
cascades every tiled and floating managed window. Membership and admission stay
intact: use `lwm toggle-pause` to resume tiling. The command has no confirmation
prompt. If recovery fails or a live window cannot be restored, cascaded, or is
still maximized, release may be partial, tiling remains paused, and the CLI
reports the failure. Release first invalidates older animation work and waits up
to 100 ms for the animation worker. If that worker is busy, or a timed-out
placement worker is still recovering, no cascade is performed and tiling stays
paused. Retry `lwm release-all-windows` after the worker finishes; the failed
request never schedules a later cascade. Final cascade positions are ordered
after previously queued animation positions, and ordering failures are reported.

### Autostart (boot with Windows)

```bash
lwm autostart enable   # writes HKCU\Software\Microsoft\Windows\CurrentVersion\Run
lwm autostart disable  # removes it
```

This is also exposed as a Settings UI toggle and a tray menu item.

### Subscribe to events (status bars, custom integrations)

```bash
lwm subscribe                                       # legacy event kinds, newline-delimited JSON
lwm subscribe --events workspace,focused_window     # filtered subset
lwm subscribe --events workspace_state              # complete all-monitor workspace state
lwm query workspaces                                 # one complete workspace snapshot
lwm workspace 2 --monitor '\\.\DISPLAY2'            # select a workspace on that display
lwm subscribe | jq                                   # pretty-printed in another terminal
```

After the daemon answers `Subscribed`, the connection stays open and streams
`IpcEvent` frames as state changes occur. An empty filter preserves only the
legacy event set (`workspace_changed`, `focused_window_changed`, `layout_changed`,
`config_reloaded`, `heartbeat`); complete workspace snapshots require explicit
`--events workspace_state` opt-in and acknowledgement. Targeted workspace commands
use 1-based indices and transfer focus to the named monitor; snapshot indices are
0-based. Full schemas and sample clients are in `agent_docs/ipc-events.md`.

### Troubleshooting

```bash
lwm doctor             # diagnostic checks (config valid, daemon reachable, hotkey conflicts, etc.)
lwm collect-logs       # bundles logs + crash reports into a zip for bug reports
lwm reload             # reload config from disk without restarting
lwm refresh            # re-enumerate windows after weird state
lwm panic-revert       # emergency: uncloak everything, drop daemon out of management
```

Run `lwm help` (or `lwm <subcommand> --help`) for the full surface — there are ~40 subcommands.

### Touchpad gesture diagnostics

Failed physical gestures are diagnosed with an opt-in, short capture — not by leaving general logging at TRACE. Capture is **default off**; turning it on does not change gesture behavior.

1. In `%APPDATA%\leopardwm\config\config.toml`, set a short interval under `[gestures]`:

   ```toml
   diagnostic_capture_secs = 15
   ```

   The value is startup-only and clamped to 120 seconds. Settings saves preserve the knob; there is no live start command.

2. Restart the daemon with the documented workflow. **`lwm reload` is not enough.**

   ```bash
   lwm stop
   lwm run
   ```

   Stopping and starting can disturb off-screen client-area or DWM frame geometry. That is a known restart limitation, not a gesture-compatibility claim.

3. During the capture window:
   - Two-finger scroll **without** the configured navigation modifier (`hotkeys.scroll_modifier`, default `Ctrl+Alt`). Pass-through is expected (`stage=classifier outcome=reject`).
   - The same scroll **with** the modifier. Navigation should classify as a pass and may `recognized`/`dispatch`.
   - All four three-finger directions, with a pause between each.

4. Record the Windows touchpad setting as observed; do not treat it as a required mapping, and do not assume it guarantees wheel delivery to LeopardWM:
   - Windows 10: Settings → Devices → Touchpad
   - Windows 11: Settings → Bluetooth & devices → Touchpad

5. Use `lwm doctor` to inspect the daemon and CLI process-integrity lines. They do not measure the arbitrary foreground app; separately reported elevated-window blocks are different evidence. The capture header records `daemon_integrity` only. This report cannot prove elevated hook visibility, finger count, or device origin.

6. When the interval ends, the daemon writes a summary even with zero input. Inspect `%LOCALAPPDATA%\leopardwm\logs\leopardwm-gesture-capture.log`, or the **Gesture Capture** section of `lwm collect-logs` (full file, not the last-100 daemon tail). For a gesture bug report, share that artifact.

   Interpretation:

   | Stage | Meaning |
   |---|---|
   | `registration` | `disabled` / `failed` / `registered` / `stopped` — config and hook setup evidence, not a claim that a capture is running now |
   | `hook_delivery` | A wheel message reached the production hook (axis, delta, flags, modifier held, swipe candidate) |
   | `classifier` | `pass` entered navigation or swipe handling; `reject` is pass-through |
   | `accumulation` | Running total after this event |
   | `timeout` | Partial swipe accumulator reset after the gesture timeout |
   | `cooldown` | Navigation event suppressed during cooldown |
   | `recognized` | Engine emitted `swipe_*` or `scroll_*` |
   | `dispatch` | Daemon binding: `known` plus a canonical command name, `no_action` (empty binding), or `unknown` (command text omitted) |

   `no_input=true` means the capture observed zero `hook_delivery` records and had no dropped, capped, admitted-at-deadline, or deadline-boundary in-flight records; registration alone still counts as no hook input. It is not proof that the touchpad is dead. If `records_dropped`, `records_capped`, `records_admitted_at_close`, `records_in_flight_at_close`, or `records_after_close` is non-zero, input may have been lost or crossed the bounded deadline — repeat with a shorter interval or fewer gestures.

7. Set `diagnostic_capture_secs = 0` afterward so the next restart does not rearm capture and overwrite the report. Default-off does not truncate an existing file; a new capture replaces it.

## Config & Runtime Paths

> **Note:** Crate names and on-disk paths still use `leopardwm` internally. A full crate rename is future work.

| Item | Path |
|---|---|
| Config | `%APPDATA%\leopardwm\config\config.toml` |
| State | `%APPDATA%\leopardwm\data\workspace-state.json` |
| Log (stdout) | `%TEMP%\leopardwm-daemon.log` |
| Log (stderr) | `%TEMP%\leopardwm-daemon.err.log` |
| Gesture capture | `%LOCALAPPDATA%\leopardwm\logs\leopardwm-gesture-capture.log` |

## Architecture

LeopardWM is a Rust workspace with five crates:

| Crate | Responsibility |
|---|---|
| `leopardwm-core-layout` | Platform-agnostic scrolling layout engine |
| `leopardwm-platform-win32` | Win32 integration, window operations, DwmFlush animation engine |
| `leopardwm-ipc` | Named-pipe command/response protocol |
| `leopardwm-daemon` | Runtime event loop, state management, dedicated message-pump threads |
| `leopardwm-cli` | User-facing CLI (also installed as `lwm` for shorter typing) |

## Platform Constraints

LeopardWM is a **window controller**, not a compositor. DWM remains the compositor. Behavior can vary across app frameworks (Win32, WPF, Electron, UWP).

LeopardWM runs unprivileged. A window running at a higher privilege level (elevated/administrator, or a protected process) can't be repositioned by an unprivileged process, so LeopardWM leaves it floating instead of reserving an empty column for it, and lists it under `lwm doctor`. Run LeopardWM as administrator if you need those windows tiled (note: an elevated WM has the inverse limitation, drag-and-drop from normal apps into it is blocked).

## Built-in Window Exclusions

LeopardWM automatically skips certain windows that should never be tiled. You can add your own rules via `[[window_rules]]` in the config, but these are always active.

### Skipped window classes (platform layer)

These windows are filtered out during enumeration and never enter the layout engine:

| Class | Why |
|---|---|
| `Progman` | Program Manager (desktop) |
| `Shell_TrayWnd` / `Shell_SecondaryTrayWnd` | Taskbar |
| `WorkerW` | Desktop worker |
| `Windows.UI.Core.CoreWindow` | UWP system windows |
| `XamlExplorerHostIslandWindow` / `TopLevelWindowForOverflowXamlIsland` | XAML islands |
| `RAIL_WINDOW` | WSLg RemoteApp — RDP-projected Linux windows that break when repositioned |
| `Ghost` | DWM hung-window replacement — tiling would duplicate the original |
| `#32770` | Standard Win32 dialog (Open/Save/Print/Properties) |
| `Chrome_RenderWidgetHostHWND` | Internal Electron/Chrome render widget, not a real window |

### Ignored executables (window rules)

These processes are ignored via built-in window rules (action = `ignore`):

| Executable | Why |
|---|---|
| `smartscreen.exe` | Windows Defender SmartScreen |
| `consent.exe` | UAC elevation prompt |
| `msiexec.exe` | Windows Installer |
| `CredentialUIBroker.exe` | Windows credential/login prompt |
| `SnippingTool.exe` | Screen capture overlay |

## Focus Border Corners

The focus border tries to match each window's actual corner radius. Apps that explicitly set `DWMWA_WINDOW_CORNER_PREFERENCE` are honored (`DONOTROUND` → 0 px, `ROUNDSMALL` → 4 px, `ROUND` → 8 px); everything else falls back to the 8 px Win11 default.

Some apps draw their own non-DWM-composited frame with square corners while still reporting the OS default — Firefox / Zen **Picture-in-Picture** popups are the most common example. Override the corner style per window rule:

```toml
[[window_rules]]
match_class = "MozillaDialogClass"
corner_style = "square"  # also: "rounded" | "small_rounded"
```

The `MozillaDialogClass` → `square` rule ships in the default config as a working example. Open **Settings → Window rules**, then use a rule’s **Options** menu to edit corners (`Auto` / `Square` / `Rounded` / `Small rounded`), placement, or its initial **Column width**. Column width is a viewport fraction from `0.05` to `1.0`; leave it blank for the default width.

## Support

If you find LeopardWM useful, consider supporting development:

[![Buy Me a Coffee](https://img.shields.io/badge/Buy%20Me%20a%20Coffee-ffdd00?logo=buy-me-a-coffee&logoColor=000)](https://buymeacoffee.com/jcardama)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## ✨ Contributors

Thanks to everyone who has helped shape LeopardWM.

<a href="https://github.com/Mihir-Null" title="Mihir Talati"><img src="https://avatars.githubusercontent.com/u/86654551?v=4&s=96" width="72" alt="Mihir Talati" /></a><br />
<a href="https://github.com/Mihir-Null"><b>Mihir Talati</b></a> (<a href="https://github.com/Mihir-Null">@Mihir-Null</a>)

## License

[GPL-3.0](LICENSE)
