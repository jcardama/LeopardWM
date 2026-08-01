# Security Policy

## Scope

LeopardWM is a local desktop window manager. It:

- **Is predominantly local** — window management, IPC, config, and logs stay on the machine. The only automated network request in LeopardWM's code is an optional outbound HTTPS update check (see [Automatic Update Check](#automatic-update-check))
- **Has no telemetry or data collection** — config, logs, state, crash reports, and window metadata are never transmitted
- **Communicates via local named pipes** — it prefers a user-scoped `\\.\pipe\leopardwm_<scope>` name and falls back to the legacy `\\.\pipe\leopardwm` name when no scope is available. The pipe's access control is described in [Named Pipe Security](#named-pipe-security)
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
| Opt-out | `behavior.check_for_updates = false` in config. The setting takes effect when the daemon starts, so restart the daemon after changing it; when false the update-check thread is never spawned. |
| Side effect | The HTTPS request discloses the source IP address to GitHub |

Confirmed absent from LeopardWM's own code: telemetry, analytics, remote logging, crash upload, and any second network client. Crash reports are written to local files only. The settings UI hosts the Microsoft Edge WebView2 runtime in-process and supplies it only local embedded HTML via `with_html`; it does not navigate to a remote URL. The Edge runtime's own network behavior is governed by Microsoft.

## Security-Relevant Win32 APIs

| API | Purpose |
|-----|---------|
| `SetWindowsHookEx` (`WH_KEYBOARD_LL` / `WH_MOUSE_LL`) | Global hotkey matching; optional focus-follows-mouse and touchpad gesture detection |
| `SetWinEventHook` | Window lifecycle events (create, destroy, focus, minimize) |
| `SetWindowPos` / `DeferWindowPos` | Window positioning (tiling layout) |
| `DwmSetWindowAttribute` | Window border colors, transitions, and cloaking |
| `EnumWindows` / `GetWindowTextW` / `GetClassNameW` | Window enumeration and metadata |
| Named pipes (async) | Local IPC between CLI and daemon |
| `ShellExecuteW` | Open the config file, log directory, releases page, and user-selected settings links |
| Registry (`HKCU\Software\Classes\AppUserModelId\jcardama.LeopardWM` and `...\jcardama.LeopardWM.Watchdog`) | AppUserModelID and display name written at daemon and watchdog startup to enable toast notifications |
| Registry (`HKCU\...\Run`) | Per-user auto-start entry when the user enables auto-start |

## Permission Model

- The daemon runs with the privileges of the user who started it
- It attempts to reposition windows owned by processes at the same or lower integrity level, hiding them by moving them off-screen; protected or ACL-denied windows are skipped. DWM cloaking applies only to LeopardWM's own windows
- It cannot manage windows from higher-integrity (including elevated) processes unless itself running at a sufficient integrity level
- Named pipe access is limited to local clients; when the explicit descriptor is built, it grants the current user and SYSTEM access

## Threat Model

### Attack Surface

LeopardWM's attack surface is minimal by design:

| Vector | Exposure | Worst Case |
|--------|----------|------------|
| Named pipe IPC | Normally local processes under the current user's SID and SYSTEM; remote clients are rejected | Malicious same-user IPC commands rearrange windows or stop the daemon |
| Low-level keyboard hook | User's keyboard | Configured hotkeys are intercepted; no privilege escalation |
| WinEvent hooks | Passive observation | Receives window events; cannot inject or modify them |
| Low-level mouse hook | Focus-follows-mouse and gesture detection | Matched modifier+wheel gestures are consumed and do not reach the foreground application; other mouse input passes through unmodified |
| Config file | Local filesystem | Malformed config causes fallback to defaults; no code execution |
| Outbound update check | Optional HTTPS GET to GitHub Releases API | Source IP address and LeopardWM version disclosed to GitHub |

### What LeopardWM Cannot Do

- **No inbound network listener** — no TCP, UDP, or HTTP listener; it accepts only local named-pipe connections. It has no telemetry, analytics, remote logging, or crash upload, and performs no automatic update download or install
- **No transmission of local user data** — config, logs, state, crash reports, and window metadata are never sent over the network. The optional update check sets only `User-Agent` and `Accept` application headers; standard HTTP, DNS, and TLS connection metadata and the source IP are also disclosed, but no local application data is included.
- **No code execution from config** — config values are data (strings, numbers, booleans); no eval, scripting, or plugin loading
- **No privilege escalation** — runs at the invoking process's integrity level and cannot elevate itself
- **No inter-process injection** — does not inject DLLs, modify process memory, or hook into other applications' code

