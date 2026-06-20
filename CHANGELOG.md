# Changelog

All notable changes to LeopardWM will be documented in this file.

## 0.2.3

### Fixes

- **Fixed-size launcher and overlay windows are no longer tiled.** A
  non-resizable helper window (for example one of Raycast's) could be given its
  own column, leaving a large empty slot. Only resizable windows are tiled now,
  and a leftover helper from a previous session is cleared on the next refresh.
- **Resolution and monitor changes keep working after you save settings.** Saving
  settings, reloading config, or recording a hotkey stopped the daemon from
  noticing later display changes, so a resolution switch (or a fullscreen app
  that changes resolution) left the layout sized for the old display until a
  manual refresh. Display and power events are now tracked for the whole session.
- **AltGr no longer triggers workspace shortcuts on international keyboards.**
  Hotkeys are now matched by a keyboard hook that tells left and right modifiers
  apart, so AltGr (Right Alt) types accented characters instead of firing
  Ctrl+Alt binds. Alt-based shortcuts use the left Alt key. The hook also drives
  combos Windows owns, like Win+Ctrl+Arrow, so the "Reclaim Windows-reserved
  shortcuts" setting is no longer needed and has been removed; combos reserved
  below the hook, like Win+L, still can't be bound and now show a warning.
- **Hotkey recording works on Dvorak and other non-QWERTY layouts.** The
  Settings recorder captured the physical key position, so a recorded letter
  binding fired from a different key than the one you pressed. It now records the
  letter your layout produces, matching the key that triggers the shortcut.
- **Fullscreen windows no longer swallow commands.** Focus and layout commands
  now drop fullscreen and apply to the visible layout instead of the hidden
  strip; scrolling and resizing are ignored while fullscreen.
- **Focus-follows-mouse no longer flashes the taskbar.** Focus now cancels when
  the cursor leaves a window and is re-checked when it fires, so it only lands
  while the cursor is still over the window.
- **Toggling focus-follows-mouse now takes effect immediately.** The tray and
  Settings toggle installs or removes the mouse hook live, with no restart.
- **Focus-follows-mouse now handles floating and tiled windows consistently.**
  Hovering between them focuses and highlights whichever window the cursor is
  over, in either direction.
- **Floating windows no longer grow each time you move them.** A dragged
  floating window now keeps its size.

### Improvements

- **F13–F24 can be bound as hotkeys.** The Settings recorder and config parser
  now accept the extended function keys, useful for keyboard navigation layers
  that map shortcuts onto them.

## 0.2.2

### Fixes

- **Clearing a hotkey now sticks.** A removed binding used to have its default
  re-added on the next load, so it kept firing and a blank field refilled on
  restart. Cleared bindings are now remembered and stay unbound.

### Improvements

- **Record hotkeys by pressing them.** In Settings, the hotkeys list now
  captures shortcuts directly: click a binding (or press Enter) and press the
  combo instead of typing it. Esc cancels, Backspace clears a binding. Global
  shortcuts are suspended while recording so the combo you press doesn't also
  trigger its current action.
- Long action names in the hotkeys list no longer wrap to two lines; the full
  name shows in a tooltip on hover.
- **The hotkeys list validates live.** Recording a combo already bound to
  another command shows an inline note, and the "couldn't be registered"
  warning now refreshes the moment you change a binding instead of only when
  you reopen Settings.
- **Default workspace prev/next shortcuts are now `Ctrl+Alt+Shift+Left/Right`.**
  The old `Win+Ctrl+Left/Right` are reserved by Windows for virtual desktops and
  never registered. Existing configs keep their current binds.
- **Reclaim Windows-reserved shortcuts** (opt-in, off by default). A new
  Behavior setting lets combos Windows owns, like `Win+Ctrl+Arrow`, drive
  LeopardWM instead of switching virtual desktops. Only known multi-modifier OS
  combos are reclaimed (never combos another app owns); bare `Win+key`,
  protected combos like `Win+L`, and keys sent to elevated windows are out of
  scope.
- Add `lwm workspace-next` and `lwm workspace-prev` to cycle through workspaces
  from the CLI (the `workspace_prev`/`workspace_next` hotkeys already existed).
- **Sticky windows keep their mode.** `Ctrl+Alt+Y` no longer force-floats the
  window: a tiled window stays tiled and follows you across workspaces as a
  column you can cycle to, while a floating window stays a floating overlay (as
  before). The mode is whatever the window already is when you toggle it.
- **Moving a stacked window past the end of the strip unstacks it.** Pressing
  move-window toward the edge now pops the window out into its own new column
  off that end, instead of doing nothing.
- **Jump to or move a column to the start/end of the strip.** New
  `focus_start`/`focus_end` (`Ctrl+Alt+Home`/`End`) jump focus to the first or
  last column, and `move_column_to_start`/`move_column_to_end`
  (`Ctrl+Alt+Shift+Home`/`End`) relocate the focused column there. Also as
  `lwm focus start|end` and `lwm move start|end`. Home/End are now valid keys
  in hotkey bindings.

## 0.2.1

### Fixes

- **The daemon no longer requests elevation by default.** It runs at the same
  privilege as the terminal that launched it, so admin users no longer get a UAC
  prompt on launch and `lwm` works from a normal terminal. To manage elevated
  apps you can still run the daemon as administrator; the IPC pipe now grants the
  current user access either way, so `lwm` connects from a normal terminal
  instead of failing with access-denied.
- **Hidden windows keep their width when they reappear.** A managed window that
  is hidden and later shown again (for example by a third-party virtual-desktop
  tool) now re-tiles at its previous column width instead of resetting to the
  default.
- **Rejected hotkeys are no longer silent.** When the OS won't register a key
  combo (Windows or another app already owns it), the Settings hotkeys tab shows
  a dismissible warning listing the rejected binds, and the daemon logs them.
- **Logs are written to a file now**, at
  `%LOCALAPPDATA%\leopardwm\logs\leopardwm-daemon.log` (the folder the tray's
  "View Logs" opens and `lwm collect-logs` reads). Previously they went only to
  stdout and were lost on a direct launch. `collect-logs` also bundles the
  watchdog log.

## 0.2.0

### Improvements

- **Overview mode.** `Ctrl+Alt+Space` (or `lwm toggle-overview`) opens a
  map of the focused monitor's non-empty workspaces over a blurred
  backdrop: one row per workspace, each window a rounded card (icon +
  title) at its true position in the strip, with a ring marking the
  active workspace's visible region. Click a card to jump to that window,
  click a row to switch workspace, middle-click a card to close it,
  arrows/Enter and digits 1-9 work from the keyboard, Esc dismisses.
  Cards show live window previews (windows on hidden workspaces show
  their last frame); set `render = "placeholder"` under `[overview]` or
  pick it in Settings to keep the icon placeholders. A third option,
  `render = "snapshot"`, captures each window right before it leaves the
  screen and shows that frame instead (icon until one exists). Opening
  and closing the overview zooms: cards fly between their real window
  positions and their spots on the map while the backdrop fades. Speed
  and easing come from a dedicated `overview_duration_ms` under
  `[animation]` and your configured easing curve (instant under reduced
  motion), tunable in Settings.

### Fixes

- **Tiled windows follow taskbar work-area changes.** Toggling the
  taskbar between auto-hide and always-visible now re-fits tiled windows
  to the new work area instead of leaving them behind a now-permanent
  taskbar. The work area was previously only refreshed on a display
  change, not on this setting change.
- **Sessions restore fully across a daemon restart.** Each window returns
  to the exact monitor, workspace, column grouping, column width, height,
  and scroll position it had last session, rebuilt from the saved
  snapshot rather than re-derived from current window positions (which
  scattered windows whose position was stale or off-screen, and reset all
  sizes to defaults). Windows that closed while the daemon was down are
  dropped; windows opened since are added. Workspace state is now saved
  continuously (debounced, written atomically) rather than only on
  graceful shutdown, so the layout survives a crash too.
- **Off-screen columns no longer bleed onto other monitors.** On a
  multi-monitor setup, a column scrolled fully off one monitor is parked off
  every display instead of reappearing on the neighbor.
- **New windows open on the active monitor.** A new window now opens on the
  monitor that currently has focus, instead of wherever it first appeared.
- **Resizing a column keeps it in view.** Changing the focused column's width
  (cycle, set-width, equalize, or drag-resize) now scrolls the viewport to keep
  the column on screen, instead of letting a column near the edge grow
  off-screen.
- **Scratchpad stashes and restores the right window.** Stashing reliably
  targets the focused window instead of occasionally grabbing a neighbor, and
  releasing returns the window to its original column with focus intact rather
  than dropping it into a new column or focusing a sibling.

## 0.1.19

### Improvements

- **Scratchpad.** Stash a window into a hidden holding area and summon it
  back as a floating, centered overlay with a hotkey, handy for a quick
  terminal or notes window. `Ctrl+Alt+Shift+S` designates the focused
  window as the scratchpad and hides it; `Ctrl+Alt+S` toggles it in and
  out. Also available as `lwm scratchpad-stash` / `lwm scratchpad-toggle`.
  The designation lasts for the session (it resets if you restart the
  daemon).
- **Sticky windows.** Pin a window so it stays visible on every workspace
  and follows you when you switch. `Ctrl+Alt+Y` toggles it (or `lwm
  toggle-sticky`). A pinned window floats and re-homes to whichever
  workspace you move to, sitting still while the rest of the layout
  scrolls. Toggle it off to unpin; it stays floating. Session-scoped.
- **Consume window into column.** Pull the neighbouring column's window
  into the focused column as a stack. `Ctrl+Alt+,` consumes from the left,
  `Ctrl+Alt+.` from the right (or `lwm consume left` / `lwm consume
  right`). The inverse of expel; combine with tabbed columns to group
  windows.
- **Friendlier hotkeys in config and settings.** Key chords now use
  symbols (`Ctrl+Alt+[` rather than `Ctrl+Alt+Bracket_Left`) in the
  generated config and the settings window, and you can write either form.
  The settings hotkey list shows every action with a readable name in a
  sensible order. `lwm config init` now produces a complete, correct
  hotkey config.
- **On-overflow centering mode.** `centering_mode = "on_overflow"` centers
  the focused column only when it is wider than the viewport; columns that
  fit scroll just into view. Selectable in Settings and the tray menu
  alongside the existing center and just_in_view modes.
- **New-window placement mode.** Choose where newly opened windows go:
  their own new column (default) or stacked into the focused column,
  directly below the focused window. Set `new_window_placement =
  "in_column"` in config, pick it in Settings or the tray menu, or flip it
  on the fly with `lwm toggle-new-window-placement` (bindable as
  `toggle_new_window_placement`).
- **Tray double-click opens Settings.** Double-click the tray icon to open
  the settings window; single and right click keep showing the menu.
- **Per-app open rules.** Window rules can now control how an app opens:
  `open_on_workspace = 5` opens it on that workspace in the background,
  `column_width = 0.5` sets the initial column width as a viewport
  fraction, and `open_maximized = true` opens the column maximized. The
  settings rules table gained workspace and maximize columns, and saving
  from Settings no longer drops rule fields the table does not show.

### Fixes

- **Pinned windows stay visible and focused over fullscreen.** A sticky
  window now remains at its position above a fullscreen window instead of
  being hidden, and keeps focus when you switch to that workspace while
  focused on it.
- **Hidden windows no longer jump to the top-left corner.** Entering
  fullscreen parked every other window at the screen origin; they now keep
  their real positions while hidden and reappear exactly where they were.

## 0.1.18

### Improvements

- **Smooth Chromium and Electron animations are now on by default.** The
  experimental swap-chain animation that shipped opt-in in v0.1.16 has
  soaked with no critical issues, so it is enabled out of the box. Add
  `swap_chain_ghost_animation = false` under `[behavior]` to fall back to
  the old per-frame path.
- **`lwm doctor` now reports the animation thumbnail balance**, a quick
  health signal that should read 0 at rest. Useful for confirming the
  animation system isn't leaking compositor handles.
- **Animation system self-recovers from an interrupted handoff.** If an
  internal animation step is ever cut short, the affected windows are no
  longer skipped by the smooth-animation path on later moves; the guard
  clears itself within a couple of seconds.
- **Animation timing is now configurable.** A new `[animation]` section
  sets the duration of layout transitions, workspace switches, and column
  scrolls, plus the easing curve (linear / ease in / ease out / ease
  in-out). Tune them in Settings → Behavior → Animation or in config.
  Set any duration to 0 to snap instantly.
- **Workspaces can have names.** Label workspaces 1-9 (e.g. "web",
  "code") in Settings → Layout or via `[workspaces].names` in config. The
  name shows in `lwm query workspace` and is pushed to status bars over
  IPC, so a bar can display a label instead of a number.

### Fixes

- **A keypress can no longer trigger the wrong action right after saving
  settings.** Hotkey IDs were assigned in an unstable order, so reloading
  the config (e.g. on a settings save) could remap an ID to a different
  command and let an in-flight keypress fire the wrong one, in the worst
  case an accidental emergency-restore. IDs are now derived from the key
  combo itself, so a given key always maps to its own current action.

## 0.1.17

### Fixes

- **External bars now see all focus changes, not just clicks and Alt+Tab.**
  Focus changes triggered by `lwm focus left/right`, workspace switches, and
  similar keyboard commands were silently swallowed by the IPC event stream,
  so any custom bar showing the current window title would go stale until you
  clicked something. They fire correctly now.

### Docs

- **IPC events reference** (`agent_docs/ipc-events.md`) updated for v0.1.17:
  documents the previously undocumented per-column `mode` field on
  `LayoutChanged` (vertical vs tabbed), clarifies that `lwm subscribe`
  consumes the `Subscribed` ack frame internally, adds a Yasb custom-widget
  config snippet that needs no plugin code, and adds a checklist for daemon
  developers wiring new state changes into the event stream.

## 0.1.16

### Improvements

- **Smooth animations for Chromium and Electron apps (Preview).** Chrome,
  Edge, Slack, Discord, Beeper, Spotify, VS Code, Cursor, Firefox / Zen,
  and Windows Terminal Preview now glide through layout transitions
  instead of stuttering. Replaces the v0.1.10 landing nudge, which only
  cleaned up after the animation finished. The 1px wobble during scrolls
  and the first-frame stall after the landing are both gone.

  Animations stay smooth across:

  - Column scrolls with multiple Chrome / Electron columns visible
  - Off-screen columns scrolling into view
  - Tab cycling inside a tabbed column
  - Rapid back-to-back scrolls (one transition can preempt another
    cleanly mid-fade)
  - Pause, reload, display change, drag cancel, and window close
    mid-animation

  **Default is off in this release.** Enable in Settings → Behavior →
  "Smooth Chromium animations", run `lwm ghost enable`, or add
  `swap_chain_ghost_animation = true` under `[behavior]`. If nothing
  breaks during the opt-in window, the default flips on in v0.1.17.

  **Known limitations:**

  - The focused window keeps using the original animation path. This
    avoids a Windows quirk where the focus-restore call can land on a
    temporarily-hidden window. Every other window in the same transition
    still gets the smooth path.
  - Cross-monitor moves are not animated through the new path. They use
    the v0.1.15 path, which already looked fine for that case.

## 0.1.15

### Improvements

- **Tab strip action affordances.** Tabbed columns now behave like browser
  tabs: hover close-X, middle-click to close, right-click context menu, and
  inline rename. No more `Ctrl+Alt+W` as the only way to remove a single tab.

  - **Hover close-X** with a Fluent-style tooltip. Narrow tabs ellipsize
    first, then hide the icon, then hide the close-X (right-click still
    works at any size).
  - **Middle-click closes** the tab, same as the X.
  - **Configurable close action.** `[behavior].tab_close_action` (Settings →
    Behavior) sets what the X and middle-click do: `close_window` (default,
    browser-style) or `untab` (rip the tab into a new vertical column to
    the right). Right-click menu items always carry their literal action.
  - **Inline rename.** Right-click → "Rename tab…" *or* double-click a tab
    title turns the tab into an in-place editor: a pill popup over the
    tab with the icon, the title in an embedded text field, and a save
    check on the right. Enter or check commits; Esc or click-away cancels.
    Empty submission clears the override (live title returns). Max 128
    UTF-8 bytes. Overrides are keyed by HWND so they survive workspace
    moves, untab, and re-tab, and persist across daemon restarts.
  - **Tab-strip font + icon sizing** now matches Win11 Terminal / Edge:
    Segoe UI ~12 px char height and ~16 px icon at the default 28 px
    strip. Both scale with strip height.
  - **About page** now shows the running version (was hardcoded `0.1.0`).

  **Implementation notes**
  - Close handling is daemon-internal (no new IPC variant; `IPC_PROTOCOL_VERSION`
    stays at 2). Untab reuses the v0.1.14 `pending_tab_focus` machinery; no
    new core_layout operations.
  - The rename popup is a uniform-alpha layered top-level window on a
    dedicated thread, with DWM rounded corners for AA edges. Rename target
    is captured by HWND at spawn (not by tab index), so column mutations
    during the popup's lifetime can't retarget the override. The tab icon
    is `CopyIcon`'d so the popup is independent of source-window lifetime.
  - The check-button tooltip reuses the strip's tooltip pipeline, so it
    matches the close-X tooltip exactly.

## 0.1.14

### Improvements

- Add **tabbed columns** — toggle a column between vertical stacking and a clickable tab strip with `Ctrl+Alt+T`. Inspired by niri's headline Linux feature; the *combination* of tabs and a scrolling viewport is genuinely 1-of-1 on Windows today (komorebi's `stackbar` is a static container, GlazeWM has nothing).

  **Using tabbed columns**
  - `Ctrl+Alt+T` on the focused column toggles between vertical-stack mode (the default) and tabbed mode, where only the active tab fills the column rect and the rest sit in a strip above
  - `Ctrl+Alt+J` / `Ctrl+Alt+K` cycle the active tab — the existing intra-column focus keys do double duty, so there are no new bindings to learn
  - Click any tab in the strip to activate it; the click is a real focus change, so the border, foreground state, and IPC events all follow
  - Tab titles update live as windows rename themselves (Chrome navigations, Slack channel switches, terminal `cd`s)
  - Tab icons refresh every couple of seconds so notification-badge swaps in Discord, Slack, etc. propagate without manual interaction
  - The active-tab highlight slides between tabs over a 150ms ease-out animation when you cycle
  - A tabbed column with one window auto-reverts to vertical mode (a 1-tab tabbed column is visually identical anyway); minimizing the active tab falls back to the next visible one so the column always renders something

  **Drag-and-drop (Chrome semantics)**
  - Drop a window anywhere on a tabbed column — body or strip — and it appends at the rightmost position and becomes the active tab ("the tab I just added goes to the right and that's what I want to see")
  - The drag-preview ghost spans the entire column rect when hovering a tabbed target rather than a per-slot rectangle, so the drop zone reads as "this whole column" instead of misleading vertical-stack semantics
  - The strip stays pinned through the entire drag — including cross-column drags that briefly focus a Vertical column on the way (it falls back to rendering the workspace's first tabbed column so it never flickers in and out)
  - The dropped window lands at its tab position without a smoothed slide

  **Appearance and configuration**
  - Strip height, background colour, active/inactive text colours, active-tab background, and opacity are all configurable from `[appearance]` (`tab_strip_height`, `tab_strip_bg`, `tab_strip_active_bg`, `tab_strip_active_text`, `tab_strip_inactive_text`, `tab_strip_opacity`) and surfaced in the WebView Settings UI alongside the border controls, with a divider rule separating the strip section from the border-position field
  - The strip's corner radius matches Win11's window radius automatically and tab buttons round to a slightly smaller inner radius for outer > inner parity; the gap between strip and window content reuses the workspace's inter-element `gap` value so the tabbed area visually rhymes with adjacent columns

  **Persistence and lifecycle**
  - Tabbed mode (and which tab is active) survives daemon restart via the existing `StateSnapshot` JSON
  - Existing v0.1.13 configs round-trip cleanly with `mode = vertical` defaults — no migration needed
  - The strip hides during fullscreen, pause, and on workspaces with no tabbed column, and repositions per animation frame so it tracks the column rect smoothly through workspace switches and monitor changes

  **For bar integrators and scripters**
  - New IPC commands: `ToggleTabbed` (no payload) and `SetActiveTab { column, tab }` — the latter is what the strip's click handler synthesizes internally, with `(monitor, workspace_idx, column_idx)` captured at draw time so click dispatch doesn't race with focus changes
  - New CLI subcommand: `lwm toggle-tabbed`
  - `lwm subscribe --events layout` now emits an additional `ColumnSummary.mode` field on every `LayoutChanged` event — either `{"type":"vertical"}` or `{"type":"tabbed","active_idx":N}`. `IPC_PROTOCOL_VERSION` is bumped from 1 to 2; v1 clients (no `mode` key) still parse cleanly via `serde(default)`

  **Implementation notes (for the curious)**
  - The strip is a Win32 `WS_EX_LAYERED` window with GDI text rendering (`Segoe UI`, DPI-scaled, ellipsis on overflow, automatic font-link fallback for CJK/emoji via Microsoft YaHei / Yu Gothic / Malgun Gothic / Segoe UI Emoji)
  - Anti-aliased rounded fills via per-pixel alpha compositing (`UpdateLayeredWindow` with `AC_SRC_ALPHA`, 4×4 supersampled coverage); no GDI+ dependency
  - Click hit-testing uses `WM_LBUTTONDOWN` with a per-pixel alpha-byte fixup so layered-window hit-testing works regardless of blend mode; `WM_MOUSEACTIVATE → MA_NOACTIVATE` keeps the strip from stealing focus when clicked; `LoadCursorW(IDC_ARROW)` ensures the standard pointer instead of the daemon's busy cursor
  - Icon retrieval falls through `WM_GETICON` (`ICON_SMALL2` → `ICON_SMALL` → `ICON_BIG`) with `GCLP_HICONSM` / `GCLP_HICON` class fallbacks; title-change notifications come from an `EVENT_OBJECT_NAMECHANGE` (`0x800C`) WinEvent hook

  **Hotkey divergence from the original spec:** the planning roadmap proposed `Ctrl+Alt+T` + `Ctrl+Alt+[` / `Ctrl+Alt+]` (cycle) + `Ctrl+Alt+Shift+T` (untab); the brackets are already `move_window_left/right` and the `J/K` cycling makes the others redundant, so v0.1.14 ships with one new binding total

## 0.1.13

### Improvements

- Add an IPC pub/sub event subscription channel for external bars (Yasb, Tauri/Electron-based custom bars, eww). `lwm subscribe` opens the named pipe, sends `IpcCommand::Subscribe { events }`, and streams newline-delimited JSON `IpcEvent` frames to stdout — pipe into `jq` for ad-hoc inspection or wire into a status bar to re-render on each event. Five event kinds: `WorkspaceChanged`, `FocusedWindowChanged`, `LayoutChanged`, `ConfigReloaded`, `Heartbeat` (every 30s of silence). Filter at the daemon level via `--events workspace,focused_window` or omit for "all kinds". The handshake is atomic: when the daemon receives `Subscribe`, the broadcast `Receiver` and a snapshot of current state are constructed in the same `AppState` mutex critical section, so no event between handoff and stream-start can be lost. Capacity is 256 buffered events per subscriber; a subscriber that lags further behind receives `IpcEvent::Lagged { skipped }` and should reconnect for a fresh snapshot. `LayoutChanged` carries inline column structure (`window_ids`, `width_px` per column) so bars don't need a follow-up `QueryWorkspace`. Sender-side signature dedup prevents the per-frame animation spam: only structurally-distinct settled layouts emit. The protocol is mode-switching: after the `Subscribed` ack frame (tagged with `status`), every subsequent frame is an `IpcEvent` (tagged with `type`) — clients must transition parsers. Documented in `agent_docs/ipc-events.md` with sample clients (Rust, Python, Yasb recipe). Backwards compatible: existing CLI users never send `Subscribe` so the request/response path is unchanged

## 0.1.12

### Reliability

- Add `leopardwm-watchdog.exe`, a tiny supervisor process that wraps the daemon. `lwm run` now spawns the watchdog by default; the watchdog spawns the daemon as its child and `wait()`s. On a clean exit (`lwm stop`, normal shutdown), the watchdog exits with the same status. On a non-zero / panic exit, the watchdog calls `uncloak_all_visible_windows()` from `platform_win32` directly (no IPC needed) so any windows the daemon had cloaked off-screen become visible again, then auto-restarts the daemon and surfaces a Win10/11 toast ("LeopardWM recovered — the daemon crashed and was restarted automatically") via `ToastNotificationManager` with a registered AppUserModelID. Crash-loop budget: 3 abnormal exits within 60s causes the watchdog to give up, surface a warning toast ("LeopardWM disabled — run `lwm doctor`"), and exit so the user can investigate (`%TEMP%\leopardwm-daemon.err.log`). Prior to this, a hard daemon panic could leave windows cloaked off-screen until the user ran `lwm panic-revert` from another terminal — the watchdog closes that window. `lwm run --no-watchdog` opts out for development. The watchdog binary is bundled into the MSI installer, the GitHub-Releases zip, and the Scoop manifest's install directory (it's not shimmed onto PATH because users don't invoke it directly — `lwm run` finds it as a sibling of the CLI binary). The watchdog also installs itself into a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` so the daemon dies with the supervisor (no orphaned daemon if the watchdog process is killed). The toast renders on a worker thread wrapped in `catch_unwind` with explicit `CoInitializeEx(MTA)` for WinRT, so a notification failure cannot take down the supervisor. Caveat: maximizebox restoration on hard `taskkill /F` of the daemon falls outside the watchdog's reach (the maximizebox set is process-local to the daemon); for that scenario, run `lwm panic-revert`. Most daemon panics fire the in-process panic hook, which handles maximizebox before exit

### Improvements

- Per-window focus-border corner radius. Trusts `DWMWA_WINDOW_CORNER_PREFERENCE` only when the app has *explicitly* opted into a non-default value: `DWMWCP_DONOTROUND` → 0 px / square, `DWMWCP_ROUNDSMALL` → 4 px, `DWMWCP_ROUND` → 8 px. Apps that report `DWMWCP_DEFAULT` (the value every app gets unless it overrides) fall back to the 8 px Win11 default — but apps like Firefox / Zen Picture-in-Picture report DEFAULT while drawing their own non-DWM-composited square frame, so a per-rule `corner_style = "square" | "rounded" | "small_rounded"` override is the escape hatch. The default config now ships with `[[window_rules]] match_class = "MozillaDialogClass", corner_style = "square"` as a working example users can edit or remove
- Surface `corner_style` in the Settings WebView. The Window Rules table now has a **Corners** column with `Auto` / `Square` / `Rounded` / `Small rounded` values, round-tripping through save/load alongside the existing class/title/executable/action fields. `Auto` (the default) is omitted from the saved TOML so existing rules without `corner_style` round-trip unchanged

## 0.1.11

### Improvements

- Add a short `lwm` CLI alias alongside `leopardwm-cli`. Same source, same behavior — just a faster name to type. Both binaries are shipped in the MSI installer, the GitHub-Releases zip, and on PATH after install. The full `leopardwm-cli` name remains the canonical reference in docs and command examples
- Forward `Win+Ctrl+Left` and `Win+Ctrl+Right` to LeopardWM workspace prev/next. Hijacks Windows' native Virtual Desktop switch shortcut so the user's existing muscle memory drives LeopardWM workspaces directly, with cycle wrapping (workspace 1 ← Win+Ctrl+Left from workspace 9, etc.). New IPC commands `WorkspacePrev` and `WorkspaceNext`, hotkey command names `workspace_prev` / `workspace_next`. Both are in the default `[hotkeys]` section — rebind freely if you want native VDs back
- Add an in-app update notifier. The daemon polls the GitHub Releases API once on startup (after a 30-second delay) and once per day, comparing the latest tag against `CARGO_PKG_VERSION` via semver. When a newer release is observed, the tray's "Check for Updates" menu item relabels to `Update available: vX.Y.Z`; clicking opens `https://github.com/jcardama/LeopardWM/releases` in the browser. No auto-download, no telemetry beyond the single anonymous HTTPS GET to `api.github.com`. Opt out via `behavior.check_for_updates = false` in `config.toml`. Useful for users on standalone-MSI installs; users on winget/Scoop continue to get updates natively via `winget upgrade` / `scoop update`

### Distribution

- Add MSI installer built via `cargo wix` from `wix/main.wxs`. Installs to `C:\Program Files\LeopardWM\bin\` and adds the bin folder to system PATH so `leopardwm-cli` is callable from any terminal. Re-running a newer MSI upgrades in place (WiX `MajorUpgrade` with `Schedule='afterInstallInitialize'`); same-version reinstall is also allowed (`AllowSameVersionUpgrades='yes'`). The fixed `UpgradeCode` GUID is constant forever — changing it would break upgrade behavior. Release workflow now publishes both the existing `LeopardWM-X.Y.Z-x86_64-windows.zip` and `LeopardWM-X.Y.Z-x86_64.msi`, plus a `checksums.txt` with SHA256 sums (used downstream by winget/Scoop manifests)

## 0.1.10

### Internal

- Add a two-layer regression guard so the BorderFrame/OverlayWindow mouse-lag class of bug cannot silently reappear: a `#[cfg(test)] panic!()` at the top of `BorderFrame::new` and `OverlayWindow::new` catches anyone constructing either from a `platform_win32`-internal test, and a new `test_app_state_skips_border_frame_under_cfg_test` daemon test asserts `state.border_frame.is_none()` and `state.paused` after `AppState::new_with_config` — fails immediately if the `cfg(test)` gate at `state.rs:370` is ever removed
- Skip `BorderFrame::new()` under `#[cfg(test)]` so daemon test setup no longer creates a real `WS_EX_LAYERED` DWM-composited window + message-pump thread per `AppState`. With ~150 daemon tests running in parallel, the live composited surfaces (cursor included) all serialized through DWM, visibly lagging the user's mouse for the 10 s test run. Removing the per-test border window dropped the daemon suite from 9.65 s to 0.73 s — a 13× speedup — and the cursor stays smooth throughout `cargo test --all`
- Default `AppState` to `paused = true` under `#[cfg(test)]` so the placement worker no longer calls `apply_placements` with placeholder hwnds (100, 200, 300, 800, …) during `cargo test`. Those values can collide with real running window handles, and the resulting `IsWindow` / `SetWindowPos` / `DwmGetWindowAttribute` calls block on the user's live DWM compositor — visibly lagging the mouse for the duration of the test run. Tests that exercise the apply_layout pipeline (toggle-pause flows, injected-behavior worker tests, snap-config reload) opt back in with `state.paused = false`. Side-effect: removes the `test_cmd_reload` parallel-contention flake — the worker is no longer spawned at all, so the 5000 ms timeout race goes away

### Bug Fixes

- Fix the focus border racing ahead of windows during workspace switches, move-to-column, expel, and drag-merge animations — `compute_window_layout_rect` returned the post-transition (final) rect for managed tiled windows even mid-transition, so the border snapped to the destination on frame 1 while the windows themselves interpolated frame-by-frame, yielding a visible "border-leads-windows" lag. The function now applies `apply_transition_interpolation` against the active `layout_transition` (if any) before returning, so the border tracks the same eased interpolated rect the worker is sending each frame
- Fix the `MovedOrResized` swap-chain guard masking real displacements — the chrome-rect cross-check used a position-only 30 px epsilon while the DWM path used 20 px, so a real 25 px displacement (small drag, theme/DPI nudge that genuinely moved the window) was treated as a swap-chain artifact and silently swallowed. Both paths now use the same 20 px epsilon: the chrome rect from `GetWindowRect` is converted to its visible-frame equivalent by subtracting `get_window_invisible_insets` (the cached DWM transparent-resize-border, ~7 px on Win10/11), and the comparison is done in the same coordinate space as the layout rect. New `get_window_invisible_insets(WindowId)` exposed from `platform_win32::placement` for this purpose
- Fix the `apply_layout` fast-path being silently disabled after every workspace switch — `last_placed_layout_rects` accumulated entries for windows that were removed from the active workspace (workspace switch, expel-to-workspace, manual close mid-animation) but never drained, so `placements.len() != last_placed_layout_rects.len()` and the placement-unchanged short-circuit was skipped, falling back to the full sync `SetWindowPos` path on every focus shift. The fast-path now retains only entries whose hwnd is in the current `all_placements` before checking for changes, so workspace switches and removals no longer permanently disable the fast-path

- Fix the focus border leaving a 1 px gap from the window edge after the show_border switch from DWM-tracking to layout-rect-tracking — `border::reposition` (the DWM-tracking path) compensates for the 1 px transparent resize border included in `DWMWA_EXTENDED_FRAME_BOUNDS` by shrinking the rect on each side; `border::show_at_rect` (the layout-rect path) was missing that compensation, so the border drew 1 px outside the visible window edge for every managed tiled window. `show_at_rect` now mirrors the same shrink for `BorderPosition::Outside` so the border sits flush with the content edge again
- Fix off-screen columns peeking 1-2 px through the right/left outer-gap area at rest — the natural strip-flow position for an `OffScreenRight` column at the boundary lands at `viewport.x + viewport.width - outer_right` (i.e. `outer_right` pixels INSIDE the viewport), and DWM cloaking is fundamentally racy enough that the leftmost off-screen column's first few pixels would visibly leak. `compute_non_fullscreen_placements` now clamps `OffScreenRight` columns to `screen_x >= viewport.x + viewport.width` and `OffScreenLeft` columns to `screen_x + eff_width <= viewport.x`, pushing them past the viewport edge. The clamp/no-clamp boundary aligns exactly with the cloak/uncloak transition in `apply_placements`, so a column transitioning OffScreen → Visible has its position jump and its uncloak commit in the same `DeferWindowPos` batch — the user only ever sees the column appear at its natural visible-edge position
- Fix random scheduled-task PowerShell windows reflowing the layout every 5 minutes — Windows scheduled tasks that run PowerShell scripts (Corsair, Defender, third-party tools) briefly create a `ConsoleWindowClass` window with the executable path as its title before they finish initializing, even when invoked with `-WindowStyle Hidden`. The daemon's WinEvent hook caught the brief `Created` event and tiled it, and ~140 ms later when the script terminated the `Hidden` event reflowed the layout again. `Created` now skips `ConsoleWindowClass` windows whose title equals the executable path or ends with `.exe` (a clear "this window has not finished initializing" signal); a real interactive PowerShell user is recovered on `Focused` once it has set a real title (e.g. "Administrator: Windows PowerShell"), via the same redispatch-as-Created pattern that handles tray-restored apps
- Fix the focus border missing the very first frame of an animation — the post-frame `tick + show_border` reorder synced frames 2+ to the worker's vsync, but frame 1 was painted before any `AnimationFrameApplied` arrived. `send_animation_frame` now updates the border immediately after queueing the worker frame, so both SetWindowPos calls commit before the same DwmFlush vsync and the border is in lockstep from frame 1 onward
- Fix the focus border tracking stale DWM bounds when a managed tiled window has no current placement (mid-minimize, transient mid-removal) — `show_border`'s tiled-window path now hides the border in this state instead of falling through to `frame.show(hwnd)`, which would leave a colored frame floating where the window used to be
- Make the random-focus-stealing suppression more robust — bumped the input-recency threshold from 500 ms to 1500 ms so that a `Focused` event delivered through the WinEvent hook -> tokio mpsc -> daemon mutex pipeline cannot miss a real Alt+Tab or click under daemon load. `GetLastInputInfo` failure now fails closed (no auto-scroll) instead of restoring the pre-fix random-scroll behavior
- Fix focus events draining for seconds after rapid Ctrl+Alt+Right/Left presses stop — every hotkey ran a synchronous `apply_layout` on the daemon mutex, costing ~100–200 ms per call (sync `SetWindowPos` over every placement, `DwmFlush` round-trip, size-violation queries against DWM, plus the sticky-compositor `(w-1 → w)` nudge per Chromium/Firefox/Cascadia window) and serializing every queued event behind the previous one. `apply_layout` now fast-paths when every placement matches `last_placed_layout_rects`, so focus shifts within the already-visible range return in microseconds and the input-tail lag is gone
- Fix the visible 1 px wobble on Zen / Slack / Cascadia / Beeper on every focus shift, drag, or window event — the `(w-1 → w)` sticky-compositor nudge fired on every `apply_layout` landing pass when its only job is to repair swap-chain desyncs after a rapid async-frame burst. The nudge now runs only after an actual scroll or layout transition completes (gated by a `post_animation_nudge_pending` flag set in the post-animation landing pass at `main.rs`); routine applies skip it entirely
- Fix `MovedOrResized` snap-back chasing stale `DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS)` for Chromium/Firefox/Cascadia under the swap-chain-stale bug — DWM reports the visual content position (where the compositor is rendering) rather than the chrome HWND position when the renderer falls behind, which can read tens to thousands of pixels off after a rapid async burst even though the window has not moved. The handler now cross-checks against `GetWindowRect` (the OS-tracked chrome rect, immune to compositor state) with a position-only 30 px epsilon — if the chrome HWND is at the expected layout position but DWM disagrees, we treat it as a swap-chain artifact and skip the snap-back
- Fix tray apps that boot in a minimized state (Raw Accel, Discord, Spotify with "minimize to tray") never entering the managed set until the user manually triggered a refresh — `enum_windows_callback` skips `IsIconic` windows during startup enumeration. The `WindowEvent::Restored` handler used to drop unmanaged restores on the floor; it now re-dispatches them as `Created` events so the standard rule/tile pipeline picks them up the first time the user actually brings them on screen
- Fix `cargo test` lagging the user's mouse and retiling real windows when run alongside a live daemon — daemon-level tests use placeholder hwnds (100, 200, 800, …) that can collide with real HWNDs on the running system, and the `AttachThreadInput` path inside `set_foreground_window` was attaching to those real threads' input queues. `set_foreground_window` is now skipped under `#[cfg(test)]`; internal state-tracking still runs so test assertions pass
- Fix windows not snapping back after a small drag or in-preset resize — the new `apply_layout` fast-path correctly saw "placements unchanged" when `snap_column_width_to_preset` rounded the resize to the same preset bucket, or when a same-column drop reinserted the window at the same slot. The window stayed wherever the user released it because no `SetWindowPos` was ever fired. `MoveSizeEnd` now evicts the affected hwnd from `last_placed_layout_rects` at the top of the handler, so every drop path (`snap_back_tiled`, `execute_window_merge`, `finalize_drag_merge`, `handle_resize_complete`) forces a full re-apply that physically returns the window to its layout slot
- Fix random focus stealing scrolling the layout to a window the user did not request — when an external app fires `EVENT_SYSTEM_FOREGROUND` (notifications, app-internal focus shuffle, background process taking foreground), the daemon's `Focused` handler ran `ensure_focused_visible_animated` and yanked the viewport to the new window. The handler now consults `GetLastInputInfo`; with no user input in the last 500 ms the focus event is treated as spurious — internal focus tracking and the border still update, but the auto-scroll is skipped, so a stray foreground change no longer hijacks the scroll position
- Fix the focus border drifting away from the window after drag snap-back / resize / scroll landing — `show_border` was falling through to `frame.show(hwnd)` which queries `DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS)` for the target window's current bounds. Those bounds lag the SetWindowPos commit by 1–2 vsyncs, so the border ended up at the window's pre-move position for a few frames. For managed tiled windows the layout rect is now authoritative — `show_border` always reads it directly from `compute_window_layout_rect` regardless of transition state. Floating windows still go through `frame.show(hwnd)` because their position is the user's, not a layout slot
- Fix the focus border scrolling at a different framerate than the windows during a smooth scroll, producing visible choppiness — the `AnimationFrameApplied` handler updated the border *before* `tick_animations`, so the border's SetWindowPos for frame N committed at vsync N+1 (one frame after the worker's window updates landed at vsync N). The handler now ticks first, repositions the border to the next interpolated state, then dispatches the next worker request — both updates queue for the same DwmFlush vsync and arrive on screen together

