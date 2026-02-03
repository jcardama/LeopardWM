# Security Policy

## Scope

LeopardWM is a local desktop window manager. It:

- **Uses only local Win32 APIs** — no network sockets, HTTP clients, or remote connections
- **Has no telemetry or data collection** — nothing leaves your machine
- **Communicates via local named pipe** (`\\.\pipe\leopardwm`) — the CLI and daemon talk over this pipe, which is accessible only to the local user session
- **Does not run as a service** — it runs as a regular user process
- **Does not require administrator privileges** — though it cannot manage elevated windows without elevation

> **Note:** Pipe names and config paths still use `leopardwm` internally. A full crate rename is separate future work.

## Win32 APIs Used

| API | Purpose |
|-----|---------|
| `SetWindowsHookEx` (WH_MOUSE_LL) | Touchpad gesture detection |
| `SetWinEventHook` | Window lifecycle events (create, destroy, focus, minimize) |
| `RegisterHotKey` | Global keyboard shortcuts |
| `SetWindowPos` / `DeferWindowPos` | Window positioning (tiling layout) |
| `DwmSetWindowAttribute` | Window border colors, cloaking |
| `EnumWindows` / `GetWindowInfo` | Window enumeration |
| Named pipes (async) | Local IPC between CLI and daemon |

## Permission Model

- The daemon runs with the privileges of the user who started it
- It can reposition and cloak windows owned by processes at the same or lower integrity level
- It cannot interact with windows from elevated (admin) processes unless itself running elevated
- Named pipe access is limited to the local machine

## Threat Model

### Attack Surface

LeopardWM's attack surface is minimal by design:

| Vector | Exposure | Worst Case |
|--------|----------|------------|
| Named pipe IPC | Local user session only | Malicious IPC commands rearrange windows or stop the daemon |
| Global hotkeys | User's keyboard | Hotkey conflicts with other apps (no escalation) |
| WinEvent hooks | Passive observation | Receives window events; cannot inject or modify them |
| Low-level mouse hook | Gesture detection | Observes mouse input for swipe gestures; does not block or modify |
| Config file | Local filesystem | Malformed config causes fallback to defaults; no code execution |

### What LeopardWM Cannot Do

- **No network access** — no sockets, HTTP clients, DNS lookups, or remote connections of any kind
- **No file exfiltration** — the daemon reads its own config file and writes logs/state; it does not read arbitrary user files
- **No code execution from config** — config values are data (strings, numbers, booleans); no eval, scripting, or plugin loading
- **No privilege escalation** — runs at user integrity level; cannot elevate itself
- **No inter-process injection** — does not inject DLLs, modify process memory, or hook into other applications' code

### Named Pipe Security

The IPC pipe (`\\.\pipe\leopardwm`) uses default Windows named pipe security:

- Accessible to the creating user's logon session
- No authentication protocol (any local process under the same user can connect)
- Commands are limited to the `IpcCommand` enum — the daemon rejects malformed messages
- Maximum message size is enforced (`MAX_IPC_MESSAGE_SIZE`)
- The pipe is single-instance; the daemon holds it exclusively

**Risk**: A malicious local process running as the same user could send IPC commands to rearrange windows or stop the daemon. This is equivalent to the attacker already having access to the user's desktop, so it does not represent a privilege boundary crossing.

### Local Privilege Boundaries

- The daemon cannot reposition windows owned by elevated (Administrator) processes unless itself running elevated
- Running the daemon elevated is not recommended for daily use — it grants no additional features beyond managing admin windows
- The daemon does not create or modify any system-wide resources (no services, scheduled tasks, or registry keys)

---

## Privacy

### No Telemetry

LeopardWM collects **no telemetry**, analytics, crash reports, or usage statistics. Nothing is transmitted over the network — the daemon has no networking code at all.

### Local Data Only

All data stays on your machine:

| Data | Location | Content |
|------|----------|---------|
| Config file | `%APPDATA%\leopardwm\config\config.toml` | User preferences (gaps, hotkeys, window rules) |
| Daemon log | stderr or `%TEMP%\leopardwm-daemon.log` | Operational messages (window events, errors) |
| Workspace state | `%APPDATA%\leopardwm\state.json` | Window positions for session restore |
| Crash reports | `%TEMP%\leopardwm-crash-*.txt` | Panic message, backtrace, version |

### Log Contents

Daemon logs may contain:

- **Window titles** — e.g., "Document.docx - Microsoft Word". These are visible on your screen and taskbar.
- **Window class names** — e.g., "Chrome_WidgetWin_1". Technical identifiers, not user content.
- **Process executable names** — e.g., "notepad.exe". Visible in Task Manager.
- **Monitor device names** — e.g., "DISPLAY1". Hardware identifiers.

Logs do **not** contain: passwords, API keys, file contents, browsing history, keystrokes, clipboard data, or any other sensitive information.

---

## Reporting a Vulnerability

If you discover a security vulnerability in LeopardWM, please report it responsibly:

1. **Do not open a public issue** for security vulnerabilities
2. Open a [private security advisory](https://github.com/jcardama/LeopardWM/security/advisories/new)
3. Include: description, reproduction steps, and impact assessment
4. You will receive an acknowledgment within 48 hours

We will coordinate disclosure and release a fix before any public announcement.

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.1.x (current dev) | Yes |
