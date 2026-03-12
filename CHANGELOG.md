# Changelog

All notable changes to LeopardWM will be documented in this file.

## 0.1.4

### Improvements

- Expanded built-in skip classes — WSLg `RAIL_WINDOW`, DWM `Ghost` hung-window placeholders, `#32770` standard dialogs, and `Chrome_RenderWidgetHostHWND` internal Electron render widgets are now ignored at the platform layer
- Additional built-in ignore executables — `CredentialUIBroker.exe` (Windows credential prompts) and `SnippingTool.exe` (screen capture overlay) are now ignored by default

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