## 0.1.9

### Improvements

- Add "Start with Windows" toggle in Settings (Behavior section) and tray menu — surfaces the previously CLI-only `leopardwm-cli autostart enable|disable` as a one-click option backed by `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Registry-backed (single source of truth, shared with the CLI command); the daemon resolves its own path via `current_exe()` so the value points at the binary that wrote it. Refuses to enable when the resolved binary lives under `%TEMP%` to prevent persisting paths that vanish across reboots. New IPC commands `GetAutoStart` / `SetAutoStart` and `IpcResponse::AutoStartState` for external clients

## 0.1.8

### Improvements

- Hide focus border in fullscreen mode — the active window highlight border is now automatically hidden when a window is fullscreened (Ctrl+Alt+Shift+F) and restored when exiting fullscreen
- Suppress border and drag ghost while paused — focus border hides when tiling is paused and restores on resume; drag ghost overlay is suppressed during pause and any visible ghost is dismissed on pause toggle

### Bug Fixes

- Fix unequal height distribution after minimizing — when a window is minimized from a multi-window column, stale `window_min_heights` constraints on sibling windows could pin them to their old sizes instead of redistributing evenly; `mark_minimized` now clears min-size constraints for all column siblings
- Fix drag ghost showing wrong height with minimized windows — ghost overlay now counts only visible (non-minimized) windows when computing slot height, and converts the raw slot index to a visible-window index so the ghost position is correct when minimized windows precede it
- Fix focus drift after animated commands (expel, move, etc.) — `SetWindowPos` during animation could trigger spurious OS focus events that silently overrode `focused_column`, causing subsequent resize/width commands to operate on the wrong column; the animation landing pass now re-syncs OS foreground to match the workspace's intended focus
- Skip focus border on ignored windows — `show_border` now checks that the target hwnd is actually managed before drawing, preventing the border from appearing on windows matched by ignore rules (e.g. Steam Friends List)
- Fix Slack/Spotify (and other Electron-style apps) overflowing their column boundaries — width-violation detection in the placement layer was using the global border-inset cache, which goes stale when an app changes how it renders its client frame at runtime. The frame-vs-frame math `actual_w > requested + cached_insets` then silently cancelled out, the violation was never reported, and the column was never widened. Detection now compares `DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS)` (the actual visible content rect) directly against the layout engine's requested rect, immune to cache staleness, and evicts the stale insets on any mismatch so the next `SetWindowPos` re-queries DWM
- Add symmetric height-violation detection — windows that enforce a minimum height (e.g. Spotify, large media players) are now detected and propagated to the layout engine via a new `window_min_heights` map; the layout engine pins min-height windows to their minimum and distributes the remainder among flexible windows by weight, with the last window honoring its own minimum even when rounding eats pixels. Replaces the per-frame "Min-size fixup" band-aid that ran for ≥2-window columns and never informed the layout engine
- Apply size-violation corrections on the same frame — after detection propagates min-width/min-height constraints, `apply_layout` now triggers a single guarded re-apply (via `reapplying_after_violation`) so the corrected layout lands on the current frame instead of waiting for the next user-triggered event. Inner re-apply errors propagate to the caller so daemon-paused states from a timed-out worker can't hide behind a successful outer apply
- Display-change handler now also clears `min_heights` alongside `min_widths` — height constraints learned under one DPI/theme metric set could otherwise survive into the next state and distort intra-column distribution
- Fix Chromium/Electron/Firefox windows getting stuck with broken internal viewports after rapid column scrolling — Beeper, Slack, Spotify, TradingView, Zen and similar apps previously required an app restart to recover their render target size after a rapid Ctrl+Alt+H/L burst. Root cause was a DirectComposition swap-chain desync: animation frames used `SWP_ASYNCWINDOWPOS`, the target app's UI thread coalesced the rapid position messages, and the final sync landing pass arrived with the same rect as the last async frame so the compositor saw "no size change" and never rebuilt its swap chain. The sync landing pass now sends a one-frame `(w-1 → w)` `SetWindowPos` pair for windows matching `Chrome_WidgetWin_1` / `MozillaWindowClass` / `CASCADIA_HOSTING_WINDOW_CLASS`, forcing a real size delta the compositor observes and rebuilds around. Re-checks `IsWindow` and the class name between the pair so HWND recycling cannot resize an unrelated window; logs a warning if the restore leg fails (the next apply will correct the 1px strand)
- Fix cascading full retiles during heavy background CPU load (`cargo build`, `cargo test`, any all-cores workload) — `EVENT_OBJECT_LOCATIONCHANGE` fires for many reasons besides actual movement (Z-order changes, DWM composition, focus shuffles, DPI nudges, app-internal size adjustments). The daemon treated all of them as user-initiated moves and snapped the window back via a full `apply_layout`, retiling every window on the active workspace. `AppState` now records the layout rect applied to each window in `last_placed_layout_rects`; the `MovedOrResized` handler queries the window's current visible bounds via `DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS)` and short-circuits the snap-back when actual and expected rects match within 20 px, so only genuine displacements trigger a retile
- Fix off-workspace windows being yanked onto the active workspace during transient apply timeouts — when the apply-layout worker exceeded its timeout (common under heavy background CPU), the recovery pass called the panic-grade `uncloak_all_visible_windows()`, which enumerates every top-level window on the desktop and moves any parked at the `MoveOffScreen` sentinel back to the primary monitor. That routinely dragged e.g. Spotify on workspace 2 onto workspace 1. `run_visibility_recovery_pass` now keeps only the scoped operations (`restore_windows_moved_offscreen` on managed window IDs, `uncloak_all_managed_windows`); the panic-grade sweep is reserved for the crash/panic hooks where it belongs. `APPLY_LAYOUT_TIMEOUT` is also raised from 1500 ms to 5000 ms so transient CPU stalls don't trigger the recovery path in the first place
- Fix stranded min-size constraints when column composition changes during a daemon stall — the constraint clear on `add_window` / `insert_at` / `remove_window` was eager, so if the subsequent `apply_layout` hit its timeout the column stayed with constraints cleared until the next user event re-triggered detection. Constraint clears now go through a new `pending_min_size_clears` set on `Workspace` drained at the start of both `apply_layout` and `send_animation_frame`; a timed-out worker can no longer leave the column in a stale state. Pending entries are pruned on actual window removal so the set cannot retain references to dead windows
- Fix inflated `min_height` values (2-window columns becoming 75/50 instead of 50/50) detected during CPU pressure — the sync landing pass queried `DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS)` immediately after `SetWindowPos`, but under load the target thread hadn't yet processed `WM_SIZE` so DWM returned stale pre-resize bounds. The violation detector then recorded the oversized rect as a permanent `min_height` constraint, breaking subsequent layouts. A `DwmFlush()` now blocks for one vsync between placement and query, forcing the compositor to present a frame incorporating the new positions; an additional 1.5× sanity cap skips any violation where the reported dimension exceeds the requested size by more than 50 % (almost always a stale-bounds read) and logs a warning
- Fix `cargo test --all` disrupting a live daemon — two platform-layer tests exercised real Win32 APIs against the desktop (`test_uncloak_all_visible_windows_no_panic` called the panic-recovery sweep; `test_uncloak_all_managed_with_invalid_ids` passed literal HWND values that could collide with a real window and move it). Both are now marked `#[ignore]` with an explanatory message; run with `cargo test -- --ignored` for panic-safety verification
- Skip shell-cloaked windows at the `Created` event — suspended UWP frames and windows on other virtual desktops are valid HWNDs with `WS_VISIBLE` but no rendered content. New `is_window_shell_cloaked(WindowId)` wrapper exposed from `platform_win32` keeps them out of the managed set

