//! Unit tests for the CLI modules.

use crate::args::*;
use crate::command_map::*;
use crate::config_cmds::*;
use crate::daemon_cmds::*;
use crate::doctor::*;
use crate::ipc_client::*;
use anyhow::Context;
use clap::Parser;
use leopardwm_ipc::{IpcCommand, IpcResponse, MAX_IPC_MESSAGE_SIZE};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

// =========================================================================
// to_ipc_command tests
// =========================================================================

#[test]
fn test_to_ipc_command_focus_left() {
    let cmd = Commands::Focus {
        direction: FocusDirection::Left,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusLeft));
}

#[test]
fn test_to_ipc_command_focus_right() {
    let cmd = Commands::Focus {
        direction: FocusDirection::Right,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusRight));
}

#[test]
fn test_to_ipc_command_focus_up() {
    let cmd = Commands::Focus {
        direction: FocusDirection::Up,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusUp));
}

#[test]
fn test_to_ipc_command_focus_down() {
    let cmd = Commands::Focus {
        direction: FocusDirection::Down,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusDown));
}

#[test]
fn test_to_ipc_command_scroll_left() {
    let cmd = Commands::Scroll {
        direction: ScrollDirection::Left { pixels: 100 },
    };
    match to_ipc_command(&cmd) {
        IpcCommand::Scroll { delta } => assert_eq!(delta, -100.0),
        other => panic!("Expected Scroll command, got {:?}", other),
    }
}

#[test]
fn test_to_ipc_command_scroll_right() {
    let cmd = Commands::Scroll {
        direction: ScrollDirection::Right { pixels: 150 },
    };
    match to_ipc_command(&cmd) {
        IpcCommand::Scroll { delta } => assert_eq!(delta, 150.0),
        other => panic!("Expected Scroll command, got {:?}", other),
    }
}

#[test]
fn test_to_ipc_command_move_left() {
    let cmd = Commands::Move {
        direction: MoveDirection::Left,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::MoveColumnLeft));
}

#[test]
fn test_to_ipc_command_move_right() {
    let cmd = Commands::Move {
        direction: MoveDirection::Right,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::MoveColumnRight));
}

#[test]
fn test_to_ipc_command_move_window_left() {
    let cmd = Commands::MoveWindow {
        direction: MoveWindowDirection::Left,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::MoveWindowLeft));
}

#[test]
fn test_to_ipc_command_move_window_right() {
    let cmd = Commands::MoveWindow {
        direction: MoveWindowDirection::Right,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::MoveWindowRight));
}

#[test]
fn test_to_ipc_command_move_window_up() {
    let cmd = Commands::MoveWindow {
        direction: MoveWindowDirection::Up,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::MoveWindowUp));
}

#[test]
fn test_to_ipc_command_move_window_down() {
    let cmd = Commands::MoveWindow {
        direction: MoveWindowDirection::Down,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::MoveWindowDown));
}

#[test]
fn test_to_ipc_command_expel_left() {
    let cmd = Commands::Expel {
        direction: ExpelDirection::Left,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::ExpelToLeft));
}

#[test]
fn test_to_ipc_command_expel_right() {
    let cmd = Commands::Expel {
        direction: ExpelDirection::Right,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::ExpelToRight));
}

#[test]
fn test_to_ipc_command_resize() {
    let cmd = Commands::Resize { delta: 50 };
    match to_ipc_command(&cmd) {
        IpcCommand::Resize { delta } => assert_eq!(delta, 50),
        other => panic!("Expected Resize command, got {:?}", other),
    }
}

#[test]
fn test_to_ipc_command_resize_negative() {
    let cmd = Commands::Resize { delta: -30 };
    match to_ipc_command(&cmd) {
        IpcCommand::Resize { delta } => assert_eq!(delta, -30),
        other => panic!("Expected Resize command, got {:?}", other),
    }
}

#[test]
fn test_to_ipc_command_focus_monitor_left() {
    let cmd = Commands::FocusMonitor {
        direction: MonitorDirection::Left,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusMonitorLeft));
}

#[test]
fn test_to_ipc_command_focus_monitor_right() {
    let cmd = Commands::FocusMonitor {
        direction: MonitorDirection::Right,
    };
    assert!(matches!(
        to_ipc_command(&cmd),
        IpcCommand::FocusMonitorRight
    ));
}

