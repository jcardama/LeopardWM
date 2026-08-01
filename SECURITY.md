# Security Policy

## Scope

LeopardWM is a local desktop window manager. It:

- **Is predominantly local** — window management, IPC, config, and logs stay on the machine. The only network use is an optional outbound HTTPS update check (see [Automatic Update Check](#automatic-update-check))
- **Has no telemetry or data collection** — config, logs, state, crash reports, and window metadata are never transmitted
- **Communicates via local named pipe** (`\\.\pipe\leopardwm`) — the CLI and daemon talk over this pipe, which is accessible only to the local user session
- **Does not run as a service** — it runs as a regular user process
- **Does not require administrator privileges** — though it cannot manage elevated windows without elevation

> **Note:** Pipe names and config paths still use `leopardwm` internally. A full crate rename is separate future work.

## Automatic Update Check

When enabled (the default), the daemon performs a single outbound HTTPS GET to check whether a newer release exists on GitHub. Nothing is downloaded or installed automatically.

| Aspect | Detail |
|--------|--------|
| Destination | `https://api.github.com/repos/jcardama/LeopardWM/releases/latest` |
| Client | `ureq` (HTTPS) |
| Schedule | Once ~30 seconds after startup, then every 24 hours |
| Request headers | `User-Agent: LeopardWM/<version>` and `Accept: application/vnd.github+json` |
| Request body | None |
| Not sent | Config, logs, state, crash reports, window titles, process names, or any other user data |
| Response use | Only `tag_name` is retained for comparison |
| Behavior | Notification/check only — no binary download, no automatic install |
| Opt-out | `behavior.check_for_updates = false` in config; when false the update-check thread is never spawned |
| Side effect | The HTTPS request discloses the source IP address to GitHub |

Confirmed absent workspace-wide: telemetry, analytics, remote logging, crash upload, and any second network client. Crash reports are written to local files only.

## Security-Relevant Win32 APIs

| API | Purpose |
|-----|---------|
| `SetWindowsHookEx` (WH_MOUSE_LL) | Touchpad gesture detection |
| `SetWinEventHook` | Window lifecycle events (create, destroy, focus, minimize) |
| `RegisterHotKey` | Global keyboard shortcuts |
| `SetWindowPos` / `DeferWindowPos` | Window positioning (tiling layout) |
| `DwmSetWindowAttribute` | Window border colors, cloaking |
| `EnumWindows` / `GetWindowInfo` | Window enumeration |
| Named pipes (async) | Local IPC between CLI and daemon |
| `ShellExecuteW` | Open the config file, log directory, releases page, and settings links |
| Registry (`HKCU\...\Run`) | Per-user auto-start entry when the user enables auto-start |

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
| Outbound update check | Optional HTTPS GET to GitHub Releases API | Source IP address and LeopardWM version disclosed to GitHub |

### What LeopardWM Cannot Do

- **No inbound network listener** — does not accept connections; no telemetry, analytics, remote logging, or crash upload; no automatic update download or install
- **No transmission of local user data** — config, logs, state, crash reports, and window metadata are never sent over the network (the optional update check sends only the request headers described above, plus the unavoidable source IP)
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
- The daemon does not create services, scheduled tasks, or system-wide (HKLM) registry keys. Enabling auto-start writes a per-user Run key at `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`

---

## Privacy

### No Telemetry

LeopardWM collects **no telemetry**, analytics, crash reports, or usage statistics. The optional update check (see [Automatic Update Check](#automatic-update-check)) is a version comparison only and is not telemetry.

### Local Data Only

The following data is stored only on your machine and is not uploaded:

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
| 0.2.x | Yes |