## 0.1.7

### Bug Fixes

- Fix window duplication across workspaces on config reload — `enumerate_and_add_windows` now checks all workspaces (including inactive ones) before inserting, preventing windows on non-active workspaces from being duplicated onto the active workspace

## 0.1.6

### Improvements

- Extract `workspace_placements()` helper in command handler — deduplicate two identical 11-line blocks in workspace-switch animation code into a single reusable method
- Extract `clear_drag_placeholder()` helper in drag module — deduplicate two identical global placeholder cleanup loops into a single method
- Prune orphaned `window_managed_at` entries — evict tracking entries for windows no longer managed in any workspace, preventing unbounded map growth over long daemon sessions
- Async window positioning during animation — add `SWP_ASYNCWINDOWPOS` to animation frames so hung/unresponsive windows don't stall the vsync-driven animation loop; landing passes remain synchronous for precise final placement. Width violation detection is deferred to the landing pass to prevent false min-width constraints from stale `GetWindowRect` data

## 0.1.5

### Features

- Disable Windows 11 Snap Layouts for tiled windows — removes `WS_MAXIMIZEBOX` from managed tiled windows to prevent edge-drag snapping and the snap layout flyout; style is restored when windows float, leave management, or the daemon exits. Enabled by default (`disable_snap_layouts = true`), opt-out via config or Settings UI
- Allow maximize on tiled windows — clicking the maximize button on a tiled window now lets it maximize normally instead of being snapped back; the next layout operation (e.g., window create/destroy) restores it to tiled position
- DPI-aware gap and border scaling — gaps, outer gaps, and border widths are now automatically scaled per-monitor based on DPI (e.g., `gap = 10` renders as 20px on a 200% DPI display), so spacing appears physically consistent across mixed-DPI setups
- `maximize_column` command (`Ctrl+Alt+M`) — toggle the focused column to fill the viewport width; invoke again to restore original width
- Scroll wheel navigation — hold a configurable modifier (default Ctrl+Alt) and scroll the mouse wheel to cycle focus between windows linearly across columns
- `focus_next` / `focus_prev` commands — traverse windows top-to-bottom within columns, then left-to-right across columns, wrapping around at boundaries
- Configurable scroll modifier in the Hotkeys tab, scroll up/down command mapping in the Gestures tab
- Battery-aware animation toggle — animations automatically disable on battery power or when Windows power saver is active; restored when plugging back into AC
- `center_column` command (`Ctrl+Alt+C`) — centers the focused column in the viewport with smooth scroll animation, regardless of centering mode
- `center_past_edges` layout option — allows center-column to scroll past content boundaries, truly centering first/last columns with empty space on the sides
- CLI `center-column` subcommand for scripting