#[test]
fn test_to_ipc_command_move_to_monitor_left() {
    let cmd = Commands::MoveToMonitor {
        direction: MonitorDirection::Left,
    };
    assert!(matches!(
        to_ipc_command(&cmd),
        IpcCommand::MoveWindowToMonitorLeft
    ));
}

#[test]
fn test_to_ipc_command_move_to_monitor_right() {
    let cmd = Commands::MoveToMonitor {
        direction: MonitorDirection::Right,
    };
    assert!(matches!(
        to_ipc_command(&cmd),
        IpcCommand::MoveWindowToMonitorRight
    ));
}

#[test]
fn test_to_ipc_command_query_workspace() {
    let cmd = Commands::Query {
        what: QueryType::Workspace,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::QueryWorkspace));
}

#[test]
fn test_to_ipc_command_query_focused() {
    let cmd = Commands::Query {
        what: QueryType::Focused,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::QueryFocused));
}

#[test]
fn test_to_ipc_command_query_all() {
    let cmd = Commands::Query {
        what: QueryType::All,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::QueryAllWindows));
}

#[test]
fn test_to_ipc_command_refresh() {
    let cmd = Commands::Refresh;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::Refresh));
}

#[test]
fn test_to_ipc_command_reload() {
    let cmd = Commands::Reload;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::Reload));
}

#[test]
fn test_to_ipc_command_stop() {
    let cmd = Commands::Stop;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::Stop));
}

#[test]
fn test_to_ipc_command_toggle_pause() {
    let cmd = Commands::TogglePause;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::TogglePause));
}

#[test]
fn test_to_ipc_command_panic_revert() {
    let cmd = Commands::PanicRevert;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::PanicRevert));
}

#[test]
fn test_cli_alias_recover_parses_to_panic_revert() {
    let cli = Cli::try_parse_from(["leopardwm-cli", "recover"]).expect("alias should parse");
    assert!(matches!(cli.command, Commands::PanicRevert));
}

#[test]
fn test_cli_alias_pause_parses_to_toggle_pause() {
    let cli = Cli::try_parse_from(["leopardwm-cli", "pause"]).expect("alias should parse");
    assert!(matches!(cli.command, Commands::TogglePause));
}

#[test]
fn test_cli_alias_restore_windows_parses_to_emergency_uncloak() {
    let cli =
        Cli::try_parse_from(["leopardwm-cli", "restore-windows"]).expect("alias should parse");
    assert!(matches!(cli.command, Commands::EmergencyUncloak));
}

// =========================================================================
// generate_default_config tests
// =========================================================================

#[test]
fn test_generate_default_config_contains_layout_section() {
    let config = generate_default_config();
    assert!(config.contains("[layout]"));
    assert!(config.contains("gap"));
    assert!(config.contains("outer_gap_left"));
}

#[test]
fn test_generate_default_config_contains_appearance_section() {
    let config = generate_default_config();
    assert!(config.contains("[appearance]"));
}

#[test]
fn test_generate_default_config_contains_behavior_section() {
    let config = generate_default_config();
    assert!(config.contains("[behavior]"));
    assert!(config.contains("focus_new_windows"));
    assert!(config.contains("track_focus_changes"));
    assert!(config.contains("log_level"));
}

#[test]
fn test_generate_default_config_contains_centering_mode() {
    let config = generate_default_config();
    assert!(config.contains("centering_mode"));
    assert!(config.contains("center") || config.contains("just_in_view"));
}

// =========================================================================
// default_config_path tests
// =========================================================================

#[test]
fn test_default_config_path_returns_some() {
    // This may return None in certain CI environments without home dirs
    // but on most systems it should return Some
    let path = default_config_path();
    // Just verify the function runs without panicking
    if let Some(p) = path {
        assert!(p.ends_with("config.toml"));
    }
}

#[test]
fn test_default_config_path_contains_leopardwm() {
    if let Some(path) = default_config_path() {
        let path_str = path.to_string_lossy();
        assert!(
            path_str.contains("leopardwm"),
            "Path should contain 'leopardwm': {}",
            path_str
        );
    }
}

// =========================================================================
// IPC timeout and framing tests
// =========================================================================

