//! Opt-in Medium/High diagnostics validation host.
//!
//! Ordinary `cargo test` compiles this module and runs the pure tests. The
//! native host/fixture entrypoint is `#[ignore]` and fail-closed unless
//! `LEOPARDWM_DIAGNOSTICS_VALIDATION=1`.
//!
//! Gap: `skip_if_elevation_blocked` is `cfg(not(test))`. This host is not full
//! daemon startup/admission E2E. It feeds real `manage_block` /
//! `window_manage_block` into `AppState::note_elevation_block`, then serves
//! actual `handle_command(HealthCheck|QueryStatus)` through `run_ipc_server`.
//! Stop, PanicRevert, Subscribe, and every other command are rejected without
//! touching `AppState`.

use crate::config::Config;
use crate::ipc_server::run_ipc_server;
use crate::AppState;
use crate::DaemonEvent;
use leopardwm_core_layout::Rect;
use leopardwm_ipc::{preferred_pipe_name, IpcCommand, IpcResponse, PIPE_NAME};
use leopardwm_platform_win32::{manage_block, window_manage_block, ManageBlock, MonitorInfo};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::ProcessStatus::K32GetModuleFileNameExW;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowLongPtrW,
    GetWindowThreadProcessId, IsWindow, PeekMessageW, RegisterClassW, TranslateMessage,
    UnregisterClassW, GWL_STYLE, MSG, PM_REMOVE, WINDOW_STYLE, WNDCLASSW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP, WS_VISIBLE,
};

const OPT_IN_ENV: &str = "LEOPARDWM_DIAGNOSTICS_VALIDATION";
const ROLE_ENV: &str = "LEOPARDWM_DIAGNOSTICS_ROLE";
const RUN_DIR_ENV: &str = "LEOPARDWM_DIAGNOSTICS_RUN_DIR";
const EVIDENCE_PREFIX_ENV: &str = "LEOPARDWM_DIAGNOSTICS_EVIDENCE_PREFIX";
const OWN_HWND_ENV: &str = "LEOPARDWM_DIAGNOSTICS_OWN_HWND";
const TIMEOUT_ENV: &str = "LEOPARDWM_DIAGNOSTICS_TIMEOUT_SECS";
const PIPE_SCOPE_ENV: &str = "LEOPARDWM_PIPE_SCOPE";
const GAP: &str = "skip_if_elevation_blocked is cfg(not(test)); this host is not full daemon startup/admission E2E. It feeds manage_block/window_manage_block into note_elevation_block then handle_command(HealthCheck) through run_ipc_server.";
const FIXTURE_TITLE: &str = "LeopardWM diagnostics validation fixture";
const DEFAULT_TIMEOUT_SECS: u64 = 90;
const MAX_TIMEOUT_SECS: u64 = 120;
const LOCAL_DIAGVAL_PREFIX: &str = r"\\.\pipe\leopardwm_diagval_";

const HWND_MESSAGE: HWND = HWND(-3isize as *mut core::ffi::c_void);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FixtureIdentity {
    pub hwnd: u64,
    pub pid: u32,
    pub creation: u64,
    pub image: String,
    pub title: String,
}

pub(crate) fn opt_in_enabled(value: Option<&str>) -> Result<(), String> {
    match value {
        Some("1") => Ok(()),
        _ => Err(format!(
            "refusing diagnostics host: {OPT_IN_ENV} must be 1 (fail-closed)"
        )),
    }
}

pub(crate) fn exact_local_diagval_pipe(requested: &str) -> Result<String, String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Err("missing exact pipe; refusing to search fallbacks".into());
    }
    if requested == PIPE_NAME {
        return Err("refusing daily-driver pipe \\\\.\\pipe\\leopardwm".into());
    }
    let Some(rest) = requested.strip_prefix(LOCAL_DIAGVAL_PREFIX) else {
        return Err("refusing non-exact local \\\\.\\pipe\\leopardwm_diagval_<scope> pipe".into());
    };
    if rest.is_empty()
        || rest.contains('\\')
        || rest.contains('/')
        || rest != rest.to_ascii_lowercase()
        || !rest
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.')
    {
        return Err("diagval pipe remainder is not a safe exact local scope".into());
    }
    Ok(requested.to_string())
}