### Bug Fixes

- Fix window stuck at full viewport width after app fullscreen — viewport-sized width violations (from video players or apps entering their own fullscreen) are now ignored, and maximized tiled windows are restored before placement to prevent SetWindowPos from being silently ignored
- Fix hotkey registration failure cascading to all hotkeys — partial registration now succeeds with warnings instead of the all-or-nothing policy that disabled all shortcuts when any single hotkey was claimed by another app
- Fix `toggle_fullscreen` not animating the transition when entering or exiting fullscreen
- Fix rapid mouse scrolling causing focus border to flicker between windows — physical mouse wheel events are now distinguished from touchpad-injected events via the LLMHF_INJECTED flag
- Harden EVENT_OBJECT_FOCUS handling — verify the window is actually the foreground window before emitting a Focused event, preventing spurious focus switches during scroll

## 0.1.4

### Features

- Windows High Contrast mode support — detects contrast themes and overrides the active border color with the system highlight color (`COLOR_HIGHLIGHT`), matching native Windows behavior
- Settings UI high contrast notice — InfoBar in Appearance tab indicates when border color is overridden; color picker is disabled. Live detection via `forced-colors` media query
- Settings UI forced-colors CSS — full high contrast stylesheet using system colors (Canvas, CanvasText, Highlight, etc.) for sidebar, cards, inputs, and comboboxes
- Startup banner shows high contrast status when active