### Named Pipe Security

The IPC server normally uses a user-scoped pipe name. It derives `\\.\pipe\leopardwm_<scope>` from `LEOPARDWM_PIPE_SCOPE` or `USERDOMAIN\USERNAME`; if neither produces a scope, it uses the legacy `\\.\pipe\leopardwm` name. Clients try the legacy name after a scoped name for compatibility.

- The server normally supplies an explicit security descriptor: `D:(A;;FA;;;<current-user-SID>)(A;;FA;;;SY)S:(ML;;NW;;;ME)`. Its DACL grants full access to the current user's SID and SYSTEM, and its mandatory label is Medium integrity with no-write-up.
- That Medium label lets a non-elevated client for the same user connect when an elevated daemon created the pipe.
- If construction of the descriptor fails, the server creates the pipe with default Windows security attributes.
- Remote clients are rejected: the server uses Tokio's default pipe options, which set `PIPE_REJECT_REMOTE_CLIENTS`, and never override it.
- There is no authentication protocol beyond the pipe's Windows access control; a local process running under the current user can connect.
- Commands are limited to the `IpcCommand` enum — the daemon rejects malformed messages.
- Maximum message size is enforced (`MAX_IPC_MESSAGE_SIZE`).
- The first instance is created with the first-pipe-instance flag, so another process cannot squat the name; subsequent instances are created per connection.

**Risk**: A malicious local process running as the same user could send IPC commands to rearrange windows or stop the daemon. This is equivalent to the attacker already having access to the user's desktop, so it does not represent a privilege boundary crossing.

### Local Privilege Boundaries

- The daemon cannot reposition windows owned by higher-integrity (including elevated Administrator) processes unless itself runs at a sufficient integrity level
- Running the daemon elevated is not recommended for daily use — it grants no additional features beyond managing higher-integrity windows
- The daemon does not create services, scheduled tasks, or system-wide (HKLM) registry keys. At startup, the daemon and watchdog write separate per-user AppUserModelID registry values under `HKEY_CURRENT_USER\Software\Classes\AppUserModelId` to enable toast notifications. Enabling auto-start also writes a per-user Run key at `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run`

---

## Privacy

### No Telemetry

LeopardWM sends **no telemetry**, analytics, crash reports, or usage statistics. Crash reports are written locally only. The optional update check (see [Automatic Update Check](#automatic-update-check)) is a version comparison only and is not telemetry.

### Local Data Only

The following data is stored only on your machine and is not uploaded:

| Data | Location | Content |
|------|----------|---------|
| Config file | `%APPDATA%\leopardwm\config\config.toml` (then `%USERPROFILE%\.config\leopardwm\config.toml` or `.\config.toml`) | User preferences (gaps, hotkeys, window rules) |
| Daemon log | `%LOCALAPPDATA%\leopardwm\logs\leopardwm-daemon.log` (fallback: `%TEMP%\leopardwm\logs\leopardwm-daemon.log`) | Operational messages (window events, errors) |
| Workspace state | `%APPDATA%\leopardwm\data\workspace-state.json` (fallback: `.\workspace-state.json`) | Window positions for session restore |
| Crash reports | `%TEMP%\leopardwm-crash-*.txt` | Timestamp, panic message and location, backtrace, version |
| Settings WebView2 profile | `%LOCALAPPDATA%\leopardwm\cache\webview2` (fallback: `%TEMP%\leopardwm-webview2`) | Persistent browser-profile data for the local settings UI |

### Log Contents

Daemon logs may contain:

- **Window titles** — e.g., "Document.docx - Microsoft Word". These are visible on your screen and taskbar.
- **Window class names** — e.g., "Chrome_WidgetWin_1". Technical identifiers, not user content.
- **Process executable names** — e.g., "notepad.exe". Visible in Task Manager.
- **Monitor device names** — e.g., "DISPLAY1". Hardware identifiers.

The global hotkey hook inspects key-down events in memory to match configured shortcuts and swallows matching keys; LeopardWM does not store or transmit keystrokes. It does not capture clipboard data, other applications' file contents, or browsing history. However, logs may contain any text a program exposes in its window title, including sensitive information such as document names, full URLs, email subjects, chat contents, passwords, or API keys. Review or redact logs before sharing them publicly, including in GitHub issues.

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