pub(crate) fn require_isolated_pipe(
    scope: Option<&str>,
    preferred: &str,
) -> Result<String, String> {
    let scope = scope.map(str::trim).unwrap_or("");
    if scope.is_empty() {
        return Err(format!(
            "{PIPE_SCOPE_ENV} is required; refusing unset scope"
        ));
    }
    if !scope.to_ascii_lowercase().starts_with("diagval_") {
        return Err("pipe scope must be a unique diagval_ run id".into());
    }
    let from_scope = leopardwm_ipc::scoped_pipe_name_for_user(scope);
    if preferred == PIPE_NAME || from_scope == PIPE_NAME {
        return Err("refusing daily-driver pipe \\\\.\\pipe\\leopardwm".into());
    }
    if preferred != from_scope {
        return Err("preferred pipe does not match unique diagval scope".into());
    }
    exact_local_diagval_pipe(preferred)
}

pub(crate) fn make_unique_scope(pid: u32, nsec: u128, tag: &str) -> Result<String, String> {
    let scope = format!("diagval_{pid}_{nsec}_{tag}");
    let pipe = leopardwm_ipc::scoped_pipe_name_for_user(&scope);
    exact_local_diagval_pipe(&pipe)?;
    if pipe == PIPE_NAME {
        return Err("unique scope collapsed to daily-driver pipe".into());
    }
    Ok(scope)
}

pub(crate) fn is_allowed_ipc_command(cmd: &IpcCommand) -> bool {
    matches!(cmd, IpcCommand::HealthCheck | IpcCommand::QueryStatus)
}

pub(crate) fn command_name(cmd: &IpcCommand) -> &'static str {
    match cmd {
        IpcCommand::HealthCheck => "HealthCheck",
        IpcCommand::WriteDiagnosticSnapshot { .. } => "WriteDiagnosticSnapshot",
        IpcCommand::QueryStatus => "QueryStatus",
        IpcCommand::Stop => "Stop",
        IpcCommand::PanicRevert => "PanicRevert",
        IpcCommand::Subscribe { .. } => "Subscribe",
        _ => "other",
    }
}

pub(crate) fn reject_if_disallowed(cmd: &IpcCommand) -> Option<IpcResponse> {
    if is_allowed_ipc_command(cmd) {
        None
    } else {
        Some(reject_response(cmd))
    }
}

pub(crate) fn identities_match(
    recorded_pid: u32,
    recorded_creation: u64,
    live_pid: u32,
    live_creation: u64,
) -> bool {
    recorded_pid != 0
        && recorded_creation != 0
        && recorded_pid == live_pid
        && recorded_creation == live_creation
}

pub(crate) fn images_match(left: &str, right: &str) -> bool {
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let normalize = |value: &str| {
        value
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(right)
}

pub(crate) fn require_complete_identity(
    pid: u32,
    creation: Option<u64>,
    image: Option<&str>,
) -> Result<(u32, u64, String), String> {
    let creation = creation.filter(|value| *value != 0).ok_or_else(|| {
        "fixture identity missing nonzero process creation time; refusing".to_string()
    })?;
    if pid == 0 {
        return Err("fixture identity missing nonzero pid; refusing".into());
    }
    let image = image
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "unavailable")
        .ok_or_else(|| "fixture identity missing image path; refusing".to_string())?;
    Ok((pid, creation, image.to_string()))
}

pub(crate) fn parse_fixture_identity(value: &Value) -> Result<FixtureIdentity, String> {
    let hwnd = value
        .get("hwnd")
        .and_then(Value::as_u64)
        .filter(|hwnd| *hwnd != 0)
        .ok_or_else(|| "fixture evidence missing nonzero hwnd".to_string())?;
    let pid = value
        .get("pid")
        .and_then(Value::as_u64)
        .filter(|pid| *pid != 0)
        .ok_or_else(|| "fixture evidence missing nonzero pid".to_string())? as u32;
    let creation = value
        .get("creation_filetime")
        .and_then(Value::as_u64)
        .filter(|creation| *creation != 0);
    let image = value.get("image").and_then(Value::as_str);
    let (pid, creation, image) = require_complete_identity(pid, creation, image)?;
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
        .unwrap_or(FIXTURE_TITLE)
        .to_string();
    Ok(FixtureIdentity {
        hwnd,
        pid,
        creation,
        image,
        title,
    })
}