### Improvements

- Expanded built-in skip classes — WSLg `RAIL_WINDOW`, DWM `Ghost` hung-window placeholders, `#32770` standard dialogs, and `Chrome_RenderWidgetHostHWND` internal Electron render widgets are now ignored at the platform layer
- Additional built-in ignore executables — `CredentialUIBroker.exe` (Windows credential prompts) and `SnippingTool.exe` (screen capture overlay) are now ignored by default
- WM_DISPLAYCHANGE debounce — contrast theme switches fire multiple display change messages with intermediate work areas; a 500ms debounce timer now waits for changes to settle before reconciling monitors
- Reusable InfoBar component — WinUI 3 Fluent Design info bar with four severity variants (informational, success, warning, error) for use across the settings UI

### Bug Fixes

- Fix cumulative column shrinking on display/theme changes — `apply_min_width_constraints` no longer proportionally shrinks flexible columns; constrained columns are widened while others keep their width, preventing the irreversible narrowing cycle
- Fix layout gaps disappearing in high contrast mode — DWM paints visible borders in the normally-invisible frame area; border inset expansion is now skipped in high contrast to prevent adjacent windows from overlapping into gap space
- Fix tiling layout reset on contrast theme switch — HMONITOR handles change during theme transitions even when physical monitors don't; workspaces are now re-keyed by device name instead of being destroyed and recreated
- Fix MovedOrResized snap-backs during theme transitions — events are suppressed while display change is pending to prevent stale border metrics from causing incorrect window positions
- Invalidate animation worker placement cache on display change — prevents stale inset-expanded positions from surviving as cache hits after theme toggles

