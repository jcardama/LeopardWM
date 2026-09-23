# LeopardWM Architecture

## Crate Dependency Graph

```
daemon ──> core_layout (layout engine, platform-agnostic)
       ──> platform_win32 ──> core_layout (types only)
       ──> ipc (named-pipe protocol)

cli ──> ipc (sends commands to daemon)
    ──> platform_win32 (window queries, doctor command)

core_layout: zero external platform dependencies
ipc: zero crate dependencies (standalone protocol)
```

## Daemon Module Map

All source in `crates/daemon/src/`. All AppState fields are `pub(crate)` for multi-file `impl` blocks.

| File | Responsibility |
|---|---|
| `main.rs` | DaemonEvent enum, hotkey registration/dispatch, shutdown handler, main event loop, main() |
| `state.rs` | AppState struct, constructor, constants, basic accessors, drag type enums |
| `event_handler.rs` | handle_window_event (Created/Destroyed/Focused/Hidden/etc.), apply_focus_follows_mouse |
| `command_handler.rs` | handle_command — dispatches 26 IPC commands (focus, move, resize, scroll, config, etc.) |
| `helpers.rs` | Shared helpers: layout recalc, border management, config reload, persistence, window rules |
| `drag.rs` | Drag-and-drop: update_drag_hint, execute_window_merge, finalize_drag_merge, placeholder logic |
| `config.rs` | TOML config parsing, defaults, validation, HotkeyConfig, WindowRule, theme |
| `ipc_server.rs` | Named-pipe IPC server, client handler, forwarding threads, join_with_timeout |
| `startup.rs` | StartupInfo, ASCII banner, crash report, duplicate-instance detection |
| `tray.rs` | System tray icon + context menu (settings, restart, quit) |
| `settings/` | WebView-based settings UI (win32.rs creates WebView2 window, HTML/CSS/JS bundled) |
| `animation_worker.rs` | DwmFlush vsync frame loop, drives animated placement transitions |

## Data Flow

```
Win32 events (WinEventHook, message pump)
  -> mpsc channel -> DaemonEvent variants
    -> main event loop (main.rs)
      -> event_handler::handle_window_event  (window lifecycle)
      -> command_handler::handle_command      (IPC commands from CLI)
        -> mutate AppState
          -> helpers: recalc layout, update borders, persist state
            -> animation_worker: DwmFlush vsync frames
              -> platform_win32::placement: batched SetWindowPos calls
```

## Key Subsystems

### Border Overlay (`platform_win32/src/border.rs`)
- Custom per-pixel alpha rendering using SDF for anti-aliased rounded corners
- DWMWA_EXTENDED_FRAME_BOUNDS rect shrunk by 1px for outside borders to match visual window edge
- Visual corner radius = WIN11_CORNER_RADIUS (8.0) - 1.0 for outside, full 8.0 for inside
- Z-order: non-topmost, positioned just above target via GetWindow(target, GW_HWNDPREV)
- Animation frame handler checks `previous_focused_hwnd` — must clear when focus moves to unmanaged window
- Band optimization must include inner_r to capture corner rounding pixels

### Transient Window Suppression (3-layer defense)
Electron apps (Beeper, Slack) create/hide `Chrome_WidgetWin_1` popup windows every ~20-120s.

- **Layer 1 — Classify on hide**: `window_managed_at` tracks when each window was added. If managed <30s before hiding -> marked transient in `recently_hidden_hwnds` (5 min TTL)
- **Layer 2 — Suppress on create**: If HWND is in `recently_hidden_hwnds`, Created event is ignored
- **Layer 3 — Recover on focus**: If user focuses a suppressed window (e.g., tray restore), remove from suppression and re-dispatch as Created
- Spurious `EVENT_OBJECT_HIDE` from Electron: check `is_window_visible()` before removing managed windows

### Hotkey System
- **Base modifier: `Ctrl+Alt`** — avoids Windows 11 system, Game Bar, and Raycast conflicts
- Layered: base=focus, +Shift=move, +Win=monitor scope
- Monitor bindings use Comma/Period (not H/L) because Raycast claims all Win+*+L combos
- `Win+Ctrl+Escape` for panic revert (unchanged, uses Win)
- `RegisterHotKey` API: all-or-nothing — if any hotkey fails, all are unregistered
- Failure logging includes modifier names + `GetLastError` detail (os error 1409 = already registered)
- `parse_vk` supports Comma, Period, Bracket_Left, Bracket_Right in addition to letters/numbers/arrows
- F13–F24 may be used as modifiers (e.g. `F13+H`), not just triggers; F1–F12 may not. The keyboard hook swallows an F-key configured as a modifier and tracks its held-state itself (`HOOK_FN_HELD`), so it never reaches the focused app. `Modifiers::fn_mods` is a 12-bit mask (bit i = F13+i); `fn_mod_bit(vk)` maps a vk to its bit

**Conflict tables (do NOT use these as base modifiers):**
- Win+Alt: Game Bar (R, G, B, K), Windows 11 snap layouts (arrows), numbered slots — 12/33 failed in testing
- Win+Ctrl: Raycast registers L, F, P, 0/1/2/3, Shift+L, Alt+Shift+L

### Minimize Behavior
- `mark_minimized(hwnd)` keeps window in column but excludes from placement calculations
- `is_column_active()` returns false if all windows in column are minimized
- `compute_placements_animated` skips inactive columns, collapsing the gap
- `ensure_focused_visible_animated` JustInView mode clamps scroll when total_width shrinks

### Settings UI (`daemon/src/settings/`)
- WebView2-based (wry crate), auto-save with debounce (no Apply/Save/Cancel buttons)
- Settings save triggers config reload + hotkey re-registration
- WinUI 3 focus accent line: `<span class="input-wrap">` with `::after` pseudo-element + `overflow: hidden`
- Custom combobox component replaces native `<select>` in rules table
- Hotkeys tab: command read-only with human-readable labels, sorted by category, per-row reset icon
- DEFAULT_HOTKEYS and CMD_ORDER constants in JS must stay in sync with Rust `HotkeyConfig::default()`
