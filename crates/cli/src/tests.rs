//! Unit tests for the CLI modules.

use crate::args::*;
use crate::command_map::*;
use crate::config_cmds::*;
use crate::daemon_cmds::*;
use crate::doctor::*;
use crate::ipc_client::*;
use anyhow::Context;
use clap::{CommandFactory, Parser};
use leopardwm_ipc::{
    ElevationBlockReason, ElevationBlockedWindow, EventKind, IpcCommand, IpcResponse,
    MAX_IPC_MESSAGE_SIZE,
};
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
fn test_to_ipc_command_focus_start() {
    let cmd = Commands::Focus {
        direction: FocusDirection::Start,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusStart));
}

#[test]
fn test_to_ipc_command_focus_end() {
    let cmd = Commands::Focus {
        direction: FocusDirection::End,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::FocusEnd));
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
fn test_to_ipc_command_move_start() {
    let cmd = Commands::Move {
        direction: MoveDirection::Start,
    };
    assert!(matches!(
        to_ipc_command(&cmd),
        IpcCommand::MoveColumnToStart
    ));
}

#[test]
fn test_to_ipc_command_move_end() {
    let cmd = Commands::Move {
        direction: MoveDirection::End,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::MoveColumnToEnd));
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
fn test_to_ipc_command_focus_monitor_up_down() {
    assert!(matches!(
        to_ipc_command(&Commands::FocusMonitor {
            direction: MonitorDirection::Up,
        }),
        IpcCommand::FocusMonitorUp
    ));
    assert!(matches!(
        to_ipc_command(&Commands::FocusMonitor {
            direction: MonitorDirection::Down,
        }),
        IpcCommand::FocusMonitorDown
    ));
}