## 0.1.3

### Features

- Mica backdrop on settings WebView — Windows 11 Mica material shows through the settings window, matching the title bar appearance. Extends DWM frame into client area with transparent WebView2 rendering
- Live theme switching — settings window responds to system dark/light mode changes in real-time via `WM_SETTINGCHANGE`
- Dark mode tray menu — native context menu follows the system theme via `uxtheme.dll` `SetPreferredAppMode`

### Improvements

- Reduced motion detection — respects Windows "Show animations" accessibility setting; snaps windows instantly when disabled
- Smooth layout transition animations — animation worker drives all frames via DwmFlush vsync instead of blocking `apply_layout()` thread, eliminating contention between the two positioning paths
- Skip `SWP_FRAMECHANGED` on cached animation frames — avoids expensive per-window `WM_NCCALCSIZE` on every frame, significant improvement for XAML/Electron windows

### Bug Fixes

- Auto-retile previously-ignored windows on config reload — changing or removing an ignore rule now picks up unmanaged windows
- Fix new windows appearing invisible — animation worker now starts after window events that trigger scroll/layout transitions
- Fix `WM_CLOSE` lifecycle — now properly calls `DestroyWindow` before `PostQuitMessage`, preventing HWND leak
- Fix `apply_win11_theming` to explicitly set dark mode off (value 0) when in light mode, enabling correct theme toggle

