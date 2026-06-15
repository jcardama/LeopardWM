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

**Sticky windows** — pin a window so it stays visible and follows you across workspaces

https://github.com/user-attachments/assets/43715787-1501-4e19-b693-f301065e914d

## Design Philosophy

A few deliberate **non-features**, so you know what you're getting:

- **Scroll-first, not multi-layout.** No BSP, no DWindle, no Equal/Stair/UltrawideVerticalStack — and we won't add them. niri (Wayland) and PaperWM (GNOME) stay scrolling-only by choice; the horizontal strip *is* the identity. If you want 9 layout variants, [komorebi](https://github.com/LGUG2Z/komorebi) is the right tool.
- **No Virtual Desktop bridging.** Per-monitor workspaces don't map cleanly to Windows' global Virtual Desktops, and the only library that bridges them (`winvd`) breaks every 3-6 months on Windows feature updates. Instead, `Win+Ctrl+Arrow` is intercepted and routed to LeopardWM's workspace prev/next so the native muscle memory still works.
- **Named-pipe IPC, not WebSocket.** Lower latency, no port allocation, no firewall prompts. If browser-based bar integration becomes a real ask, we'll add a thin bridge rather than make the daemon serve sockets directly.

## Features

- Multi-monitor workspaces with monitor-aware focus and move (9 workspaces per monitor)
- Global hotkeys with live config reload
- Smooth scroll animations with layout transition effects (vsync-locked)
- Touchpad gestures with configurable swipe actions
- Drag-and-drop column reorder (Shift+drag to merge windows)
- **Tabbed columns** — toggle a column between vertical-stack and tab-strip mode (`Ctrl+Alt+T`); only the active tab fills the column rect, the rest sit in a clickable strip above
- **Scratchpad** — stash the focused window out of the layout (`Ctrl+Alt+Shift+S`) and summon it back as a floating, centered overlay on demand (`Ctrl+Alt+S`); stash it again to release it back to tiling
- **Sticky windows** — pin a window (`Ctrl+Alt+Y`) so it stays visible on every workspace and follows you as you switch
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
scoop install extras/leopardwm            # Scoop (after `scoop bucket add extras`)
```

Both fetch the signed MSI installer and put `leopardwm`, `leopardwm-cli`, and `lwm` on your PATH. `winget upgrade` / `scoop update` keep you on the latest release.

### Via MSI installer

Download `LeopardWM-x.y.z-x86_64.msi` from [GitHub Releases](https://github.com/jcardama/LeopardWM/releases) and run it. Re-running a newer MSI upgrades in place — no manual uninstall needed.

### Via standalone zip

For users who prefer not to install:

1. Download `LeopardWM-x.y.z-x86_64-windows.zip` from [GitHub Releases](https://github.com/jcardama/LeopardWM/releases)
2. Extract to a permanent location
3. Run `leopardwm.exe`
4. (Optional) Enable autostart: `lwm autostart enable`

Releases are signed via the [SignPath Foundation](https://signpath.org/) program.

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

Most hotkeys use `Ctrl+Alt` as the base modifier. Layered pattern: base = focus, +Shift = move, +Win = monitor scope. Workspace prev/next reuses Windows' native `Win+Ctrl+Arrow` shortcut so it works with existing muscle memory. Every hotkey is rebindable in `config.toml`.

| Key | Action |
|---|---|
| `Ctrl+Alt+H/L/J/K` | Focus left / right / down / up |
| `Ctrl+Alt+Shift+H/L` | Move column left / right |
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
| `Win+Ctrl+Left` / `Right` | Workspace prev / next (cycles) |
| `Ctrl+Alt+W` | Close focused window |
| `Ctrl+Alt+F` | Toggle floating |
| `Ctrl+Alt+Shift+F` | Toggle fullscreen |
| `Ctrl+Alt+T` | Toggle tabbed mode on focused column |
| `Ctrl+Alt+S` | Toggle scratchpad (summon / hide) |
| `Ctrl+Alt+Shift+S` | Stash focused window to scratchpad (or release it back to tiling) |
| `Ctrl+Alt+Y` | Toggle sticky (pin focused window to every workspace) |
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

### Query state

```bash
lwm query workspace    # current workspace placements as JSON
lwm query focused      # focused window info
lwm query all-windows  # every managed window across all workspaces
```

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
```

### Autostart (boot with Windows)

```bash
lwm autostart enable   # writes HKCU\Software\Microsoft\Windows\CurrentVersion\Run
lwm autostart disable  # removes it
```

This is also exposed as a Settings UI toggle and a tray menu item.

### Subscribe to events (status bars, custom integrations)

```bash
lwm subscribe                                       # all events, newline-delimited JSON
lwm subscribe --events workspace,focused_window     # filtered subset
lwm subscribe | jq                                   # pretty-printed in another terminal
```

After the daemon answers `Subscribed`, the connection stays open and streams `IpcEvent` frames (`workspace_changed`, `focused_window_changed`, `layout_changed`, `config_reloaded`, `heartbeat`) as state changes occur. Pipe into a status bar (Yasb, eww, custom Tauri/Electron widgets) to re-render on each event without polling. Full schemas + sample clients in `agent_docs/ipc-events.md`.

### Troubleshooting

```bash
lwm doctor             # diagnostic checks (config valid, daemon reachable, hotkey conflicts, etc.)
lwm collect-logs       # bundles logs + crash reports into a zip for bug reports
lwm reload             # reload config from disk without restarting
lwm refresh            # re-enumerate windows after weird state
lwm panic-revert       # emergency: uncloak everything, drop daemon out of management
```

Run `lwm help` (or `lwm <subcommand> --help`) for the full surface — there are ~40 subcommands.

## Config & Runtime Paths

> **Note:** Crate names and on-disk paths still use `leopardwm` internally. A full crate rename is future work.

| Item | Path |
|---|---|
| Config | `%APPDATA%\leopardwm\config\config.toml` |
| State | `%APPDATA%\leopardwm\data\workspace-state.json` |
| Log (stdout) | `%TEMP%\leopardwm-daemon.log` |
| Log (stderr) | `%TEMP%\leopardwm-daemon.err.log` |

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

LeopardWM is a **window controller**, not a compositor. DWM remains the compositor. Elevated or protected windows may reject placement/styling changes, and behavior can vary across app frameworks (Win32, WPF, Electron, UWP).

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

The `MozillaDialogClass` → `square` rule ships in the default config as a working example. Open **Settings → Window rules** and use the **Corners** column (`Auto` / `Square` / `Rounded` / `Small rounded`) to edit, remove, or add new rules for other apps.

## Support

If you find LeopardWM useful, consider supporting development:

[![Buy Me a Coffee](https://img.shields.io/badge/Buy%20Me%20a%20Coffee-ffdd00?logo=buy-me-a-coffee&logoColor=000)](https://buymeacoffee.com/jcardama)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[GPL-3.0](LICENSE)
