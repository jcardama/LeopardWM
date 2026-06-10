//! Human-readable formatting of daemon IPC responses.

use leopardwm_ipc::IpcResponse;

/// Print a response in a human-readable format.
pub(crate) fn print_response(response: &IpcResponse) {
    match response {
        IpcResponse::Ok => {
            println!("OK");
        }
        IpcResponse::Error { message } => {
            eprintln!("Error: {}", message);
        }
        IpcResponse::WorkspaceState {
            columns,
            windows,
            focused_column,
            focused_window,
            scroll_offset,
            total_width,
            active_workspace,
            active_workspace_name,
        } => {
            println!("Workspace State:");
            match active_workspace_name {
                Some(name) => println!("  Active workspace: {} ({})", active_workspace, name),
                None => println!("  Active workspace: {}", active_workspace),
            }
            println!("  Columns: {}", columns);
            println!("  Windows: {}", windows);
            println!("  Focused column: {}", focused_column);
            println!("  Focused window in column: {}", focused_window);
            println!("  Scroll offset: {:.1}", scroll_offset);
            println!("  Total width: {}", total_width);
        }
        IpcResponse::FocusedWindow {
            window_id,
            column_index,
            window_index,
        } => {
            println!("Focused Window:");
            match window_id {
                Some(id) => println!("  Window ID: {}", id),
                None => println!("  No window focused"),
            }
            println!("  Column index: {}", column_index);
            println!("  Window index: {}", window_index);
        }
        IpcResponse::WindowList { windows } => {
            println!("Managed Windows ({} total):", windows.len());
            for win in windows {
                let location = if win.is_floating {
                    "floating".to_string()
                } else {
                    format!(
                        "col {} win {}",
                        win.column_index.unwrap_or(0),
                        win.window_index.unwrap_or(0)
                    )
                };
                let focus_marker = if win.is_focused { " [FOCUSED]" } else { "" };
                println!(
                    "  {} - {} ({}) [{}]{}",
                    win.window_id, win.title, win.executable, location, focus_marker
                );
            }
        }
        IpcResponse::FocusedWindowInfo { window } => match window {
            Some(win) => {
                println!("Focused Window Info:");
                println!("  Window ID: {}", win.window_id);
                println!("  Title: {}", win.title);
                println!("  Class: {}", win.class_name);
                println!("  Executable: {}", win.executable);
                println!("  Position: ({}, {})", win.rect.x, win.rect.y);
                println!("  Size: {}x{}", win.rect.width, win.rect.height);
                println!("  Monitor: {}", win.monitor_id);
                if win.is_floating {
                    println!("  Layout: floating");
                } else {
                    println!(
                        "  Layout: tiled (col {}, win {})",
                        win.column_index.unwrap_or(0),
                        win.window_index.unwrap_or(0)
                    );
                }
            }
            None => {
                println!("No window is currently focused");
            }
        },
        IpcResponse::StatusInfo {
            version,
            monitors,
            total_windows,
            uptime_seconds,
        } => {
            println!("LeopardWM Daemon Status:");
            println!("  Version: {}", version);
            println!("  Monitors: {}", monitors);
            println!("  Total windows: {}", total_windows);
            let hours = uptime_seconds / 3600;
            let mins = (uptime_seconds % 3600) / 60;
            let secs = uptime_seconds % 60;
            println!("  Uptime: {}h {}m {}s", hours, mins, secs);
        }
        IpcResponse::HealthInfo { .. } => {
            // Health command removed; display as generic success if received
            println!("OK");
        }
        IpcResponse::AutoStartState { enabled } => {
            println!("Auto-start: {}", if *enabled { "enabled" } else { "disabled" });
        }
        IpcResponse::BoolValue { value } => {
            println!("{}", if *value { "enabled" } else { "disabled" });
        }
        IpcResponse::Subscribed { events } => {
            // Reaching this arm via the normal command path means the
            // caller used send_command for Subscribe, which is wrong —
            // Subscribe transitions the connection to stream mode and the
            // dedicated `lwm subscribe` subcommand handles that flow.
            println!("Subscribed (events: {:?}); stream mode active", events);
        }
        IpcResponse::Unknown => {
            println!("Daemon returned an unknown response status (client/daemon version mismatch)");
        }
    }
}