## 0.1.2

### Features

- Border resize snap — dragging a window's border now snaps to width/height presets with animated ghost preview overlay
- DWM composition surface preservation — off-screen windows are cloaked via DWMWA_CLOAK to prevent content shifting when scrolling back into view after 10-15+ seconds

### Improvements

- Width violations (min-width enforcement) now detected and fed back from the animation worker, not just the landing pass
- Overlay window exposes `hwnd_raw()` + `reposition_overlay()` for low-latency vsync-aligned updates from animation threads
- `core_layout` crate refactored from monolithic 5200-line file into focused modules (`types`, `animation`, `column`, `workspace`)

### Bug Fixes

- Fix off-screen windows losing DWM composition surfaces after ~15 seconds, causing content to render at wrong offset when scrolled back
- Fix cloaked windows permanently disappearing when switching workspaces or scrolling with centering mode (orphan uncloak on prune)
- Fix empty placement list leaving previously cloaked windows invisible (uncloak all tracked on empty)
- Fix GLOBAL_CLOAKED mutex poison recovery — shutdown/panic cleanup no longer silently skips uncloaking
- Reduce lock contention: Win32 cloak/uncloak calls happen after releasing GLOBAL_CLOAKED mutex
- Remove unused `IsWindow` import from border module
- Remove dead `offscreen_buffer` field from `PlatformConfig`

