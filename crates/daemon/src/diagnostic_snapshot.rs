use crate::state::AppState;
use leopardwm_ipc::diagnostic_snapshot::{
    self, ConfigLoadStatus, HealthRow, Monitor, SnapshotInput, Workspace,
};
use leopardwm_ipc::{log_text, DaemonLogStatus, NativeSwipeStatus};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

pub(crate) fn config_health_rows(
    rejection: Option<crate::config::ConfigRejection>,
    unknown_count: usize,
) -> Vec<HealthRow> {
    let mut rows = Vec::new();
    if let Some(rejection) = rejection {
        let location = rejection
            .location
            .map_or_else(String::new, |(line, column)| {
                format!(" at line {line}, column {column}")
            });
        rows.push(HealthRow {
            summary: format!(
                "Config rejected{location}: {}",
                log_text::text(rejection.reason)
            ),
            hint: None,
        });
        if !rejection.recovered_fields.is_empty() {
            let fields = rejection
                .recovered_fields
                .iter()
                .map(|field| log_text::name(&field.path).to_string())
                .collect::<Vec<_>>()
                .join(", ");
            rows.push(HealthRow {
                summary: format!("Recovered settings: {fields}"),
                hint: None,
            });
        }
    }
    if unknown_count != 0 {
        rows.push(HealthRow {
            summary: format!("Unknown configuration keys: {unknown_count}"),
            hint: None,
        });
    }
    rows
}

fn diagnostic_health_rows(state: &AppState) -> Vec<HealthRow> {
    let mut rows = config_health_rows(
        crate::config::config_rejection(),
        crate::config::unknown_config_keys().len(),
    );
    if let Some(
        DaemonLogStatus::OpenFailed { error, .. } | DaemonLogStatus::WriteFailed { error, .. },
    ) = state.daemon_log.as_ref().map(|log| log.status())
    {
        rows.push(HealthRow {
            summary: format!("Daemon log failure: {}", log_text::text(error)),
            hint: None,
        });
    }
    for hwnd in state.elevation_blocked.keys() {
        let executable = state
            .lookup_window_info(*hwnd)
            .and_then(|info| leopardwm_platform_win32::get_process_executable(info.process_id))
            .unwrap_or_else(|| "unknown executable".into());
        rows.push(HealthRow {
            summary: format!("Privilege-blocked window: {}", log_text::name(executable)),
            hint: None,
        });
    }
    if let NativeSwipeStatus::Inactive { reason } = &state.native_swipes {
        rows.push(HealthRow {
            summary: format!("Native swipes inactive: {}", log_text::text(reason)),
            hint: None,
        });
    }
    for issue in crate::hotkey_resolution::resolve_hotkeys(&state.config.hotkeys).issues {
        rows.push(HealthRow {
            summary: format!(
                "Hotkey {}: {}",
                log_text::name(&issue.binding),
                log_text::name(&issue.message)
            ),
            hint: None,
        });
    }
    let balance = leopardwm_platform_win32::thumbnail::current_register_balance();
    if balance != 0 {
        rows.push(HealthRow {
            summary: format!("Thumbnail registration balance: {balance}"),
            hint: None,
        });
    }
    rows.sort_by(|a, b| a.summary.cmp(&b.summary));
    rows
}

fn collect_workspace(
    monitor: &str,
    index: usize,
    ws: &leopardwm_core_layout::Workspace,
    ids: &mut HashSet<u64>,
) -> Workspace {
    let rows = ws
        .rows()
        .iter()
        .map(|row| {
            row.columns()
                .iter()
                .map(|column| {
                    ids.extend(column.windows().iter().copied());
                    (column.width(), column.len())
                })
                .collect()
        })
        .collect();
    ids.extend(ws.floating_windows().iter().map(|window| window.id));
    Workspace {
        monitor: monitor.into(),
        index,
        rows,
        floating: ws.floating_count(),
    }
}

#[derive(Default)]
struct SnapshotCache {
    last: Option<(Instant, PathBuf)>,
}

impl SnapshotCache {
    fn get_or_write(
        &mut self,
        write: impl FnOnce() -> anyhow::Result<PathBuf>,
    ) -> anyhow::Result<PathBuf> {
        if let Some((written, path)) = &self.last {
            if written.elapsed() < Duration::from_secs(5) && path.is_file() {
                return Ok(path.clone());
            }
        }
        let path = write()?;
        self.last = Some((Instant::now(), path.clone()));
        Ok(path)
    }
}

static SNAPSHOT_CACHE: Mutex<SnapshotCache> = Mutex::new(SnapshotCache { last: None });

/// Settings calls this same entry point when it offers snapshot capture.
pub(crate) fn create_diagnostic_snapshot(
    state: &AppState,
    requester: &str,
    requester_version: String,
    requester_integrity: Option<u32>,
) -> anyhow::Result<PathBuf> {
    SNAPSHOT_CACHE.lock().unwrap().get_or_write(|| {
        write_diagnostic_snapshot(state, requester, requester_version, requester_integrity)
    })
}