pub(crate) fn classifications_agree(
    window: ManageBlock,
    process: ManageBlock,
) -> Result<ManageBlock, String> {
    if window != process {
        return Err(format!(
            "window and process classification disagree: window={} process={}",
            manage_block_label(window),
            manage_block_label(process)
        ));
    }
    Ok(window)
}

pub(crate) fn deadline_exceeded(elapsed: Duration, timeout: Duration) -> bool {
    elapsed >= timeout
}

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn require_opt_in_from_env() -> Result<(), String> {
    opt_in_enabled(env_opt(OPT_IN_ENV).as_deref())
}

fn run_dir() -> Result<PathBuf, String> {
    env_opt(RUN_DIR_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{RUN_DIR_ENV} is required"))
}

fn evidence_prefix() -> String {
    env_opt(EVIDENCE_PREFIX_ENV).unwrap_or_else(|| "host".into())
}

fn timeout_duration() -> Duration {
    let secs = env_opt(TIMEOUT_ENV)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn stop_path(dir: &Path) -> PathBuf {
    dir.join("stop")
}

fn write_json_atomic(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    let tmp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name()
            .ok_or_else(|| "json path missing file name".to_string())?
            .to_string_lossy(),
        std::process::id()
    ));
    fs::write(&tmp, bytes).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    if path.exists() {
        fs::remove_file(path).map_err(|e| format!("replace {}: {e}", path.display()))?;
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("publish {}: {e}", path.display())
    })
}

fn manage_block_label(block: ManageBlock) -> &'static str {
    match block {
        ManageBlock::No => "No",
        ManageBlock::HigherIntegrity => "HigherIntegrity",
        ManageBlock::Protected => "Protected",
    }
}

fn probe_own_integrity_rid() -> Option<u32> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let rid = token_integrity_rid(token);
        let _ = CloseHandle(token);
        rid
    }
}

unsafe fn token_integrity_rid(token: HANDLE) -> Option<u32> {
    let mut len = 0u32;
    let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut len);
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    GetTokenInformation(
        token,
        TokenIntegrityLevel,
        Some(buf.as_mut_ptr() as *mut _),
        len,
        &mut len,
    )
    .ok()?;
    let label = std::ptr::read_unaligned(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
    let sid = label.Label.Sid;
    if sid.is_invalid() {
        return None;
    }
    let count = *GetSidSubAuthorityCount(sid);
    if count == 0 {
        return None;
    }
    Some(*GetSidSubAuthority(sid, u32::from(count) - 1))
}

fn process_creation_filetime(pid: u32) -> Option<u64> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut create = windows::Win32::Foundation::FILETIME::default();
        let mut dummy = windows::Win32::Foundation::FILETIME::default();
        let ok = GetProcessTimes(handle, &mut create, &mut dummy, &mut dummy, &mut dummy).is_ok();
        let _ = CloseHandle(handle);
        if !ok {
            return None;
        }
        Some(((create.dwHighDateTime as u64) << 32) | u64::from(create.dwLowDateTime))
    }
}

fn process_image_path(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0u16; 1024];
        let len = K32GetModuleFileNameExW(Some(handle), None, &mut buffer);
        let _ = CloseHandle(handle);
        if len == 0 || len as usize >= buffer.len() {
            return None;
        }
        Some(String::from_utf16_lossy(&buffer[..len as usize]))
    }
}

fn current_identity() -> Result<(u32, u64, String), String> {
    let pid = std::process::id();
    let creation = process_creation_filetime(pid);
    let image = std::env::current_exe()
        .map_err(|e| format!("current_exe failed: {e}"))?
        .display()
        .to_string();
    require_complete_identity(pid, creation, Some(&image))
}