#[test]
fn test_ipc_connect_timeout_is_reasonable() {
    // Timeout should be between 1 and 30 seconds
    assert!(IPC_CONNECT_TIMEOUT >= Duration::from_secs(1));
    assert!(IPC_CONNECT_TIMEOUT <= Duration::from_secs(30));
}

#[test]
fn test_recovery_connect_timeout_is_longer_than_default_connect() {
    assert!(IPC_RECOVERY_CONNECT_TIMEOUT > IPC_CONNECT_TIMEOUT);
    assert!(IPC_RECOVERY_CONNECT_TIMEOUT <= Duration::from_secs(30));
}

#[test]
fn test_default_response_timeout_is_reasonable() {
    assert!(IPC_DEFAULT_RESPONSE_TIMEOUT >= Duration::from_secs(1));
    assert!(IPC_DEFAULT_RESPONSE_TIMEOUT <= Duration::from_secs(30));
}

#[test]
fn test_recovery_response_timeout_is_longer_than_default() {
    assert!(IPC_RECOVERY_RESPONSE_TIMEOUT > IPC_DEFAULT_RESPONSE_TIMEOUT);
    assert!(IPC_RECOVERY_RESPONSE_TIMEOUT <= Duration::from_secs(60));
}

#[test]
fn test_shutdown_confirm_timeout_is_reasonable() {
    assert!(SHUTDOWN_CONFIRM_TIMEOUT >= Duration::from_secs(1));
    assert!(SHUTDOWN_CONFIRM_TIMEOUT <= Duration::from_secs(60));
}

#[test]
fn test_shutdown_confirm_poll_interval_is_reasonable() {
    assert!(SHUTDOWN_CONFIRM_POLL_INTERVAL >= Duration::from_millis(50));
    assert!(SHUTDOWN_CONFIRM_POLL_INTERVAL <= Duration::from_secs(1));
}

#[test]
fn test_command_connect_timeout_for_apply_uses_default_connect_budget() {
    assert_eq!(
        command_connect_timeout(&IpcCommand::Apply),
        IPC_CONNECT_TIMEOUT
    );
}

#[test]
fn test_command_connect_timeout_for_stop_and_panic_revert_uses_recovery_budget() {
    assert_eq!(
        command_connect_timeout(&IpcCommand::Stop),
        IPC_RECOVERY_CONNECT_TIMEOUT
    );
    assert_eq!(
        command_connect_timeout(&IpcCommand::PanicRevert),
        IPC_RECOVERY_CONNECT_TIMEOUT
    );
}

#[test]
fn test_command_response_timeout_for_stop_and_panic_revert() {
    assert_eq!(
        command_response_timeout(&IpcCommand::Stop),
        IPC_RECOVERY_RESPONSE_TIMEOUT
    );
    assert_eq!(
        command_response_timeout(&IpcCommand::PanicRevert),
        IPC_RECOVERY_RESPONSE_TIMEOUT
    );
}

#[test]
fn test_command_response_timeout_for_apply_uses_default() {
    assert_eq!(
        command_response_timeout(&IpcCommand::Apply),
        IPC_DEFAULT_RESPONSE_TIMEOUT
    );
}

#[test]
fn test_command_response_timeout_for_regular_command_uses_default() {
    assert_eq!(
        command_response_timeout(&IpcCommand::FocusLeft),
        IPC_DEFAULT_RESPONSE_TIMEOUT
    );
}

#[test]
fn test_empty_response_parse_fails() {
    // Verify that an empty string cannot be parsed as a valid IPC response
    let result: Result<IpcResponse, _> = serde_json::from_str("");
    assert!(
        result.is_err(),
        "Empty string should not parse as IpcResponse"
    );
}

