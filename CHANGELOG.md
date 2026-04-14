# Changelog

All notable changes to LeopardWM will be documented in this file.

## 0.1.8

### Improvements

- Hide focus border in fullscreen mode — the active window highlight border is now automatically hidden when a window is fullscreened (Ctrl+Alt+Shift+F) and restored when exiting fullscreen

### Bug Fixes

- Skip focus border on ignored windows — `show_border` now checks that the target hwnd is actually managed before drawing, preventing the border from appearing on windows matched by ignore rules (e.g. Steam Friends List)
- Fix Slack/Spotify (and other Electron-style apps) overflowing their column boundaries — width-violation detection in the placement layer was using the global border-inset cache, which goes stale when an app changes how it renders its client frame at runtime. The frame-vs-frame math `actual_w > requested + cached_insets` then silently cancelled out, the violation was never reported, and the column was never widened. Detection now compares `DwmGetWindowAttribute(EXTENDED_FRAME_BOUNDS)` (the actual visible content rect) directly against the layout engine's requested rect, immune to cache staleness, and evicts the stale insets on any mismatch so the next `SetWindowPos` re-queries DWM
- Add symmetric height-violation detection — windows that enforce a minimum height (e.g. Spotify, large media players) are now detected and propagated to the layout engine via a new `window_min_heights` map; the layout engine pins min-height windows to their minimum and distributes the remainder among flexible windows by weight, with the last window honoring its own minimum even when rounding eats pixels. Replaces the per-frame "Min-size fixup" band-aid that ran for ≥2-window columns and never informed the layout engine
- Apply size-violation corrections on the same frame — after detection propagates min-width/min-height constraints, `apply_layout` now triggers a single guarded re-apply (via `reapplying_after_violation`) so the corrected layout lands on the current frame instead of waiting for the next user-triggered event. Inner re-apply errors propagate to the caller so daemon-paused states from a timed-out worker can't hide behind a successful outer apply
- Display-change handler now also clears `min_heights` alongside `min_widths` — height constraints learned under one DPI/theme metric set could otherwise survive into the next state and distort intra-column distribution

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