fn synthetic_monitor() -> Vec<MonitorInfo> {
    vec![MonitorInfo {
        id: 1,
        rect: Rect::new(0, 0, 1920, 1080),
        work_area: Rect::new(0, 0, 1920, 1040),
        is_primary: true,
        device_name: "DIAGVAL".to_string(),
        scale_factor: 1.0,
    }]
}

fn hwnd_to_u64(hwnd: HWND) -> u64 {
    hwnd.0 as usize as u64
}

fn hwnd_from_u64(hwnd: u64) -> HWND {
    HWND(hwnd as usize as *mut core::ffi::c_void)
}

unsafe extern "system" fn fixture_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn create_message_only_hwnd(class_w: &[u16], title_w: &[u16]) -> Result<HWND, String> {
    unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            windows::core::PCWSTR(class_w.as_ptr()),
            windows::core::PCWSTR(title_w.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            1,
            1,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        )
        .map_err(|e| format!("CreateWindowExW HWND_MESSAGE failed: {e}"))
    }
}

fn create_hidden_popup_hwnd(class_w: &[u16], title_w: &[u16]) -> Result<HWND, String> {
    unsafe {
        CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            windows::core::PCWSTR(class_w.as_ptr()),
            windows::core::PCWSTR(title_w.as_ptr()),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            None,
            None,
        )
        .map_err(|e| format!("CreateWindowExW hidden popup failed: {e}"))
    }
}

fn window_is_visible(hwnd: HWND) -> bool {
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        (style & WS_VISIBLE.0) != 0
    }
}

fn window_exists(hwnd: HWND) -> bool {
    unsafe { IsWindow(Some(hwnd)).as_bool() }
}

struct FixtureWindow {
    hwnd: HWND,
    class_w: Vec<u16>,
}

struct FixtureGuard(Option<FixtureWindow>);

impl Drop for FixtureGuard {
    fn drop(&mut self) {
        if let Some(fixture) = self.0.take() {
            destroy_fixture(fixture);
        }
    }
}

fn create_hidden_fixture() -> Result<FixtureWindow, String> {
    let pid = std::process::id();
    let class = format!("LeopardWMDiagValFixture_{pid}\0");
    let class_w: Vec<u16> = class.encode_utf16().collect();
    let title_w: Vec<u16> = format!("{FIXTURE_TITLE}\0").encode_utf16().collect();
    unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(fixture_wndproc),
            lpszClassName: windows::core::PCWSTR(class_w.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err("RegisterClassW failed".into());
        }
    }
    let hwnd = match create_message_only_hwnd(&class_w, &title_w) {
        Ok(hwnd) => hwnd,
        Err(_) => match create_hidden_popup_hwnd(&class_w, &title_w) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                let _ = unsafe { UnregisterClassW(windows::core::PCWSTR(class_w.as_ptr()), None) };
                return Err(error);
            }
        },
    };
    if window_is_visible(hwnd) {
        let _ = unsafe { DestroyWindow(hwnd) };
        let _ = unsafe { UnregisterClassW(windows::core::PCWSTR(class_w.as_ptr()), None) };
        return Err("fixture HWND was visible; refusing".into());
    }
    Ok(FixtureWindow { hwnd, class_w })
}

fn destroy_fixture(fixture: FixtureWindow) {
    let _ = unsafe { DestroyWindow(fixture.hwnd) };
    let _ = unsafe { UnregisterClassW(windows::core::PCWSTR(fixture.class_w.as_ptr()), None) };
}

fn fixture_pid(hwnd: HWND) -> Option<u32> {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    (pid != 0).then_some(pid)
}

