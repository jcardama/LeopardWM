use crate::state::AppState;
use leopardwm_ipc::DaemonLogStatus;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Health {
    pub rows: Vec<HealthRow>,
    pub unknown_keys: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum HealthRow {
    Config {
        path: String,
        reason: String,
        line: Option<usize>,
        column: Option<usize>,
        recovered: Vec<String>,
    },
    Warning {
        field: String,
        message: String,
    },
    UnknownKeys {
        keys: Vec<String>,
    },
    ReservedHotkeys {
        binds: Vec<String>,
    },
    KeyboardHook,
    ElevatedWindow {
        executable: String,
    },
    Integrity {
        value: Option<u32>,
    },
    WatchdogRestart,
    LogFailure {
        path: String,
        error: String,
    },
    Update {
        tag: String,
    },
}

struct ConfigProblem {
    path: String,
    reason: String,
    location: Option<(usize, usize)>,
    recovered: Vec<String>,
}

#[derive(Default)]
struct HealthInputs {
    config: Option<ConfigProblem>,
    warnings: Vec<(String, String)>,
    unknown_keys: Option<Vec<String>>,
    reserved: Vec<String>,
    hook_unavailable: bool,
    elevated: Vec<String>,
    integrity: Option<u32>,
    watchdog_restarts: usize,
    log_status: Option<DaemonLogStatus>,
    update: Option<String>,
}

fn build_health(input: HealthInputs) -> Health {
    let mut rows = Vec::new();
    if let Some(config) = input.config {
        rows.push(HealthRow::Config {
            path: config.path,
            reason: config.reason,
            line: config.location.map(|location| location.0),
            column: config.location.map(|location| location.1),
            recovered: config.recovered,
        });
    }
    rows.extend(
        input
            .warnings
            .into_iter()
            .map(|(field, message)| HealthRow::Warning { field, message }),
    );
    if let Some(keys) = &input.unknown_keys {
        if !keys.is_empty() {
            rows.push(HealthRow::UnknownKeys { keys: keys.clone() });
        }
    }
    if !input.reserved.is_empty() {
        rows.push(HealthRow::ReservedHotkeys {
            binds: input.reserved,
        });
    }
    if input.hook_unavailable {
        rows.push(HealthRow::KeyboardHook);
    }
    rows.extend(
        input
            .elevated
            .into_iter()
            .map(|executable| HealthRow::ElevatedWindow { executable }),
    );
    if !matches!(
        input.integrity,
        Some(leopardwm_platform_win32::INTEGRITY_MEDIUM | leopardwm_platform_win32::INTEGRITY_HIGH)
    ) {
        rows.push(HealthRow::Integrity {
            value: input.integrity,
        });
    }
    if input.watchdog_restarts > 0 {
        rows.push(HealthRow::WatchdogRestart);
    }
    if let Some(
        DaemonLogStatus::OpenFailed { path, error } | DaemonLogStatus::WriteFailed { path, error },
    ) = input.log_status
    {
        rows.push(HealthRow::LogFailure { path, error });
    }
    if let Some(tag) = input.update {
        rows.push(HealthRow::Update { tag });
    }
    Health {
        rows,
        unknown_keys: input.unknown_keys,
    }
}

pub(crate) fn health(state: &AppState) -> Health {
    let config = crate::config::config_rejection().map(|rejection| ConfigProblem {
        path: rejection.path.display().to_string(),
        reason: rejection.reason,
        location: rejection.location,
        recovered: rejection
            .recovered_fields
            .into_iter()
            .map(|field| field.path)
            .collect(),
    });
    let mut elevated = state
        .elevation_blocked
        .keys()
        .map(|hwnd| {
            let mut pid = 0;
            unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                    windows::Win32::Foundation::HWND(*hwnd as *mut _),
                    Some(&mut pid),
                );
            }
            leopardwm_platform_win32::get_process_executable(pid).unwrap_or_default()
        })
        .collect::<Vec<_>>();
    elevated.sort();
    elevated.dedup();
    build_health(HealthInputs {
        config,
        warnings: state
            .config_warnings
            .iter()
            .map(|warning| (warning.field.clone(), warning.message.clone()))
            .collect(),
        unknown_keys: Some(crate::config::unknown_config_keys()),
        reserved: state.settings_failed_binds.clone(),
        hook_unavailable: state.keyboard_hook_unavailable,
        elevated,
        integrity: leopardwm_platform_win32::current_process_integrity(),
        watchdog_restarts: state.watchdog_restarts,
        log_status: state.daemon_log.as_ref().map(|log| log.status()),
        update: state.available_update.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn healthy() -> HealthInputs {
        HealthInputs {
            integrity: Some(leopardwm_platform_win32::INTEGRITY_MEDIUM),
            ..Default::default()
        }
    }
    #[test]
    fn no_problems_has_no_rows() {
        assert!(build_health(healthy()).rows.is_empty());
    }
    #[test]
    fn config_rejection_keeps_location_and_recovered_fields() {
        let mut input = healthy();
        let row = HealthRow::Config {
            path: "config.toml".into(),
            reason: "invalid".into(),
            line: Some(3),
            column: Some(7),
            recovered: vec!["layout.gap".into()],
        };
        input.config = Some(ConfigProblem {
            path: "config.toml".into(),
            reason: "invalid".into(),
            location: Some((3, 7)),
            recovered: vec!["layout.gap".into()],
        });
        assert_eq!(build_health(input).rows, vec![row]);
    }
    #[test]
    fn validation_warning_is_reported() {
        let mut input = healthy();
        input.warnings.push(("layout.gap".into(), "clamped".into()));
        assert_eq!(
            build_health(input).rows,
            vec![HealthRow::Warning {
                field: "layout.gap".into(),
                message: "clamped".into()
            }]
        );
    }
    #[test]
    fn unknown_keys_are_optional_and_only_nonempty_keys_are_reported() {
        let mut input = healthy();
        input.unknown_keys = Some(vec![]);
        let result = build_health(input);
        assert!(result.rows.is_empty());
        assert_eq!(result.unknown_keys, Some(vec![]));
        let mut input = healthy();
        input.unknown_keys = Some(vec!["unknown".into()]);
        assert_eq!(
            build_health(input).rows,
            vec![HealthRow::UnknownKeys {
                keys: vec!["unknown".into()]
            }]
        );
        assert_eq!(build_health(healthy()).unknown_keys, None);
    }
    #[test]
    fn reserved_hotkeys_are_reported() {
        let mut input = healthy();
        input.reserved.push("Win+L".into());
        assert_eq!(
            build_health(input).rows,
            vec![HealthRow::ReservedHotkeys {
                binds: vec!["Win+L".into()]
            }]
        );
    }
    #[test]
    fn unavailable_keyboard_hook_is_reported() {
        let mut input = healthy();
        input.hook_unavailable = true;
        assert_eq!(build_health(input).rows, vec![HealthRow::KeyboardHook]);
    }
    #[test]
    fn hidden_blocked_window_keeps_its_executable_in_health() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::*;
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!(""),
                WS_OVERLAPPED,
                0,
                0,
                1,
                1,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        };
        assert!(!unsafe { IsWindowVisible(hwnd) }.as_bool());
        let mut state = AppState::new_with_config(crate::config::Config::default(), vec![]);
        state.note_elevation_block(
            hwnd.0 as u64,
            "fixture",
            leopardwm_platform_win32::ManageBlock::HigherIntegrity,
        );
        let rows = health(&state).rows;
        unsafe {
            DestroyWindow(hwnd).unwrap();
        }
        let executable = std::env::current_exe()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(rows.iter().any(|row| matches!(row, HealthRow::ElevatedWindow { executable: actual } if actual.eq_ignore_ascii_case(&executable))));
    }
    #[test]
    fn unexpected_or_unknown_integrity_is_reported() {
        for value in [None, Some(0x1000)] {
            let mut input = healthy();
            input.integrity = value;
            assert_eq!(
                build_health(input).rows,
                vec![HealthRow::Integrity { value }]
            );
        }
        let mut input = healthy();
        input.integrity = Some(leopardwm_platform_win32::INTEGRITY_HIGH);
        assert!(build_health(input).rows.is_empty());
    }
    #[test]
    fn watchdog_restart_is_reported_without_count() {
        let mut input = healthy();
        input.watchdog_restarts = 2;
        assert_eq!(build_health(input).rows, vec![HealthRow::WatchdogRestart]);
    }
    #[test]
    fn log_failure_is_reported() {
        for status in [
            DaemonLogStatus::OpenFailed {
                path: "daemon.log".into(),
                error: "denied".into(),
            },
            DaemonLogStatus::WriteFailed {
                path: "daemon.log".into(),
                error: "denied".into(),
            },
        ] {
            let mut input = healthy();
            input.log_status = Some(status);
            assert_eq!(
                build_health(input).rows,
                vec![HealthRow::LogFailure {
                    path: "daemon.log".into(),
                    error: "denied".into()
                }]
            );
        }
        let mut input = healthy();
        input.log_status = Some(DaemonLogStatus::Writing {
            path: "daemon.log".into(),
        });
        assert!(build_health(input).rows.is_empty());
    }
    #[test]
    fn available_update_is_reported() {
        let mut input = healthy();
        input.update = Some("v1".into());
        assert_eq!(
            build_health(input).rows,
            vec![HealthRow::Update { tag: "v1".into() }]
        );
    }
}