#[test]
fn test_to_ipc_command_move_to_monitor_up_down() {
    assert!(matches!(
        to_ipc_command(&Commands::MoveToMonitor {
            direction: MonitorDirection::Up,
        }),
        IpcCommand::MoveWindowToMonitorUp
    ));
    assert!(matches!(
        to_ipc_command(&Commands::MoveToMonitor {
            direction: MonitorDirection::Down,
        }),
        IpcCommand::MoveWindowToMonitorDown
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
fn test_to_ipc_command_workspace_next() {
    assert!(matches!(
        to_ipc_command(&Commands::WorkspaceNext),
        IpcCommand::WorkspaceNext
    ));
}

#[test]
fn test_to_ipc_command_workspace_prev() {
    assert!(matches!(
        to_ipc_command(&Commands::WorkspacePrev),
        IpcCommand::WorkspacePrev
    ));
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
fn test_to_ipc_command_query_hotkeys() {
    let cmd = Commands::Query {
        what: QueryType::Hotkeys,
    };
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::QueryHotkeys));
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
fn test_to_ipc_command_release_all_windows() {
    let cmd = Commands::ReleaseAllWindows;
    assert!(matches!(
        to_ipc_command(&cmd),
        IpcCommand::ReleaseAllWindows
    ));
}

#[test]
fn test_to_ipc_command_toggle_ignore() {
    let cmd = Commands::ToggleIgnore;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::ToggleIgnore));
}

#[test]
fn test_to_ipc_command_panic_revert() {
    let cmd = Commands::PanicRevert;
    assert!(matches!(to_ipc_command(&cmd), IpcCommand::PanicRevert));
}

#[test]
fn test_cli_query_workspaces_parses() {
    let cli = Cli::try_parse_from(["leopardwm-cli", "query", "workspaces"])
        .expect("query workspaces must parse");
    assert!(matches!(
        cli.command,
        Commands::Query {
            what: QueryType::Workspaces
        }
    ));
}

#[test]
fn test_cli_workspace_monitor_target_parses() {
    let cli = Cli::try_parse_from([
        "leopardwm-cli",
        "workspace",
        "2",
        "--monitor",
        r"\\.\DISPLAY2",
    ])
    .expect("workspace --monitor must parse");
    match cli.command {
        Commands::Workspace { number, monitor } => {
            assert_eq!(number, 2);
            assert_eq!(monitor.as_deref(), Some(r"\\.\DISPLAY2"));
        }
        _ => panic!("expected workspace command"),
    }
}

#[test]
fn test_workspace_cli_dispatch_preserves_legacy_and_targets_named_monitor() {
    assert!(matches!(
        to_ipc_command(&Commands::Workspace {
            number: 4,
            monitor: None,
        }),
        IpcCommand::SwitchWorkspace { index: 4 }
    ));

    assert_eq!(
        to_ipc_command(&Commands::Workspace {
            number: 4,
            monitor: Some(r"\\.\DISPLAY2".to_string()),
        }),
        IpcCommand::SwitchWorkspaceOnMonitor {
            monitor_device_name: r"\\.\DISPLAY2".to_string(),
            index: 4,
        }
    );
}

#[test]
fn test_query_workspaces_dispatches_to_streaming_query_command() {
    assert!(matches!(
        to_ipc_command(&Commands::Query {
            what: QueryType::Workspaces,
        }),
        IpcCommand::QueryWorkspaceState
    ));
}

#[test]
fn test_subscribe_workspace_state_filter_parses() {
    let kinds = parse_event_kinds(Some(vec!["workspace_state".to_string()])).unwrap();
    assert_eq!(kinds.len(), 1);
    assert!(kinds.contains(&EventKind::WorkspaceState));
    assert!(parse_event_kinds(None).unwrap().is_empty());
}

#[tokio::test]
async fn test_workspace_query_consumes_ack_and_forwards_complete_snapshot() {
    let ack = "{\"status\":\"workspace_state_ready\",\"protocol_version\":4}\n";
    let events = concat!(
        "{\"type\":\"workspace_snapshot_begin\",\"protocol_version\":4,\"session_id\":\"session\",\"revision\":8,\"focused_monitor_device_name\":null}\n",
        "{\"type\":\"workspace_snapshot_chunk\",\"revision\":8,\"records\":[]}\n",
        "{\"type\":\"workspace_snapshot_end\",\"revision\":8}\n"
    );
    let input = format!("{ack}{events}");
    let mut reader = tokio::io::BufReader::new(input.as_bytes());
    let mut output = Vec::new();

    read_stream_ack(&mut reader, StreamAckKind::WorkspaceState, None)
        .await
        .unwrap();
    forward_event_frames(&mut reader, &mut output, EventReadMode::WorkspaceQuery)
        .await
        .unwrap();

    assert_eq!(output, events.as_bytes());
}

#[tokio::test]
async fn test_workspace_query_fails_on_incomplete_snapshot() {
    let events = "{\"type\":\"workspace_snapshot_begin\",\"protocol_version\":4,\"session_id\":\"session\",\"revision\":8,\"focused_monitor_device_name\":null}\n";
    let mut reader = tokio::io::BufReader::new(events.as_bytes());
    let mut output = Vec::new();

    let error = forward_event_frames(&mut reader, &mut output, EventReadMode::WorkspaceQuery)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("before completing"));
}

#[tokio::test(start_paused = true)]
async fn test_workspace_query_times_out_waiting_for_ack() {
    let (_writer, reader) = tokio::io::duplex(1024);
    let mut reader = tokio::io::BufReader::new(reader);
    let result = tokio::time::timeout(
        IPC_DEFAULT_RESPONSE_TIMEOUT * 2,
        read_stream_ack(&mut reader, StreamAckKind::WorkspaceState, None),
    )
    .await
    .expect("query acknowledgment must have its own deadline");
    assert!(result.unwrap_err().to_string().contains("Timed out"));
}