#[test]
fn test_unknown_response_parse_maps_to_unknown() {
    let result: Result<IpcResponse, _> =
        serde_json::from_str(r#"{"status":"future_response","data":{"x":1}}"#);
    assert!(matches!(result, Ok(IpcResponse::Unknown)));
}

#[test]
fn test_is_non_success_response_for_unknown() {
    assert!(is_non_success_response(&IpcResponse::Unknown));
    assert!(!is_non_success_response(&IpcResponse::Ok));
}

#[test]
fn test_classify_pipe_probe_error_busy() {
    let err = std::io::Error::from_raw_os_error(231);
    assert_eq!(classify_pipe_probe_error(&err), Some(true));
}

#[test]
fn test_classify_pipe_probe_error_not_found() {
    let err = std::io::Error::from_raw_os_error(2);
    assert_eq!(classify_pipe_probe_error(&err), Some(false));
}

#[test]
fn test_classify_pipe_probe_error_unknown() {
    let err = std::io::Error::from_raw_os_error(5);
    assert_eq!(classify_pipe_probe_error(&err), None);
}

#[test]
fn test_pipe_connect_retry_timeout_message_busy_only() {
    let message = pipe_connect_retry_timeout_message(Duration::from_millis(750), true, false);
    assert!(message.contains("750ms"));
    assert!(message.contains("busy"));
    assert!(message.contains("leopardwm-cli status"));
}

#[test]
fn test_pipe_connect_retry_timeout_message_not_found_only() {
    let message = pipe_connect_retry_timeout_message(Duration::from_millis(500), false, true);
    assert!(message.contains("500ms"));
    assert!(message.contains("not found"));
    assert!(message.contains("leopardwm-cli run"));
}

#[test]
fn test_pipe_connect_retry_timeout_message_mixed_states() {
    let message = pipe_connect_retry_timeout_message(Duration::from_millis(1000), true, true);
    assert!(message.contains("1000ms"));
    assert!(message.contains("busy"));
    assert!(message.contains("not-found"));
    assert!(message.contains("leopardwm-cli status"));
}

#[test]
fn test_pipe_connect_not_found_fast_fail_message_is_actionable() {
    let message = pipe_connect_not_found_fast_fail_message(Duration::from_millis(800));
    assert!(message.contains("800ms"));
    assert!(message.contains("not found"));
    assert!(message.contains("leopardwm-cli run"));
}

#[test]
fn test_safe_mode_existing_daemon_message_is_actionable() {
    let message = safe_mode_existing_daemon_message();
    assert!(message.contains("leopardwm-cli stop"));
    assert!(message.contains("leopardwm-cli run --safe-mode"));
}

#[test]
fn test_error_chain_has_pipe_not_found_true() {
    let err = Err::<(), _>(std::io::Error::from_raw_os_error(2))
        .context("wrapped")
        .unwrap_err();
    assert!(error_chain_has_pipe_not_found(&err));
}

#[test]
fn test_error_chain_has_pipe_not_found_false() {
    let err = Err::<(), _>(std::io::Error::from_raw_os_error(5))
        .context("wrapped")
        .unwrap_err();
    assert!(!error_chain_has_pipe_not_found(&err));
}

#[test]
fn test_error_chain_has_disconnected_before_response_true() {
    let err = anyhow::anyhow!(PIPE_DISCONNECTED_BEFORE_RESPONSE_MESSAGE).context("wrapped");
    assert!(error_chain_has_disconnected_before_response(&err));
}

#[test]
fn test_error_chain_has_disconnected_before_response_false() {
    let err = anyhow::anyhow!("some other message").context("wrapped");
    assert!(!error_chain_has_disconnected_before_response(&err));
}

#[test]
fn test_error_chain_has_command_timeout_true_for_timeout_message() {
    let err = anyhow::anyhow!("Timed out waiting for daemon response after 15000ms");
    assert!(error_chain_has_command_timeout(&err));
}

#[test]
fn test_error_chain_has_command_timeout_false_for_non_timeout_error() {
    let err = anyhow::anyhow!("some other failure");
    assert!(!error_chain_has_command_timeout(&err));
}

#[test]
fn test_error_chain_indicates_pipe_not_found_timeout_true() {
    let err = anyhow::anyhow!(
        "Timed out after 1000ms connecting to daemon IPC pipe: the pipe was not found (daemon is likely not running). Start it with `leopardwm-cli run`."
    );
    assert!(error_chain_indicates_pipe_not_found_timeout(&err));
}

#[test]
fn test_error_chain_indicates_pipe_not_found_timeout_true_for_fast_fail_message() {
    let err = anyhow::anyhow!(pipe_connect_not_found_fast_fail_message(
        Duration::from_millis(800)
    ));
    assert!(error_chain_indicates_pipe_not_found_timeout(&err));
}

#[test]
fn test_error_chain_indicates_pipe_not_found_timeout_false() {
    let err = anyhow::anyhow!(
        "Timed out after 1000ms connecting to daemon IPC pipe: observed both busy and not-found states (daemon may be transitioning startup/shutdown)."
    );
    assert!(!error_chain_indicates_pipe_not_found_timeout(&err));
}

#[test]
fn test_error_chain_has_connect_timeout_true() {
    let err = anyhow::anyhow!(
        "Timed out after 1000ms connecting to daemon IPC pipe: observed both busy and not-found states (daemon may be transitioning startup/shutdown)."
    );
    assert!(error_chain_has_connect_timeout(&err));
}

#[test]
fn test_error_chain_has_connect_timeout_false() {
    let err = anyhow::anyhow!("Timed out waiting for daemon response after 15000ms");
    assert!(!error_chain_has_connect_timeout(&err));
}

#[test]
fn test_stop_race_shutdown_message_is_actionable() {
    assert!(stop_race_shutdown_message().contains("stopping or stopped"));
    assert!(stop_race_shutdown_message().contains("leopardwm-cli status"));
}

#[test]
fn test_panic_revert_not_running_message_is_actionable() {
    let message = panic_revert_not_running_message();
    assert!(message.contains("not running"));
    assert!(message.contains("leopardwm-cli emergency-uncloak"));
}

#[test]
fn test_panic_revert_unconfirmed_message_is_actionable() {
    let message = panic_revert_unconfirmed_message();
    assert!(message.contains("before confirming"));
    assert!(message.contains("leopardwm-cli status"));
    assert!(message.contains("Local emergency visibility restore"));
}

#[test]
fn test_panic_revert_timeout_recovery_message_is_actionable() {
    let message = panic_revert_timeout_recovery_message();
    assert!(message.contains("Timed out"));
    assert!(message.contains("Local emergency visibility restore"));
    assert!(message.contains("leopardwm-cli status"));
}

#[test]
fn test_stop_timeout_recovery_message_is_actionable() {
    let message = stop_timeout_recovery_message();
    assert!(message.contains("Timed out"));
    assert!(message.contains("leopardwm-cli status"));
    assert!(message.contains("leopardwm-cli panic-revert"));
    assert!(message.contains("leopardwm-cli emergency-uncloak"));
}

#[test]
fn test_apply_not_running_message_is_actionable() {
    let message = apply_not_running_message();
    assert!(message.contains("not running"));
    assert!(message.contains("leopardwm-cli run"));
}

#[test]
fn test_apply_timeout_recovery_message_is_actionable() {
    let message = apply_timeout_recovery_message();
    assert!(message.contains("Timed out"));
    assert!(message.contains("leopardwm-cli panic-revert"));
    assert!(message.contains("leopardwm-cli emergency-uncloak"));
}

#[test]
fn test_apply_unconfirmed_recovery_message_is_actionable() {
    let message = apply_unconfirmed_recovery_message();
    assert!(message.contains("not confirmed"));
    assert!(message.contains("Local emergency visibility restore"));
    assert!(message.contains("leopardwm-cli status"));
}

#[test]
fn test_apply_error_response_recovery_message_is_actionable() {
    let message = apply_error_response_recovery_message();
    assert!(message.contains("non-success apply response"));
    assert!(message.contains("Local emergency visibility restore"));
    assert!(message.contains("leopardwm-cli status"));
}

#[test]
fn test_stop_error_response_recovery_message_is_actionable() {
    let message = stop_error_response_recovery_message();
    assert!(message.contains("non-success stop response"));
    assert!(message.contains("Local emergency visibility restore"));
    assert!(message.contains("leopardwm-cli status"));
}

#[test]
fn test_panic_revert_error_response_recovery_message_is_actionable() {
    let message = panic_revert_error_response_recovery_message();
    assert!(message.contains("non-success panic-revert response"));
    assert!(message.contains("Local emergency visibility restore"));
    assert!(message.contains("leopardwm-cli status"));
}

#[test]
fn test_non_success_recovery_reasons_are_command_specific() {
    assert!(apply_non_success_recovery_reason().contains("apply"));
    assert!(stop_non_success_recovery_reason().contains("stop"));
    assert!(panic_revert_non_success_recovery_reason().contains("panic-revert"));
}

#[test]
fn test_parse_ipc_response_line_parses_ok_response() {
    let raw = serde_json::to_string(&IpcResponse::Ok).unwrap();
    let response = parse_ipc_response_line(&raw).unwrap();
    assert!(matches!(response, IpcResponse::Ok));
}

#[test]
fn test_parse_ipc_response_frame_accepts_valid_newline_terminated_response() {
    let frame = format!("{}\n", serde_json::to_string(&IpcResponse::Ok).unwrap());
    let response = parse_ipc_response_frame(frame.as_bytes(), MAX_IPC_MESSAGE_SIZE).unwrap();
    assert!(matches!(response, IpcResponse::Ok));
}

#[test]
fn test_parse_ipc_response_frame_rejects_oversized_payload() {
    let oversized = vec![b'x'; MAX_IPC_MESSAGE_SIZE + 1];
    let err = parse_ipc_response_frame(&oversized, MAX_IPC_MESSAGE_SIZE).unwrap_err();
    assert!(err.to_string().contains("exceeded"));
}

#[test]
fn test_parse_ipc_response_frame_rejects_non_newline_terminated_payload() {
    let frame = serde_json::to_string(&IpcResponse::Ok).unwrap();
    let err = parse_ipc_response_frame(frame.as_bytes(), MAX_IPC_MESSAGE_SIZE).unwrap_err();
    assert!(err.to_string().contains("newline-terminated"));
}

#[test]
fn test_to_ipc_command_close_window() {
    let cmd = Commands::CloseWindow;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::CloseWindow));
}