fn write_diagnostic_snapshot(
    state: &AppState,
    requester: &str,
    requester_version: String,
    requester_integrity: Option<u32>,
) -> anyhow::Result<PathBuf> {
    let mut monitors: Vec<_> = state
        .monitors
        .values()
        .map(|m| Monitor {
            device: m.device_name.clone(),
            x: m.rect.x,
            y: m.rect.y,
            width: m.rect.width,
            height: m.rect.height,
            scale: m.scale_factor,
        })
        .collect();
    monitors.sort_by(|a, b| a.device.cmp(&b.device));
    let mut ids = HashSet::new();
    let mut workspaces = Vec::new();
    for (id, items) in &state.workspaces {
        let name = state
            .monitors
            .get(id)
            .map(|m| m.device_name.clone())
            .unwrap_or_else(|| format!("monitor {id}"));
        for (index, workspace) in items.iter().enumerate() {
            workspaces.push(collect_workspace(&name, index, workspace, &mut ids));
        }
    }
    for (name, layout) in &state.stashed_monitor_layouts {
        for (index, workspace) in layout.workspaces.iter().enumerate() {
            workspaces.push(collect_workspace(name, index, workspace, &mut ids));
        }
    }
    workspaces.sort_by(|a, b| a.monitor.cmp(&b.monitor).then(a.index.cmp(&b.index)));
    if let Some(scratchpad) = state.scratchpad {
        ids.insert(scratchpad.window_id);
    }
    ids.extend(state.elevation_blocked.keys().copied());
    let mut titles: Vec<String> = ids
        .into_iter()
        .map(|id| {
            state
                .lookup_window_info(id)
                .map(|info| info.title)
                .unwrap_or_else(|| leopardwm_platform_win32::get_window_title(id))
        })
        .collect();
    titles.extend(
        state
            .elevation_blocked
            .values()
            .map(|entry| entry.title.clone()),
    );
    titles.extend(state.tab_title_overrides.values().cloned());
    if let Some(report) = &state.pending_layout_apply_timeout_report {
        titles.extend(
            report
                .candidates
                .iter()
                .filter_map(|candidate| candidate.title.clone()),
        );
    }
    let paths = crate::config::config_paths();
    let config_path = paths
        .iter()
        .find(|path| path.exists())
        .cloned()
        .unwrap_or_else(|| paths[0].clone());
    let config_status = if config_path.exists() {
        match std::fs::read(&config_path) {
            Ok(_) => ConfigLoadStatus::Loaded,
            Err(error) => ConfigLoadStatus::ReadFailed(error.to_string()),
        }
    } else {
        ConfigLoadStatus::NotFound
    };
    let input = SnapshotInput {
        requester: requester.into(),
        requester_version,
        requester_integrity,
        daemon_version: Some(env!("CARGO_PKG_VERSION").into()),
        daemon_integrity: leopardwm_platform_win32::current_process_integrity(),
        daemon_uptime: Some(state.start_time.elapsed().as_secs()),
        unavailable_reason: "daemon not running".into(),
        windows_version: leopardwm_platform_win32::get_windows_version(),
        monitors: Some(monitors),
        health: Some(diagnostic_health_rows(state)),
        config_path,
        config_status,
        config_text: Some(toml::to_string(&state.config)?),
        workspaces: Some(workspaces),
        known_titles: titles,
        userprofile: std::env::var("USERPROFILE").ok(),
        log_dir: leopardwm_ipc::log_dir(),
        timestamp: SystemTime::now(),
    };
    Ok(diagnostic_snapshot::write(
        &input,
        &leopardwm_ipc::diagnostics_dir(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_snapshot_is_reused_until_missing_or_expired() {
        let dir = std::env::temp_dir().join(format!("lwm-snapshot-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let mut cache = SnapshotCache::default();
        let input = SnapshotInput {
            requester: "test".into(),
            requester_version: "1.0.0".into(),
            requester_integrity: None,
            daemon_version: None,
            daemon_integrity: None,
            daemon_uptime: None,
            unavailable_reason: "test".into(),
            windows_version: "test".into(),
            monitors: None,
            health: None,
            config_path: dir.join("config.toml"),
            config_status: ConfigLoadStatus::NotFound,
            config_text: None,
            workspaces: None,
            known_titles: vec![],
            userprofile: None,
            log_dir: dir.clone(),
            timestamp: SystemTime::now(),
        };
        let write = || Ok(diagnostic_snapshot::write(&input, &dir)?);
        let first = cache.get_or_write(write).unwrap();
        let second = cache.get_or_write(write).unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_file(&first).unwrap();
        let third = cache.get_or_write(write).unwrap();
        assert!(third.is_file());
        cache.last.as_mut().unwrap().0 = Instant::now() - Duration::from_secs(5);
        let fourth = cache.get_or_write(write).unwrap();
        assert_ne!(third, fourth);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