fn pump_until(stop: &AtomicBool, deadline: Instant) {
    let mut msg = MSG::default();
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn write_fixture_evidence(
    dir: &Path,
    identity: &FixtureIdentity,
    visible: bool,
) -> Result<(), String> {
    write_json_atomic(
        &dir.join("fixture.json"),
        &json!({
            "hwnd": identity.hwnd,
            "hwnd_hex": format!("{:#x}", identity.hwnd),
            "pid": identity.pid,
            "creation_filetime": identity.creation,
            "image": identity.image,
            "title": identity.title,
            "visible": visible,
            "gap": GAP,
        }),
    )
}

fn verify_live_fixture(identity: &FixtureIdentity) -> Result<(), String> {
    let hwnd = hwnd_from_u64(identity.hwnd);
    if !window_exists(hwnd) {
        return Err("fixture HWND is not a live window".into());
    }
    if window_is_visible(hwnd) {
        return Err("fixture HWND is visible; refusing".into());
    }
    let live_pid = fixture_pid(hwnd).ok_or_else(|| "HWND pid unreadable; refusing".to_string())?;
    if live_pid != identity.pid {
        return Err(format!(
            "HWND pid {live_pid} != fixture pid {}; refusing arbitrary target",
            identity.pid
        ));
    }
    let live_creation = process_creation_filetime(live_pid)
        .ok_or_else(|| "fixture process creation unreadable; refusing".to_string())?;
    if !identities_match(identity.pid, identity.creation, live_pid, live_creation) {
        return Err("fixture pid/creation mismatch against live process; refusing".into());
    }
    let live_image = process_image_path(live_pid)
        .ok_or_else(|| "fixture image unreadable; refusing".to_string())?;
    if !images_match(&live_image, &identity.image) {
        return Err(format!(
            "fixture image mismatch: live={live_image} recorded={}",
            identity.image
        ));
    }
    Ok(())
}

fn load_fixture_identity(dir: &Path) -> Result<FixtureIdentity, String> {
    let path = dir.join("fixture.json");
    let raw = fs::read_to_string(&path).map_err(|e| {
        format!(
            "read {}: {e}; refusing missing fixture identity",
            path.display()
        )
    })?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|e| format!("fixture.json is not valid JSON: {e}"))?;
    parse_fixture_identity(&value)
}