#[test]
fn test_to_ipc_command_toggle_floating() {
    let cmd = Commands::ToggleFloating;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::ToggleFloating));
}

#[test]
fn test_to_ipc_command_toggle_fullscreen() {
    let cmd = Commands::ToggleFullscreen;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::ToggleFullscreen));
}

#[test]
fn test_to_ipc_command_set_width() {
    let cmd = Commands::SetWidth { fraction: 0.5 };
    match to_ipc_command(&cmd) {
        IpcCommand::SetColumnWidth { fraction } => {
            assert!((fraction - 0.5).abs() < f64::EPSILON)
        }
        other => panic!("Expected SetColumnWidth, got {:?}", other),
    }
}

#[test]
fn test_validate_set_width_fraction_accepts_bounds() {
    assert!(validate_set_width_fraction(0.1).is_ok());
    assert!(validate_set_width_fraction(1.0).is_ok());
}

#[test]
fn test_validate_set_width_fraction_rejects_out_of_range() {
    assert!(validate_set_width_fraction(0.09).is_err());
    assert!(validate_set_width_fraction(1.01).is_err());
}

#[test]
fn test_validate_set_width_fraction_rejects_non_finite() {
    assert!(validate_set_width_fraction(f64::NAN).is_err());
    assert!(validate_set_width_fraction(f64::INFINITY).is_err());
}

