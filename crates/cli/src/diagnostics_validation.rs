//! Native diagnostics is opt-in only: connect to the exact scoped pipe without candidate fallback,
//! and verify the server identity before sending commands.

use crate::doctor::{blocked_windows_check, format_integrity_line, integrity_check, CheckResult};
use crate::ipc_client::{named_pipe_server_pid, parse_ipc_response_frame};
use leopardwm_ipc::{IpcCommand, IpcResponse, MAX_IPC_MESSAGE_SIZE, PIPE_NAME};
use serde_json::{json, Value};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

const OPT_IN_ENV: &str = "LEOPARDWM_DIAGNOSTICS_VALIDATION";
const ROLE_ENV: &str = "LEOPARDWM_DIAGNOSTICS_ROLE";
const RUN_DIR_ENV: &str = "LEOPARDWM_DIAGNOSTICS_RUN_DIR";
const EVIDENCE_PREFIX_ENV: &str = "LEOPARDWM_DIAGNOSTICS_EVIDENCE_PREFIX";
const PIPE_ENV: &str = "LEOPARDWM_DIAGNOSTICS_PIPE";
const EXPECTED_PID_ENV: &str = "LEOPARDWM_DIAGNOSTICS_EXPECTED_SERVER_PID";
const EXPECTED_CREATION_ENV: &str = "LEOPARDWM_DIAGNOSTICS_EXPECTED_SERVER_CREATION";
const TIMEOUT_ENV: &str = "LEOPARDWM_DIAGNOSTICS_TIMEOUT_SECS";
const GAP: &str = "skip_if_elevation_blocked is cfg(not(test)); this client talks to the restricted diagnostics host over an exact unique pipe and does not exercise full daemon admission.";
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const MAX_TIMEOUT_SECS: u64 = 120;
const LOCAL_DIAGVAL_PREFIX: &str = r"\\.\pipe\leopardwm_diagval_";

pub(crate) fn opt_in_enabled(value: Option<&str>) -> Result<(), String> {
    match value {
        Some("1") => Ok(()),
        _ => Err(format!(
            "refusing diagnostics client: {OPT_IN_ENV} must be 1 (fail-closed)"
        )),
    }
}

