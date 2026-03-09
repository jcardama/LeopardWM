# Changelog

All notable changes to LeopardWM will be documented in this file.

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
