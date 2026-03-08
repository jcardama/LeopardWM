# Changelog

All notable changes to LeopardWM will be documented in this file.

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