#[test]
fn test_parse_set_width_fraction_rejects_non_numeric() {
    assert!(parse_set_width_fraction("not-a-number").is_err());
}

#[test]
fn test_to_ipc_command_equalize_widths() {
    let cmd = Commands::EqualizeWidths;
    assert!(matches!(
        to_ipc_command(&cmd),
        IpcCommand::EqualizeColumnWidths
    ));
}

#[test]
fn test_to_ipc_command_status() {
    let cmd = Commands::Status;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::QueryStatus));
}

#[test]
fn test_generate_default_config_contains_hotkeys() {
    let config = generate_default_config();
    assert!(config.contains("[hotkeys]"));
    assert!(config.contains("close_window"));
    assert!(config.contains("toggle_floating"));
    assert!(config.contains("\"Win+Ctrl+Escape\" = \"panic_revert\""));
    assert!(config.contains("toggle_pause"));
}

#[test]
fn test_generate_default_config_contains_gestures() {
    let config = generate_default_config();
    assert!(config.contains("[gestures]"));
    assert!(config.contains("enabled = true"));
}

#[test]
fn test_generate_default_config_contains_snap_hints() {
    let config = generate_default_config();
    assert!(config.contains("[snap_hints]"));
}

// =========================================================================
// Doctor helper tests
// =========================================================================

#[test]
fn test_doctor_config_path_returns_primary() {
    let (_found, display) = doctor_config_path();
    let display_str = display.to_string_lossy();
    assert!(
        display_str.contains("leopardwm"),
        "Display path should contain leopardwm: {}",
        display_str
    );
    assert!(
        display_str.ends_with("config.toml"),
        "Display path should end with config.toml: {}",
        display_str
    );
}