## 0.1.1

### Features

- Multi-workspace support — up to 9 workspaces per monitor with hotkeys to switch (`Ctrl+Alt+1-9`) and move windows (`Ctrl+Alt+Shift+1-9`)
- Animated workspace switching with continuous vertical scroll (both old and new workspaces scroll in unison)
- Alt+Tab to off-workspace windows triggers animated workspace switch
- Workspace state persisted across daemon restarts
- Tray tooltip shows active workspace number

### Improvements

- DeferWindowPos batching for atomic window repositioning (smoother animations, single DWM recomposition per frame)
- Layout transition animation sends a final frame at t=1.0 to ensure windows land at exact positions
- Minimum window size detection — windows that enforce a minimum height (e.g., Telegram) no longer overlap neighbors in stacked columns

### Bug Fixes

- Fix scroll offset clamping after minimize — cancel stale animations and clamp to non-negative values
- Fix duplicate WindowID check in `insert_column_at` preventing cross-monitor drag corruption
- Fix fullscreen placements emitting offscreen moves for minimized floating windows
- Fix `equalize_column_widths` animation overwriting reclamped scroll offset
- Fix invalid HDWP use after `DeferWindowPos` failure (Win32 already frees the handle)
- Fix `EndDeferWindowPos` failure now falls back to individual `SetWindowPos` calls
- Fix min-size fixup pass skipping windows that failed positioning
- Clear stale placement cache when placements list is empty
- Fix `focus_new_windows` not updating `focused_monitor` and `previous_focused_hwnd` for floating windows
- Clear `previous_focused_hwnd` on window Destroyed/Hidden events to prevent stale focus
- Fix `MoveToWorkspace` floating using canonical workspace rect instead of Win32 lookup, with rollback on failure
- Fix drag target workspace verified before removing window from source, preventing window loss
- Fix `snap_back_tiled` using drag's source workspace index instead of active workspace index
- Fix floating window coordinate clamping to target work area on cross-monitor move
- Fix monitor removal migration using proper source→target coordinate translation (source info still available)
- Preserve minimized state for windows migrated during monitor removal
- Rename settings UI labels from "Move to WS N" to "Move to Workspace N"

## 0.1.0

Initial release — a scrollable tiling window manager for Windows.

### Features

- Scroll-first tiling with horizontal strip layout and vsync-aligned smooth scrolling
- Multi-monitor workspaces with monitor-aware focus and window movement
- Global hotkeys with `Ctrl+Alt` base modifier and live config reload
- Touchpad gestures with configurable swipe actions
- Drag-and-drop column reorder and Shift+drag window merging
- Width and height presets with column/row equalization
- Floating and fullscreen toggles per window
- Window rules — match by class, title, or executable to tile, float, or ignore
- WebView-based settings GUI accessible from the system tray
- Session persistence — workspace state survives daemon restarts
- System tray with pause, reload, settings, diagnostics, and quick toggles
- Built-in diagnostics via `leopardwm-cli doctor`
- Safe mode for troubleshooting (`--safe-mode`)
- Autostart via Registry (`leopardwm-cli autostart enable`)
- Built-in ignore rules for system dialogs (SmartScreen, UAC, Windows Installer)