fn wait_for_fixture(dir: &Path, timeout: Duration) -> Result<FixtureIdentity, String> {
    let start = Instant::now();
    let error_path = dir.join("fixture-error.json");
    loop {
        if error_path.exists() {
            let raw = fs::read_to_string(&error_path).unwrap_or_default();
            return Err(format!("fixture failed: {raw}"));
        }
        if dir.join("fixture.json").exists() {
            if let Ok(identity) = load_fixture_identity(dir) {
                return Ok(identity);
            }
        }
        if deadline_exceeded(start.elapsed(), timeout) {
            return Err("timed out waiting for complete fixture identity".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn admit_owned_fixture(state: &mut AppState, identity: &FixtureIdentity) -> Result<Value, String> {
    verify_live_fixture(identity)?;
    let window_verdict = window_manage_block(identity.hwnd);
    let process_verdict = manage_block(identity.pid);
    let noted = classifications_agree(window_verdict, process_verdict)?;
    let check = state.note_elevation_block(identity.hwnd, &identity.title, noted);
    Ok(json!({
        "hwnd": identity.hwnd,
        "pid": identity.pid,
        "creation_filetime": identity.creation,
        "image": identity.image,
        "title": identity.title,
        "window_manage_block": manage_block_label(window_verdict),
        "manage_block": manage_block_label(process_verdict),
        "noted": manage_block_label(noted),
        "elevation_check": format!("{check:?}"),
        "classifications_agree": true,
    }))
}

fn reject_response(cmd: &IpcCommand) -> IpcResponse {
    IpcResponse::error(format!(
        "diagnostics host rejects {}; only HealthCheck and QueryStatus are allowed",
        command_name(cmd)
    ))
}

async fn restricted_loop(
    mut state: AppState,
    mut rx: mpsc::Receiver<DaemonEvent>,
    dir: PathBuf,
    timeout: Duration,
) {
    let start = Instant::now();
    let mut ticks = tokio::time::interval(Duration::from_millis(50));
    loop {
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Some(DaemonEvent::IpcCommand { cmd, responder }) => {
                        let response = if let Some(rejected) = reject_if_disallowed(&cmd) {
                            rejected
                        } else {
                            state.handle_command(cmd)
                        };
                        let _ = responder.send(response);
                    }
                    Some(DaemonEvent::IpcSubscribe { responder, .. }) => {
                        drop(responder);
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            _ = ticks.tick() => {
                if stop_path(&dir).exists() || deadline_exceeded(start.elapsed(), timeout) {
                    break;
                }
            }
        }
    }
}

fn run_owned_fixture_thread(
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    timeout: Duration,
    pid: u32,
    creation: u64,
    image: String,
) -> JoinHandle<Result<(), String>> {
    std::thread::spawn(move || {
        let result = (|| {
            let fixture = create_hidden_fixture()?;
            let hwnd = fixture.hwnd;
            let _guard = FixtureGuard(Some(fixture));
            let live_pid =
                fixture_pid(hwnd).ok_or_else(|| "owned HWND pid unreadable".to_string())?;
            if live_pid != pid {
                return Err(format!("owned HWND pid {live_pid} != host pid {pid}"));
            }
            let identity = FixtureIdentity {
                hwnd: hwnd_to_u64(hwnd),
                pid,
                creation,
                image,
                title: FIXTURE_TITLE.to_string(),
            };
            verify_live_fixture(&identity)?;
            write_fixture_evidence(&dir, &identity, false)?;
            pump_until(&stop, Instant::now() + timeout);
            Ok(())
        })();
        if let Err(err) = &result {
            let _ = write_json_atomic(
                &dir.join("fixture-error.json"),
                &json!({ "error": err, "gap": GAP }),
            );
        }
        result
    })
}

struct HostCleanup {
    fixture_stop: Arc<AtomicBool>,
    fixture_thread: Option<JoinHandle<Result<(), String>>>,
    server: Option<tokio::task::JoinHandle<()>>,
}

impl HostCleanup {
    async fn finish(mut self) -> Result<(), String> {
        if let Some(server) = self.server.take() {
            server.abort();
            let _ = server.await;
        }
        self.fixture_stop.store(true, Ordering::SeqCst);
        let Some(thread) = self.fixture_thread.take() else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !thread.is_finished() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if !thread.is_finished() {
            return Err("fixture thread did not exit within bound".into());
        }
        match thread.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(err),
            Err(_) => Err("fixture thread panicked".into()),
        }
    }
}

async fn run_host_body(cleanup: &mut HostCleanup) -> Result<(), String> {
    require_opt_in_from_env()?;
    let dir = run_dir()?;
    let timeout = timeout_duration();
    let scope = env_opt(PIPE_SCOPE_ENV);
    let pipe = require_isolated_pipe(scope.as_deref(), &preferred_pipe_name())?;
    let (pid, creation, image) = current_identity()?;
    let prefix = evidence_prefix();
    let own_hwnd = env_opt(OWN_HWND_ENV).as_deref() == Some("1");

    if own_hwnd {
        cleanup.fixture_thread = Some(run_owned_fixture_thread(
            dir.clone(),
            cleanup.fixture_stop.clone(),
            timeout,
            pid,
            creation,
            image.clone(),
        ));
    }

    let identity = wait_for_fixture(&dir, timeout)?;
    if own_hwnd && identity.pid != pid {
        return Err("owned fixture pid does not match host pid".into());
    }

    let mut state = AppState::new_with_config(Config::default(), synthetic_monitor());
    let admission = admit_owned_fixture(&mut state, &identity)?;

    write_json_atomic(
        &dir.join(format!("{prefix}.json")),
        &json!({
            "role": "host",
            "pid": pid,
            "creation_filetime": creation,
            "image": image,
            "pipe": pipe,
            "oracle_integrity_rid": probe_own_integrity_rid(),
            "platform_integrity_rid": leopardwm_platform_win32::current_process_integrity(),
            "admission": admission,
            "gap": GAP,
        }),
    )?;

    let (tx, rx) = mpsc::channel::<DaemonEvent>(8);
    cleanup.server = Some(tokio::spawn(run_ipc_server(tx)));
    write_json_atomic(
        &dir.join(format!("{prefix}-ready.json")),
        &json!({
            "role": "host",
            "pid": pid,
            "creation_filetime": creation,
            "pipe": pipe,
            "ready": true,
            "gap": GAP,
        }),
    )?;
    restricted_loop(state, rx, dir.clone(), timeout).await;
    write_json_atomic(
        &dir.join(format!("{prefix}-exit.json")),
        &json!({ "role": "host", "pid": pid, "creation_filetime": creation, "exited": true, "gap": GAP }),
    )?;
    Ok(())
}

async fn run_host() -> Result<(), String> {
    let mut cleanup = HostCleanup {
        fixture_stop: Arc::new(AtomicBool::new(false)),
        fixture_thread: None,
        server: None,
    };
    let body = run_host_body(&mut cleanup).await;
    let cleanup_result = cleanup.finish().await;
    match (body, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(err), Ok(())) => Err(err),
        (Ok(()), Err(err)) => Err(err),
        (Err(err), Err(cleanup_err)) => Err(format!("{err}; cleanup: {cleanup_err}")),
    }
}

fn native_entry() -> Result<(), String> {
    require_opt_in_from_env()?;
    match env_opt(ROLE_ENV).as_deref() {
        Some("host") => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            rt.block_on(run_host())
        }
        Some("fixture") => Err(
            "standalone fixture role removed; the host owns the hidden HWND on OWN_HWND=1".into(),
        ),
        other => Err(format!("{ROLE_ENV} must be host, got {other:?}")),
    }
}

#[test]
#[ignore = "opt-in native diagnostics host; ordinary cargo test must not launch it"]
fn diagnostics_validation_native() {
    native_entry().unwrap_or_else(|err| panic!("{err}"));
}

#[test]
fn opt_in_is_fail_closed() {
    assert!(opt_in_enabled(None).is_err());
    assert!(opt_in_enabled(Some("")).is_err());
    assert!(opt_in_enabled(Some("0")).is_err());
    assert!(opt_in_enabled(Some("true")).is_err());
    assert!(opt_in_enabled(Some("1")).is_ok());
}

#[test]
fn unique_scope_never_selects_daily_driver_pipe() {
    let scope = make_unique_scope(4242, 1_700_000_000_000_000_000, "abcd12").unwrap();
    let pipe = leopardwm_ipc::scoped_pipe_name_for_user(&scope);
    assert_ne!(pipe, PIPE_NAME);
    assert!(require_isolated_pipe(Some(&scope), &pipe).is_ok());
    assert!(require_isolated_pipe(None, PIPE_NAME).is_err());
    assert!(require_isolated_pipe(Some(""), PIPE_NAME).is_err());
    assert!(require_isolated_pipe(Some("diagval"), PIPE_NAME).is_err());
    let user_scope = "acme_jose";
    let user_pipe = leopardwm_ipc::scoped_pipe_name_for_user(user_scope);
    assert!(require_isolated_pipe(Some(user_scope), &user_pipe).is_err());
    assert!(exact_local_diagval_pipe(r"\\.\pipe\foo_leopardwm_diagval_x").is_err());
    assert!(exact_local_diagval_pipe(r"\\server\pipe\leopardwm_diagval_x").is_err());
    assert!(exact_local_diagval_pipe(r"\\localhost\pipe\leopardwm_diagval_x").is_err());
    assert!(exact_local_diagval_pipe(r"\\.\pipe\leopardwm_diagval_").is_err());
    assert!(exact_local_diagval_pipe(PIPE_NAME).is_err());
}

#[test]
fn health_and_status_are_allowlisted_stop_panic_and_others_are_not() {
    assert!(is_allowed_ipc_command(&IpcCommand::HealthCheck));
    assert!(is_allowed_ipc_command(&IpcCommand::QueryStatus));
    assert!(!is_allowed_ipc_command(&IpcCommand::Stop));
    assert!(!is_allowed_ipc_command(&IpcCommand::PanicRevert));
    assert!(!is_allowed_ipc_command(&IpcCommand::Subscribe {
        events: Default::default()
    }));
    assert!(!is_allowed_ipc_command(&IpcCommand::Reload));
    assert_eq!(command_name(&IpcCommand::Stop), "Stop");
    assert_eq!(command_name(&IpcCommand::PanicRevert), "PanicRevert");
}

#[test]
fn restricted_host_rejects_stop_and_panic_without_dispatch() {
    let mut handled = Vec::new();
    let mut dispatch = |cmd: IpcCommand| {
        handled.push(command_name(&cmd));
        IpcResponse::Ok
    };
    let stop = reject_if_disallowed(&IpcCommand::Stop).expect("Stop must be rejected");
    let panic_revert =
        reject_if_disallowed(&IpcCommand::PanicRevert).expect("PanicRevert must be rejected");
    assert!(reject_if_disallowed(&IpcCommand::HealthCheck).is_none());
    assert!(reject_if_disallowed(&IpcCommand::QueryStatus).is_none());
    let _ = dispatch(IpcCommand::HealthCheck);
    match stop {
        IpcResponse::Error { message } => {
            assert!(message.contains("Stop"), "{message}");
            assert!(message.contains("HealthCheck"), "{message}");
        }
        other => panic!("expected error, got {other:?}"),
    }
    match panic_revert {
        IpcResponse::Error { message } => {
            assert!(message.contains("PanicRevert"), "{message}");
        }
        other => panic!("expected error, got {other:?}"),
    }
    assert_eq!(handled, ["HealthCheck"]);
}

#[test]
fn incomplete_or_mismatched_fixture_identity_is_refused() {
    assert!(require_complete_identity(0, Some(1), Some(r"C:\host.exe")).is_err());
    assert!(require_complete_identity(10, Some(0), Some(r"C:\host.exe")).is_err());
    assert!(require_complete_identity(10, None, Some(r"C:\host.exe")).is_err());
    assert!(require_complete_identity(10, Some(99), Some("")).is_err());
    assert!(require_complete_identity(10, Some(99), Some("unavailable")).is_err());
    assert!(require_complete_identity(10, Some(99), Some(r"C:\host.exe")).is_ok());
    assert!(parse_fixture_identity(&json!({"hwnd": 1, "pid": 2})).is_err());
    assert!(parse_fixture_identity(&json!({
        "hwnd": 0,
        "pid": 2,
        "creation_filetime": 3,
        "image": r"C:\host.exe"
    }))
    .is_err());
    let parsed = parse_fixture_identity(&json!({
        "hwnd": 11,
        "pid": 22,
        "creation_filetime": 33,
        "image": r"C:\tmp\host.exe",
        "title": FIXTURE_TITLE
    }))
    .unwrap();
    assert_eq!(parsed.pid, 22);
    assert_eq!(parsed.creation, 33);
    assert!(!identities_match(10, 99, 10, 100));
    assert!(!identities_match(0, 99, 0, 99));
    assert!(identities_match(10, 99, 10, 99));
    assert!(!images_match("", r"C:\tmp\host.exe"));
    assert!(images_match(r"C:\tmp\host.exe", r"c:/tmp/host.exe"));
}

#[test]
fn window_and_process_classification_must_agree() {
    assert_eq!(
        classifications_agree(ManageBlock::No, ManageBlock::No).unwrap(),
        ManageBlock::No
    );
    assert_eq!(
        classifications_agree(ManageBlock::HigherIntegrity, ManageBlock::HigherIntegrity).unwrap(),
        ManageBlock::HigherIntegrity
    );
    assert!(classifications_agree(ManageBlock::HigherIntegrity, ManageBlock::No).is_err());
    assert!(classifications_agree(ManageBlock::Protected, ManageBlock::HigherIntegrity).is_err());
}

#[test]
fn cleanup_requires_pid_and_creation_time() {
    assert!(identities_match(10, 99, 10, 99));
    assert!(!identities_match(10, 99, 10, 100));
    assert!(!identities_match(10, 99, 11, 99));
}

#[test]
fn deadline_is_inclusive_of_timeout() {
    assert!(!deadline_exceeded(
        Duration::from_secs(1),
        Duration::from_secs(2)
    ));
    assert!(deadline_exceeded(
        Duration::from_secs(2),
        Duration::from_secs(2)
    ));
    assert!(deadline_exceeded(
        Duration::from_secs(3),
        Duration::from_secs(2)
    ));
}

#[test]
fn native_entry_without_opt_in_does_not_start_host() {
    if env_opt(OPT_IN_ENV).as_deref() == Some("1") {
        return;
    }
    let err = native_entry().unwrap_err();
    assert!(err.contains("fail-closed"), "{err}");
}
