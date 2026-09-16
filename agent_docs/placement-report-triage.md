# Placement-report triage (#104, #112)

This is an evidence intake record for local ticket LWM-210-01. It was reviewed on 2026-09-16 against published v0.2.9. It records reporter statements, log and code facts, and inferences for GitHub issues [#104](https://github.com/jcardama/LeopardWM/issues/104) and [#112](https://github.com/jcardama/LeopardWM/issues/112). No placement behavior is changed by this record.

## Evidence labels

| Label | Meaning |
|---|---|
| Reporter-stated | Claimed by the reporter; not independently confirmed here |
| Verified | Confirmed from attached logs, attached config, or current code |
| Inference | A hypothesis or interpretation, not a conclusion |

### Evidence sources

Log-derived rows in this record come from these GitHub issue attachments. They are not checked into the repository.

- First #104 daemon log (v0.2.8 resolution cycling, 2026-09-02): https://github.com/user-attachments/files/31746799/leopardwm-daemon_test.log
- Reporter config: https://github.com/user-attachments/files/31747005/leopardwm-config.txt
- Second #104 daemon log (Notion, 2026-09-04): https://github.com/user-attachments/files/31821712/leopardwm-daemon_test.log
- #112 has no attached log yet.

## #104 Windows Settings tiled off-screen at low resolution

**Status:** Reproduced by reporter on v0.2.7 and v0.2.8; awaiting v0.2.9 retest evidence. A retest request was posted 2026-09-15 by jcardama. No reporter reply as of 2026-09-16. Not reproduced locally in earlier sessions. A v0.2.8-era comment says the exact Settings 1280x720 case could not be reliably confirmed.

### Environment

| Item | Value | Evidence |
|---|---|---|
| Reporter | PhungNgocMinh | Reporter-stated |
| OS | Windows 11 25H2 build 26200.9168 | Reporter-stated |
| Primary monitor | `\\.\DISPLAY1`, native 1920x1080 at 150% DPI | Verified in daemon log |
| Second monitor | `\\.\DISPLAY2`, 1360x768, attached during part of the first log | Verified in daemon log |

### Reporter config

Verified from attached `leopardwm-config.txt`:

- `centering_mode = "center"`
- `width_presets` `[0.5, 0.75, 1.0]`
- `default_width_preset = 3`
- gap 20
- outer gaps `[10, 10, 0, 0]`
- `scroll_duration_ms = 400`
- `swap_chain_ghost_animation = true`
- one window rule for class `MozillaDialogClass`

### Resolution matrix

Reporter-stated on v0.2.8, 2026-09-02:

| Result | Resolutions |
|---|---|
| Fails | 1280x720, 1280x1024, 1280x600, 1024x768, 800x600 |
| Passes | 1600x900, 1366x768, 1360x768 |

Reporter-stated: changing the available scaling at those resolutions made no difference, and aspect ratio is not the determinant.

Verified from the daemon log: the reporter cycled exactly those resolutions between 15:50 and 16:20 on 2026-09-02, creating a fresh Settings window after each change (roughly 35 Settings creations).

### Trigger and workaround

Reporter-stated trigger: lower the resolution, open Windows Settings (class `ApplicationFrameWindow`, verified in log). The window lands mostly off the right edge and stays there. Focusing it later leaves part obscured.

Reporter-stated workaround: by minimizing and maximizing, or by clicking at an edge of the window (when mouse cursor indicates a resize action). That re-tiles it correctly.

### Other applications

- Microsoft Access (class `OMain`): reporter-stated same off-screen placement. Verified created three times in the log.
- Notion desktop (class `Chrome_WidgetWin_1`): reporter-stated same placement. Verified in the second log dated 2026-09-04. After Notion was first opened, the Settings failure became permanent even at native 1920x1080 and survived closing Notion, restarting the daemon, and restarting Windows (reporter-stated).

### Log evidence and limits

Verified: both logs are at the default info level. They contain no placement geometry, no native-minimum records, and no landing results, so the failing placement itself is invisible in them.

The only warnings are:

- repeated `AttachThreadInput` focus warnings for the Settings HWND
- two HigherIntegrity refusals

The second log also shows a daemon restart at 05:38 where a new instance briefly reported "Another leopardwm-daemon instance is already running" before the old one finished shutting down, plus "forwarding thread did not exit within timeout" warnings. These are shutdown-race noise, not placement evidence.

### Code facts and coverage gaps

Verified: native minimum sizes are runtime-only. `window_min_widths` / `window_min_heights` are `#[serde(skip)]` in `crates/core_layout/src/workspace/mod.rs:99-108`. Requested column widths plus scroll offset are persisted and restored (`crates/daemon/src/persistence.rs:190-203`, `crates/daemon/src/persistence.rs:269-299`).

Inference to test, not a conclusion: a placement failure that survives a daemon restart cannot be carried by a persisted native minimum. It must come from persisted requested width or scroll state, from Windows' own remembered window placement for that app, or from re-detection on every launch.

0.2.9 changes relevant to this report (from CHANGELOG 0.2.9):

- requested widths preserved under native minimums
- runtime display changes rescale column widths and keep the focused column in view
- transient oversize measurements need a repeated confirmation before becoming minimums
- column width saves track requested width

Deterministic coverage gaps (verified by code mapping): no test combines a viewport shrink with creation of a NEW window whose freshly measured native minimum exceeds its requested column width and then asserts that new window is visible. Existing tests cover the pieces separately:

| Location | What it covers |
|---|---|
| `crates/core_layout/src/tests.rs:2783-2792` | rescale 1920 to 1280 |
| `crates/daemon/src/tests.rs:6351-6371` | shrink keeps focused column visible |
| `crates/daemon/src/tests.rs:721-770` | width feedback on existing windows |
| `crates/core_layout/src/tests.rs:626-658` | minimum on focused column re-derives visibility |
| `crates/platform_win32/src/placement.rs:2075-2118` | oversize confirmation retry |

No test covers daemon restart persistence of a bad placement.

### Threshold hypothesis

Inference: with 150% DPI the logical viewport is about 853 px wide at 1280x720 and about 907 px at 1360x768. With outer gaps of 10+10 a full-width column is about 833 vs 887 logical px. A Settings native minimum width between those values would explain the exact 1280/1360 threshold.

The reporter's statement that changing scaling made no difference contradicts a purely logical-width threshold. This stays a hypothesis.

### Bounded next step (LWM-211-01)

1. Wait for the v0.2.9 retest. If it still fails, ask for one run with `behavior.log_level = "debug"` in the config and the full `%TEMP%\leopardwm-daemon.log` file (not `lwm collect-logs`, which only includes the last 100 lines), covering: set 1280x720, open Settings, then by minimizing and maximizing, or by clicking at an edge of the window (when mouse cursor indicates a resize action). That log will show the confirmed native-minimum record and the placement summary.
2. In parallel, write a red/green daemon test for shrink to 1280 then insert a new window whose native minimum exceeds its requested width and assert it is fully visible, using existing seams.
3. Only then choose a repair. Do not pull a repair into v0.2.10 without that test.

## #112 Whole-window drift (Vivaldi and Power BI Desktop)

**Status:** Reported on v0.2.8 on two identical machines; not reproduced locally; awaiting v0.2.9 retest and a pre-restart log. A retest request with `lwm collect-logs` instruction was posted 2026-09-15. No reply as of 2026-09-16. No log or diagnose output has been attached so far.

### Environment

| Item | Value | Evidence |
|---|---|---|
| Reporter | cavallaro88 | Reporter-stated |
| Affected apps | Vivaldi and Power BI Desktop | Reporter-stated |
| Machines | two identical machines | Reporter-stated |
| Version | v0.2.8 | Reporter-stated |

### Symptom and workaround

Reporter-stated: after a period of normal column scrolling a tiled window sits offset from its tile rect, sometimes partly outside the viewport. The window itself moves, not its content. The reporter distinguishes this from [#72](https://github.com/jcardama/LeopardWM/issues/72). Restarting the affected application restores correct placement. Intermittent, no deterministic recipe. A recipe is not required to keep the issue valid.

Already tried by reporter:

- `scroll_duration_ms = 0` reduced frequency but did not eliminate it
- `swap_chain_ghost_animation` on and off made no difference

Do not treat repeating those settings as the investigation.

### Animation-path inference

Inference: frequency dropping with animation disabled points at the animated path (asynchronous animation frames versus the synchronous landing) rather than the ghost overlay. A zero-duration scroll still goes through a landing pass, so a landing or stale-result defect remains possible.

### Diagnosability

Verified by code mapping. Default log level is info (`crates/daemon/src/config.rs:497-499`).

Synchronous `SetWindowPos` failures are collected but not logged at the platform call site (`crates/platform_win32/src/placement.rs:904-925`, `crates/platform_win32/src/placement.rs:949-988`). The daemon logs a warn for failed, unreadable, missing, or unconfirmed-parked landings (`crates/daemon/src/physical_placement.rs:647-666`), but its confirmation check for an ordinary placement only tests that an actual visible rect exists, not that it equals the requested rect (`crates/daemon/src/physical_placement.rs:654-661`), so a readable landing at the wrong rect produces no log line.

A stale animation result that arrives late is logged at debug only (`crates/daemon/src/layout_apply.rs:104-113`). The `InvalidatedCurrent` branch logs nothing (`crates/daemon/src/layout_apply.rs:115-125`). Confirmed native-minimum records are debug only (`crates/platform_win32/src/placement.rs:1284-1319`).

`lwm collect-logs` includes only the last 100 daemon log lines (`crates/cli/src/doctor.rs:327-341`).

Conclusion: a default-level log plus `lwm collect-logs` may expose failed or unreadable landings but cannot show or quantify readable wrong-rect drift; a useful capture still needs `behavior.log_level = "debug"`, the full daemon log file, and the last action before the displacement.

### Bounded next step (LWM-211-02)

1. Ask the reporter for that debug capture before restarting the app.
2. A small diagnostics-only candidate for v0.2.10, separately scoped, would be to log at warn when a confirmed landing rect differs from the requested rect in `crates/daemon/src/physical_placement.rs`, so drift becomes visible at the default level. Record this as a recommended follow-up, not implemented here.
3. No animation or placement behavior change until a capture or deterministic reproduction exists.

## Findings for other tickets

### Other reporter observations

- **Task Scheduler UI stretching when focus alternates while the daemon runs elevated.** Reporter observation. Unverified and out of scope for LWM-211-01. In the attached log the daemon was not elevated at that time: Task Scheduler (`MMCMainFrame`) and SQL Server Installation Center were refused with "HigherIntegrity, leaving it floating" (verified).
- **File Explorer showing half height during expel.** Reporter observation. Unverified and out of scope for LWM-211-01.

### LWM-210-02 candidate: Notion Command Search popup

Verified from the second #104 log: a window titled "Notion - Command Search" with class `Chrome_WidgetWin_1` was admitted as a tiled window twice (2026-09-04 05:34:41 and 05:34:46), each time removed again within seconds. Inference: it is Notion's quick-search palette rather than an application window; the log records only title and class.

Verified admission facts: the create/show admission path rejects any window with a non-null `GW_OWNER` (`crates/platform_win32/src/enumeration.rs:518-530` and `576-582`), so this popup must be unowned. `WS_POPUP` alone is not an exclusion (`crates/platform_win32/src/enumeration.rs:76-115`). The daemon's built-in dialog check treats a window as dialog-like only when it has `WS_CAPTION` but neither `WS_MINIMIZEBOX` nor `WS_MAXIMIZEBOX` (`crates/platform_win32/src/window_query.rs:161-182`), and an unruled dialog-like window is left unmanaged (`crates/daemon/src/event_handler.rs:599-637`). The built-in class skip list (`crates/platform_win32/src/enumeration.rs:675-705`) excludes `Chrome_RenderWidgetHostHWND` but not `Chrome_WidgetWin_1`. Notion's own main window uses that same class (verified in the same log), so excluding the class would also exclude the application window.

Inference: for the popup to be admitted it is unowned and either carries minimize or maximize box styles or has no caption at all. Which of those holds is not known from the log; the "Window created" info line records only title and class.

Evidence needed before an exclusion is implemented: the popup's style and extended style bits and its owner, captured while it is open. That can come from a reporter running with `behavior.log_level = "debug"` if a future diagnostic logs style bits at admission, or from a local reproduction with Notion installed. No such capture exists as of 2026-09-16. A title-based built-in exclusion is not proposed because it would be app-specific and fragile.

Interim workaround (Verified from the config schema at `crates/daemon/src/config.rs:521-557` and `596-607`): a user window rule matching class `Chrome_WidgetWin_1` and title regex `^Notion - Command Search$` with `action = "ignore"`:

```toml
[[window_rules]]
match_class = "Chrome_WidgetWin_1"
match_title = "^Notion - Command Search$"
action = "ignore"
```

## What this record does not establish

- No local reproduction of #104 or #112.
- No v0.2.9 reporter evidence.
- No live desktop testing performed for this record.
- No placement or animation behavior change.