#[test]
fn test_validate_toml_valid() {
    let dir = std::env::temp_dir();
    let path = dir.join("leopardwm-test-valid.toml");
    fs::write(&path, "[layout]\ngap = 10\n").unwrap();
    assert!(validate_toml_file(&path).is_ok());
    let _ = fs::remove_file(&path);
}

#[test]
fn test_validate_toml_invalid() {
    let dir = std::env::temp_dir();
    let path = dir.join("leopardwm-test-invalid.toml");
    fs::write(&path, "[layout\ngap = !!!").unwrap();
    assert!(validate_toml_file(&path).is_err());
    let _ = fs::remove_file(&path);
}

#[test]
fn test_check_result_variants() {
    // Verify that all CheckResult variants can be constructed and printed
    let pass = CheckResult::Pass("test pass".to_string());
    let warn = CheckResult::Warn("test warn".to_string());
    let fail = CheckResult::Fail("test fail".to_string());
    // Just verify they don't panic when printed
    pass.print();
    warn.print();
    fail.print();
}

#[test]
fn test_get_windows_version_does_not_panic() {
    let version = get_windows_version();
    assert!(!version.is_empty());
}

// =========================================================================
// Phase 2: CLI completeness tests (Iteration 42)
// =========================================================================

#[test]
fn test_config_backup_path() {
    let config = PathBuf::from("/some/path/config.toml");
    let backup = config_backup_path(&config);
    assert!(
        backup.to_string_lossy().ends_with("config.toml.bak"),
        "backup path should end with .toml.bak: {}",
        backup.display()
    );
}

#[test]
fn test_config_backup_and_restore_roundtrip() {
    let dir = std::env::temp_dir().join("leopardwm-test-config-roundtrip");
    let _ = fs::create_dir_all(&dir);
    let config_path = dir.join("config.toml");
    let backup_path = config_backup_path(&config_path);

    // Write initial config
    fs::write(&config_path, "gap = 10\n").unwrap();

    // Backup
    fs::copy(&config_path, &backup_path).unwrap();
    assert!(backup_path.exists());

    // Modify config
    fs::write(&config_path, "gap = 20\n").unwrap();

    // Restore
    fs::copy(&backup_path, &config_path).unwrap();
    let restored = fs::read_to_string(&config_path).unwrap();
    assert_eq!(restored, "gap = 10\n");

    // Cleanup
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_handle_collect_logs_does_not_panic() {
    // Just verify the function runs without panicking
    // It prints to stdout, which is fine in tests
    let result = handle_collect_logs();
    assert!(result.is_ok());
}

#[test]
fn test_config_action_variants_parse() {
    // Verify ConfigAction variants can be constructed
    let _init = ConfigAction::Init {
        output: None,
        force: false,
        profile: None,
    };
    let _reset = ConfigAction::Reset;
    let _backup = ConfigAction::Backup;
    let _restore = ConfigAction::Restore;
}

// =========================================================================
// Phase 4: Profile config tests (Iteration 44)
// =========================================================================

#[test]
fn test_generate_profile_config_laptop() {
    let config = generate_profile_config("laptop");
    assert!(config.contains("laptop profile"));
    assert!(config.contains("gap = 6"));
    assert!(config.contains("outer_gap_left = 6"));
}

#[test]
fn test_generate_profile_config_ultrawide() {
    let config = generate_profile_config("ultrawide");
    assert!(config.contains("ultrawide profile"));
    assert!(config.contains("outer_gap_left = 16"));
    assert!(config.contains("just_in_view"));
}

#[test]
fn test_generate_profile_config_developer() {
    let config = generate_profile_config("developer");
    assert!(config.contains("developer profile"));
    assert!(config.contains("outer_gap_left = 10"));
    assert!(config.contains("\"Win+Ctrl+Escape\" = \"panic_revert\""));
}

#[test]
fn test_all_profile_configs_are_valid_toml() {
    for profile in &["developer", "laptop", "ultrawide"] {
        let content = generate_profile_config(profile);
        let result = content.parse::<toml::Table>();
        assert!(
            result.is_ok(),
            "Profile '{}' generates invalid TOML: {:?}",
            profile,
            result.err()
        );
    }
}