pub(crate) fn exact_pipe_to_open(requested: &str) -> Result<String, String> {
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

pub(crate) fn require_expected_identity(pid: u32, creation: u64) -> Result<(), String> {
    if pid == 0 {
        return Err("expected server pid must be nonzero before connecting".into());
    }
    if creation == 0 {
        return Err("expected server creation time must be nonzero before connecting".into());
    }
    Ok(())
}

pub(crate) fn server_identity_matches(
    expected_pid: u32,
    expected_creation: u64,
    live_pid: u32,
    live_creation: Option<u64>,
) -> Result<(), String> {
    require_expected_identity(expected_pid, expected_creation)?;
    let live_creation = live_creation
        .filter(|value| *value != 0)
        .ok_or_else(|| "connected server creation time unreadable; abandoning pipe".to_string())?;
    if live_pid == 0 || live_pid != expected_pid || live_creation != expected_creation {
        return Err(format!(
            "connected server pid/creation {live_pid}/{live_creation} != expected {expected_pid}/{expected_creation}; abandoning pipe"
        ));
    }
    Ok(())
}

pub(crate) fn require_health_info(response: &IpcResponse) -> Result<(), String> {
    match response {
        IpcResponse::HealthInfo { .. } => Ok(()),
        other => Err(format!("required HealthInfo missing; got {other:?}")),
    }
}

pub(crate) fn require_status_info(response: &IpcResponse) -> Result<(), String> {
    match response {
        IpcResponse::StatusInfo { .. } => Ok(()),
        other => Err(format!(
            "required QueryStatus StatusInfo missing; got {other:?}"
        )),
    }
}

pub(crate) fn daemon_integrity_from_health(health: Option<&IpcResponse>) -> Option<u32> {
    match health {
        Some(IpcResponse::HealthInfo {
            daemon_integrity, ..
        }) => *daemon_integrity,
        _ => None,
    }
}

pub(crate) fn render_integrity_pair(
    health: Option<&IpcResponse>,
    cli_rid: Option<u32>,
) -> (CheckResult, CheckResult) {
    (
        integrity_check("Daemon", daemon_integrity_from_health(health)),
        integrity_check("CLI", cli_rid),
    )
}

pub(crate) fn check_message(result: &CheckResult) -> &str {
    match result {
        CheckResult::Pass(msg) | CheckResult::Warn(msg) | CheckResult::Fail(msg) => msg,
    }
}

pub(crate) fn exchange_timed_out(elapsed: Duration, budget: Duration) -> bool {
    elapsed >= budget
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
    env_opt(EVIDENCE_PREFIX_ENV).unwrap_or_else(|| "client".into())
}

fn timeout_duration() -> Duration {
    let secs = env_opt(TIMEOUT_ENV)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);
    Duration::from_secs(secs)
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

async fn open_exact_pipe(pipe: &str, timeout: Duration) -> Result<NamedPipeClient, String> {
    let start = Instant::now();
    loop {
        match ClientOptions::new().open(pipe) {
            Ok(client) => return Ok(client),
            Err(err) => {
                let not_found = err.kind() == ErrorKind::NotFound
                    || err.raw_os_error() == Some(2)
                    || err.raw_os_error() == Some(231);
                if !not_found || start.elapsed() >= timeout {
                    return Err(format!(
                        "exact pipe {pipe} unavailable: {err}; not trying daily-driver or candidate fallback"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

async fn send_on_verified_pipe(
    pipe: &str,
    cmd: IpcCommand,
    expected_pid: u32,
    expected_creation: u64,
    timeout: Duration,
) -> Result<(u32, IpcResponse), String> {
    require_expected_identity(expected_pid, expected_creation)?;
    tokio::time::timeout(timeout, async {
        let client = open_exact_pipe(pipe, timeout).await?;
        let server_pid = named_pipe_server_pid(&client)?;
        server_identity_matches(
            expected_pid,
            expected_creation,
            server_pid,
            process_creation_filetime(server_pid),
        )?;
        let (reader, mut writer) = tokio::io::split(client);
        let request = serde_json::to_string(&cmd).map_err(|e| e.to_string())? + "\n";
        writer
            .write_all(request.as_bytes())
            .await
            .map_err(|e| format!("write command failed: {e}"))?;
        writer
            .flush()
            .await
            .map_err(|e| format!("flush command failed: {e}"))?;
        let mut reader = BufReader::new(reader).take((MAX_IPC_MESSAGE_SIZE + 1) as u64);
        let mut frame = Vec::new();
        let bytes_read = reader
            .read_until(b'\n', &mut frame)
            .await
            .map_err(|e| format!("read response failed: {e}"))?;
        if bytes_read == 0 {
            return Err("daemon disconnected before sending a response".into());
        }
        let response =
            parse_ipc_response_frame(&frame, MAX_IPC_MESSAGE_SIZE).map_err(|e| e.to_string())?;
        Ok((server_pid, response))
    })
    .await
    .map_err(|_| format!("pipe exchange timed out after {timeout:?}"))?
}

fn response_json(response: &IpcResponse) -> Value {
    serde_json::to_value(response).unwrap_or(Value::Null)
}

fn render_blocked(health: &IpcResponse) -> Result<CheckResult, String> {
    match health {
        IpcResponse::HealthInfo {
            elevation_blocked_records,
            elevation_blocked_windows,
            ..
        } => Ok(blocked_windows_check(
            elevation_blocked_records.as_deref(),
            elevation_blocked_windows,
        )),
        other => Err(format!(
            "blocked-window render requires HealthInfo, got {other:?}"
        )),
    }
}

async fn run_client() -> Result<(), String> {
    require_opt_in_from_env()?;
    if env_opt(ROLE_ENV).as_deref() != Some("client") {
        return Err(format!("{ROLE_ENV} must be client"));
    }
    let dir = run_dir()?;
    let pipe = exact_pipe_to_open(env_opt(PIPE_ENV).as_deref().unwrap_or(""))?;
    let expected_pid: u32 = env_opt(EXPECTED_PID_ENV)
        .ok_or_else(|| format!("{EXPECTED_PID_ENV} is required"))?
        .parse()
        .map_err(|_| format!("{EXPECTED_PID_ENV} is not a pid"))?;
    let expected_creation: u64 = env_opt(EXPECTED_CREATION_ENV)
        .ok_or_else(|| format!("{EXPECTED_CREATION_ENV} is required"))?
        .parse()
        .map_err(|_| format!("{EXPECTED_CREATION_ENV} is not a creation time"))?;
    require_expected_identity(expected_pid, expected_creation)?;
    let timeout = timeout_duration();
    let oracle = probe_own_integrity_rid();
    let platform = leopardwm_platform_win32::current_process_integrity();
    let prefix = evidence_prefix();

    let (server_pid, health) = send_on_verified_pipe(
        &pipe,
        IpcCommand::HealthCheck,
        expected_pid,
        expected_creation,
        timeout,
    )
    .await?;
    require_health_info(&health)?;
    let (server_pid_status, status) = send_on_verified_pipe(
        &pipe,
        IpcCommand::QueryStatus,
        expected_pid,
        expected_creation,
        timeout,
    )
    .await?;
    require_status_info(&status)?;
    if server_pid_status != server_pid {
        return Err("QueryStatus connected to a different server pid".into());
    }

    let (daemon_check, cli_check) = render_integrity_pair(Some(&health), platform);
    let blocked = render_blocked(&health)?;

    write_json_atomic(
        &dir.join(format!("{prefix}.json")),
        &json!({
            "role": "client",
            "pid": std::process::id(),
            "creation_filetime": process_creation_filetime(std::process::id()),
            "image": std::env::current_exe().ok().map(|p| p.display().to_string()),
            "pipe": pipe,
            "connected_server_pid": server_pid,
            "expected_server_pid": expected_pid,
            "expected_server_creation": expected_creation,
            "oracle_integrity_rid": oracle,
            "platform_integrity_rid": platform,
            "health": response_json(&health),
            "query_status": response_json(&status),
            "rendered": {
                "daemon": check_message(&daemon_check),
                "cli": check_message(&cli_check),
                "blocked": check_message(&blocked),
            },
            "gap": GAP,
        }),
    )?;
    write_json_atomic(
        &dir.join(format!("{prefix}-ready.json")),
        &json!({ "role": "client", "ready": true, "pipe": pipe, "gap": GAP }),
    )?;
    Ok(())
}

fn native_entry() -> Result<(), String> {
    require_opt_in_from_env()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(run_client())
}

#[test]
#[ignore = "opt-in native diagnostics client; ordinary cargo test must not launch it"]
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
fn missing_or_legacy_pipe_does_not_use_fallback() {
    assert!(exact_pipe_to_open("").is_err());
    assert!(exact_pipe_to_open(PIPE_NAME).is_err());
    let candidates = leopardwm_ipc::pipe_name_candidates();
    assert!(
        candidates.iter().any(|name| name == PIPE_NAME),
        "production candidates include the daily-driver fallback: {candidates:?}"
    );
    let exact = r"\\.\pipe\leopardwm_diagval_unit";
    assert_eq!(exact_pipe_to_open(exact).unwrap(), exact);
    assert!(exact_pipe_to_open(r"\\.\pipe\leopardwm_acme_jose").is_err());
    assert!(exact_pipe_to_open(r"\\.\pipe\foo_leopardwm_diagval_unit").is_err());
    assert!(exact_pipe_to_open(r"\\server\pipe\leopardwm_diagval_unit").is_err());
    assert!(exact_pipe_to_open(r"\\localhost\pipe\leopardwm_diagval_unit").is_err());
    assert!(exact_pipe_to_open(r"\\.\pipe\leopardwm_diagval_").is_err());
}

#[test]
fn commands_require_nonzero_server_identity_before_send() {
    assert!(require_expected_identity(0, 99).is_err());
    assert!(require_expected_identity(10, 0).is_err());
    assert!(require_expected_identity(10, 99).is_ok());
    assert!(server_identity_matches(10, 99, 10, Some(99)).is_ok());
    assert!(server_identity_matches(10, 99, 10, Some(100)).is_err());
    assert!(server_identity_matches(10, 99, 11, Some(99)).is_err());
    assert!(server_identity_matches(10, 99, 10, None).is_err());
    assert!(server_identity_matches(10, 99, 0, Some(99)).is_err());
}

#[test]
fn required_health_and_status_are_not_optional() {
    assert!(require_health_info(&IpcResponse::Ok).is_err());
    assert!(require_status_info(&IpcResponse::Ok).is_err());
    assert!(require_status_info(&IpcResponse::error("no")).is_err());
    let health = IpcResponse::HealthInfo {
        healthy: true,
        uptime_seconds: 1,
        total_windows: 0,
        monitors: 1,
        paused: true,
        thumbnail_register_balance: 0,
        elevation_blocked_windows: Vec::new(),
        daemon_integrity: Some(leopardwm_platform_win32::INTEGRITY_MEDIUM),
        elevation_blocked_records: Some(Vec::new()),
        native_swipes: None,
        daemon_log: None,
        unknown_config_keys: None,
    };
    assert!(require_health_info(&health).is_ok());
    assert!(require_status_info(&IpcResponse::StatusInfo {
        version: "0".into(),
        monitors: 1,
        total_windows: 0,
        uptime_seconds: 1,
    })
    .is_ok());
}

#[test]
fn bounded_frame_rejects_oversize_and_missing_newline() {
    assert!(parse_ipc_response_frame(&[b'x'; 8], 4).is_err());
    assert!(parse_ipc_response_frame(b"{}", 64).is_err());
    assert!(exchange_timed_out(
        Duration::from_secs(2),
        Duration::from_secs(2)
    ));
    assert!(!exchange_timed_out(
        Duration::from_secs(1),
        Duration::from_secs(2)
    ));
}

#[test]
fn missing_health_info_does_not_copy_cli_rid() {
    let cli = Some(leopardwm_platform_win32::INTEGRITY_HIGH);
    let (daemon, cli_check) = render_integrity_pair(None, cli);
    assert_eq!(
        daemon,
        CheckResult::Warn("Daemon integrity: unavailable".into())
    );
    assert_eq!(cli_check, CheckResult::Pass("CLI integrity: High".into()));
    let daemon_msg = check_message(&daemon);
    assert_eq!(daemon_msg, "Daemon integrity: unavailable");
    assert!(!daemon_msg.contains("High"));
    assert!(!daemon_msg.contains("Medium"));
    assert!(!daemon_msg.contains("0x3000"));
    assert!(!daemon_msg.contains("0x2000"));
    assert_eq!(format_integrity_line("CLI", cli), "CLI integrity: High");
    assert_eq!(daemon_integrity_from_health(None), None);
}

#[test]
fn health_info_uses_reported_daemon_rid_not_cli() {
    let health = IpcResponse::HealthInfo {
        healthy: true,
        uptime_seconds: 1,
        total_windows: 0,
        monitors: 1,
        paused: true,
        thumbnail_register_balance: 0,
        elevation_blocked_windows: Vec::new(),
        daemon_integrity: Some(leopardwm_platform_win32::INTEGRITY_MEDIUM),
        elevation_blocked_records: Some(Vec::new()),
        native_swipes: None,
        daemon_log: None,
        unknown_config_keys: None,
    };
    let (daemon, cli) = render_integrity_pair(
        Some(&health),
        Some(leopardwm_platform_win32::INTEGRITY_HIGH),
    );
    assert_eq!(check_message(&daemon), "Daemon integrity: Medium");
    assert_eq!(check_message(&cli), "CLI integrity: High");
}

#[test]
fn native_entry_without_opt_in_does_not_open_a_pipe() {
    if env_opt(OPT_IN_ENV).as_deref() == Some("1") {
        return;
    }
    let err = native_entry().unwrap_err();
    assert!(err.contains("fail-closed"), "{err}");
}