#[tokio::test(start_paused = true)]
async fn test_workspace_query_times_out_on_stalled_or_partial_frame() {
    use tokio::io::AsyncWriteExt;
    for prefix in ["", "{\"type\":\"workspace_snapshot_begin\""] {
        let (mut writer, reader) = tokio::io::duplex(1024);
        writer.write_all(prefix.as_bytes()).await.unwrap();
        let mut reader = tokio::io::BufReader::new(reader);
        let mut output = Vec::new();
        let result = tokio::time::timeout(
            IPC_DEFAULT_RESPONSE_TIMEOUT * 2,
            forward_event_frames(&mut reader, &mut output, EventReadMode::WorkspaceQuery),
        )
        .await
        .expect("query frames must have their own deadline");
        assert!(result.unwrap_err().to_string().contains("Timed out"));
        assert!(output.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn test_workspace_query_read_deadline_resets_for_each_frame() {
    use tokio::io::AsyncWriteExt;
    let frames = [
        "{\"type\":\"workspace_snapshot_begin\",\"protocol_version\":4,\"session_id\":\"session\",\"revision\":8,\"focused_monitor_device_name\":null}\n",
        "{\"type\":\"workspace_snapshot_chunk\",\"revision\":8,\"records\":[]}\n",
        "{\"type\":\"workspace_snapshot_end\",\"revision\":8}\n",
    ];
    let (mut writer, reader) = tokio::io::duplex(1024);
    let producer = tokio::spawn(async move {
        for frame in frames {
            tokio::time::sleep(IPC_DEFAULT_RESPONSE_TIMEOUT * 3 / 4).await;
            writer.write_all(frame.as_bytes()).await.unwrap();
        }
    });
    let mut reader = tokio::io::BufReader::new(reader);
    let mut output = Vec::new();
    forward_event_frames(&mut reader, &mut output, EventReadMode::WorkspaceQuery)
        .await
        .unwrap();
    producer.await.unwrap();
    assert_eq!(output, frames.concat().as_bytes());
}

#[tokio::test(start_paused = true)]
async fn test_subscription_allows_idle_longer_than_query_read_timeout() {
    use tokio::io::AsyncWriteExt;
    for workspace_state in [false, true] {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let producer = tokio::spawn(async move {
            tokio::time::sleep(IPC_DEFAULT_RESPONSE_TIMEOUT * 2).await;
            writer
                .write_all(b"{\"type\":\"heartbeat\",\"uptime_seconds\":10}\n")
                .await
                .unwrap();
        });
        let mut reader = tokio::io::BufReader::new(reader);
        let mut output = Vec::new();
        forward_event_frames(
            &mut reader,
            &mut output,
            EventReadMode::Subscribe { workspace_state },
        )
        .await
        .unwrap();
        producer.await.unwrap();
        assert_eq!(output, b"{\"type\":\"heartbeat\",\"uptime_seconds\":10}\n");
    }
}

#[tokio::test]
async fn test_workspace_subscription_rejects_orphan_chunk_and_end() {
    for event in [
        "{\"type\":\"workspace_snapshot_chunk\",\"revision\":8,\"records\":[]}\n",
        "{\"type\":\"workspace_snapshot_end\",\"revision\":8}\n",
    ] {
        let mut reader = tokio::io::BufReader::new(event.as_bytes());
        let mut output = Vec::new();
        let error = forward_event_frames(
            &mut reader,
            &mut output,
            EventReadMode::Subscribe {
                workspace_state: true,
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Unexpected or mismatched"));
        assert!(output.is_empty());
    }
}

#[tokio::test]
async fn test_workspace_query_forwards_error_frame_then_fails() {
    let event = "{\"type\":\"workspace_snapshot_error\",\"message\":\"record too large\"}\n";
    let mut reader = tokio::io::BufReader::new(event.as_bytes());
    let mut output = Vec::new();

    let error = forward_event_frames(&mut reader, &mut output, EventReadMode::WorkspaceQuery)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("record too large"));
    assert_eq!(output, event.as_bytes());
}

#[tokio::test]
async fn test_subscribe_ack_rejects_missing_workspace_state_capability() {
    let ack = "{\"status\":\"subscribed\",\"events\":[\"workspace\"]}\n";
    let mut reader = tokio::io::BufReader::new(ack.as_bytes());
    let required = [EventKind::WorkspaceState].into_iter().collect();

    let error = read_stream_ack(&mut reader, StreamAckKind::Subscribe, Some(&required))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("WorkspaceState"));
}

#[tokio::test]
async fn test_workspace_subscription_fails_on_partial_snapshot_eof() {
    let event = "{\"type\":\"workspace_snapshot_begin\",\"protocol_version\":4,\"session_id\":\"session\",\"revision\":8,\"focused_monitor_device_name\":null}\n";
    let mut reader = tokio::io::BufReader::new(event.as_bytes());
    let mut output = Vec::new();

    let error = forward_event_frames(
        &mut reader,
        &mut output,
        EventReadMode::Subscribe {
            workspace_state: true,
        },
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("during a workspace-state snapshot"));
}

#[tokio::test]
async fn test_stream_reader_rejects_frame_over_ipc_limit() {
    let mut event = vec![b'x'; MAX_IPC_MESSAGE_SIZE + 1];
    event.push(b'\n');
    let mut reader = tokio::io::BufReader::new(event.as_slice());
    let mut output = Vec::new();

    let error = forward_event_frames(
        &mut reader,
        &mut output,
        EventReadMode::Subscribe {
            workspace_state: false,
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("exceeded"));
    assert!(output.is_empty());
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
fn test_cli_release_all_windows_parses() {
    let cli = Cli::try_parse_from(["leopardwm-cli", "release-all-windows"])
        .expect("release command should parse");
    assert!(matches!(cli.command, Commands::ReleaseAllWindows));
}

#[test]
fn test_cli_help_lists_release_all_windows() {
    let help = Cli::command().render_help().to_string();
    assert!(help.contains("release-all-windows"));
}

#[test]
fn test_cli_toggle_ignore_parses() {
    let cli = Cli::try_parse_from(["leopardwm-cli", "toggle-ignore"])
        .expect("toggle-ignore should parse");
    assert!(matches!(cli.command, Commands::ToggleIgnore));
}

#[test]
fn test_cli_help_lists_toggle_ignore() {
    let help = Cli::command().render_help().to_string();
    assert!(help.contains("toggle-ignore"));
}

#[test]
fn test_cli_alias_restore_windows_parses_to_emergency_uncloak() {
    let cli =
        Cli::try_parse_from(["leopardwm-cli", "restore-windows"]).expect("alias should parse");
    assert!(matches!(cli.command, Commands::EmergencyUncloak));
}

#[test]
fn test_cli_export_shortcut_guide_install_parses() {
    let cli = Cli::try_parse_from(["leopardwm-cli", "export-shortcut-guide", "--install"])
        .expect("export command should parse");
    assert!(matches!(
        cli.command,
        Commands::ExportShortcutGuide {
            output: None,
            install: true
        }
    ));
}

#[test]
fn test_cli_export_shortcut_guide_rejects_output_with_install() {
    let result = Cli::try_parse_from([
        "leopardwm-cli",
        "export-shortcut-guide",
        "--output",
        "guide.yml",
        "--install",
    ]);
    assert!(result.is_err());
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
    assert!(is_non_success_response(&IpcResponse::error("apply failed")));
    assert!(!is_non_success_response(&IpcResponse::Ok));
    assert!(!is_non_success_response(&IpcResponse::ApplyPending {
        message: "Layout application remains pending while tiling is paused".to_string(),
    }));
}

fn restore_counter(
    restores: &std::cell::Cell<usize>,
) -> impl FnMut(&str) -> anyhow::Result<()> + '_ {
    move |_| {
        restores.set(restores.get() + 1);
        Ok(())
    }
}

#[test]
fn test_conclude_apply_pending_is_non_success_without_restore() {
    let restores = std::cell::Cell::new(0);
    let response = IpcResponse::ApplyPending {
        message: "Layout application remains pending while tiling is paused".to_string(),
    };
    let err = conclude_apply_command_response(&response, restore_counter(&restores))
        .expect_err("pending apply must be non-success");
    assert_eq!(restores.get(), 0);
    assert_eq!(err.to_string(), apply_pending_response_message());
    assert!(!apply_pending_response_message().contains("emergency"));
    assert!(!apply_pending_response_message().contains("restore"));
}

#[test]
fn test_conclude_apply_error_invokes_restore() {
    let restores = std::cell::Cell::new(0);
    let err = conclude_apply_command_response(
        &IpcResponse::error("Failed to apply layout: boom"),
        restore_counter(&restores),
    )
    .expect_err("error apply must be non-success");
    assert_eq!(restores.get(), 1);
    assert_eq!(err.to_string(), apply_error_response_recovery_message());
}

#[test]
fn test_conclude_apply_unknown_invokes_restore() {
    let restores = std::cell::Cell::new(0);
    let err = conclude_apply_command_response(&IpcResponse::Unknown, restore_counter(&restores))
        .expect_err("unknown apply must be non-success");
    assert_eq!(restores.get(), 1);
    assert_eq!(err.to_string(), apply_error_response_recovery_message());
}

#[test]
fn test_conclude_apply_ok_does_not_restore() {
    let restores = std::cell::Cell::new(0);
    conclude_apply_command_response(&IpcResponse::Ok, restore_counter(&restores)).unwrap();
    assert_eq!(restores.get(), 0);
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
fn test_apply_pending_response_message_is_pending_without_restore() {
    let message = apply_pending_response_message();
    assert!(message.contains("pending"));
    assert!(!message.contains("emergency"));
    assert!(!message.contains("restore"));
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
fn test_parse_ipc_response_line_parses_error_and_apply_pending() {
    let error_raw = serde_json::to_string(&IpcResponse::error("Failed to apply layout")).unwrap();
    assert!(matches!(
        parse_ipc_response_line(&error_raw).unwrap(),
        IpcResponse::Error { .. }
    ));

    let pending_raw = serde_json::to_string(&IpcResponse::ApplyPending {
        message: "Layout application remains pending while tiling is paused".to_string(),
    })
    .unwrap();
    assert!(matches!(
        parse_ipc_response_line(&pending_raw).unwrap(),
        IpcResponse::ApplyPending { .. }
    ));
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
    assert!(
        !config.contains("toggle_ignore"),
        "toggle_ignore has no default binding and must not appear in the generated template"
    );
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
    let pass = CheckResult::Pass("test pass".to_string());
    let warn = CheckResult::Warn("test warn".to_string());
    let fail = CheckResult::Fail("test fail".to_string());
    pass.print();
    warn.print();
    fail.print();
}

#[test]
fn test_integrity_rendering_medium_daemon_high_cli() {
    assert_eq!(
        format_integrity_line("Daemon", Some(leopardwm_platform_win32::INTEGRITY_MEDIUM)),
        "Daemon integrity: Medium"
    );
    assert_eq!(
        format_integrity_line("CLI", Some(leopardwm_platform_win32::INTEGRITY_HIGH)),
        "CLI integrity: High"
    );
    assert_eq!(
        integrity_check("Daemon", Some(leopardwm_platform_win32::INTEGRITY_MEDIUM)),
        CheckResult::Pass("Daemon integrity: Medium".to_string())
    );
    assert_eq!(
        integrity_check("CLI", Some(leopardwm_platform_win32::INTEGRITY_HIGH)),
        CheckResult::Pass("CLI integrity: High".to_string())
    );
}

#[test]
fn test_integrity_rendering_high_daemon_medium_cli() {
    assert_eq!(
        format_integrity_line("Daemon", Some(leopardwm_platform_win32::INTEGRITY_HIGH)),
        "Daemon integrity: High"
    );
    assert_eq!(
        format_integrity_line("CLI", Some(leopardwm_platform_win32::INTEGRITY_MEDIUM)),
        "CLI integrity: Medium"
    );
}

#[test]
fn test_integrity_unavailable_missing_and_unknown_rid() {
    assert_eq!(format_integrity_rid(None), "unavailable");
    assert_eq!(
        integrity_check("Daemon", None),
        CheckResult::Warn("Daemon integrity: unavailable".to_string())
    );
    assert_eq!(
        format_integrity_line("CLI", None),
        "CLI integrity: unavailable"
    );
    assert_eq!(format_integrity_rid(Some(0x4000)), "0x4000");
    assert_eq!(
        format_integrity_line("Daemon", Some(0x4000)),
        "Daemon integrity: 0x4000"
    );
}

#[test]
fn test_blocked_windows_empty_current_record_wording() {
    let empty = blocked_windows_check(Some(&[]), &[]);
    assert_eq!(
        empty,
        CheckResult::Pass(
            "No privilege-blocked windows currently recorded by the daemon".to_string()
        )
    );
    assert_eq!(blocked_windows_check(None, &[]), empty);
    match empty {
        CheckResult::Pass(msg) | CheckResult::Warn(msg) | CheckResult::Fail(msg) => {
            assert!(!msg.contains("since daemon start"));
        }
    }
}

#[test]
fn test_blocked_windows_nonempty_mixed_reasons_and_advice() {
    let records = [
        ElevationBlockedWindow {
            hwnd: 0x10,
            title: "Admin".to_string(),
            reason: ElevationBlockReason::HigherIntegrity,
        },
        ElevationBlockedWindow {
            hwnd: 0x20,
            title: "Zed".to_string(),
            reason: ElevationBlockReason::Protected,
        },
        ElevationBlockedWindow {
            hwnd: 0x30,
            title: "Mystery".to_string(),
            reason: ElevationBlockReason::Unknown,
        },
    ];
    let result = blocked_windows_check(Some(&records), &[(0x10, "Admin".to_string())]);
    let CheckResult::Warn(msg) = result else {
        panic!("expected Warn, got {result:?}");
    };
    assert!(msg.contains("3 window(s) currently recorded as privilege-blocked at admission"));
    assert!(msg.contains("snapshot, not a live reclassification"));
    assert!(msg.contains("\"Admin\" (hwnd 0x10, higher integrity"));
    assert!(msg.contains(
        "running LeopardWM elevated can help, but is not guaranteed if the target is System"
    ));
    assert!(msg.contains(
        "\"Zed\" (hwnd 0x20, protected/access-denied/unreadable token; elevation may not help)"
    ));
    assert!(msg.contains("\"Mystery\" (hwnd 0x30, unknown admission-time reason)"));
    assert!(!msg.contains("since daemon start"));
    assert!(!msg.contains("can't be tiled regardless"));

    let CheckResult::Warn(protected_msg) = blocked_windows_check(Some(&records[1..2]), &[]) else {
        panic!("expected Warn for Protected");
    };
    assert!(protected_msg.contains("may not help"));
    assert!(!protected_msg.contains("can help"));
    assert!(!protected_msg.contains("administrator"));
    assert!(!protected_msg.contains("can't be tiled regardless"));

    let CheckResult::Warn(unknown_msg) = blocked_windows_check(Some(&records[2..]), &[]) else {
        panic!("expected Warn for Unknown");
    };
    assert!(unknown_msg.contains("unknown admission-time reason"));
    assert!(!unknown_msg.contains("can help"));
    assert!(!unknown_msg.contains("administrator"));
    assert!(!unknown_msg.contains("elevat"));
}

#[test]
fn test_blocked_windows_legacy_old_ipc_does_not_guess_reason() {
    let legacy = vec![(0x10, "Admin".to_string()), (0x20, "Zed".to_string())];
    let result = blocked_windows_check(None, &legacy);
    let CheckResult::Warn(msg) = result else {
        panic!("expected Warn, got {result:?}");
    };
    assert!(msg.contains("admission-time reason unavailable"));
    assert!(msg.contains("\"Admin\" (hwnd 0x10)"));
    assert!(msg.contains("\"Zed\" (hwnd 0x20)"));
    assert!(!msg.contains("higher integrity"));
    assert!(!msg.contains("can help"));
    assert!(!msg.contains("administrator"));
    assert!(!msg.contains("protected"));
}

#[test]
fn test_get_windows_version_does_not_panic() {
    let version = get_windows_version();
    assert!(!version.is_empty());
}

// =========================================================================
// CLI completeness tests
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

    fs::write(&config_path, "gap = 10\n").unwrap();

    fs::copy(&config_path, &backup_path).unwrap();
    assert!(backup_path.exists());

    fs::write(&config_path, "gap = 20\n").unwrap();

    fs::copy(&backup_path, &config_path).unwrap();
    let restored = fs::read_to_string(&config_path).unwrap();
    assert_eq!(restored, "gap = 10\n");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_handle_collect_logs_does_not_panic() {
    let result = handle_collect_logs();
    assert!(result.is_ok());
}

#[test]
fn collect_logs_includes_full_gesture_capture_not_last_100() {
    let dir = std::env::temp_dir().join(format!(
        "lwm-collect-logs-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    let daemon = dir.join("leopardwm-daemon.log");
    let capture = dir.join(leopardwm_ipc::GESTURE_CAPTURE_LOG_FILE);
    let daemon_body: String = (0..150).map(|i| format!("daemon-line-{i}\n")).collect();
    let capture_body: String = (0..150).map(|i| format!("capture-line-{i}\n")).collect();
    fs::write(&daemon, &daemon_body).unwrap();
    fs::write(&capture, &capture_body).unwrap();

    let daemon_section = format_file_section("Daemon Log", &daemon, Some(100));
    let capture_section = format_file_section("Gesture Capture", &capture, None);

    assert!(
        daemon_section.contains("daemon-line-149"),
        "{daemon_section}"
    );
    assert!(
        !daemon_section.contains("daemon-line-0"),
        "{daemon_section}"
    );
    assert!(
        daemon_section.contains("earlier lines omitted"),
        "{daemon_section}"
    );
    assert!(
        capture_section.contains("capture-line-0"),
        "{capture_section}"
    );
    assert!(
        capture_section.contains("capture-line-149"),
        "{capture_section}"
    );
    assert!(
        !capture_section.contains("earlier lines omitted"),
        "{capture_section}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn test_config_action_variants_parse() {
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
// Profile config tests
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
