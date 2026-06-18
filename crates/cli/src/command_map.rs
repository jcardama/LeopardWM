//! Mapping from parsed CLI commands to IPC commands.

use crate::args::*;
use leopardwm_ipc::IpcCommand;

/// Convert CLI command to IPC command.
pub(crate) fn to_ipc_command(cmd: &Commands) -> IpcCommand {
    match cmd {
        Commands::Focus { direction } => match direction {
            FocusDirection::Left => IpcCommand::FocusLeft,
            FocusDirection::Right => IpcCommand::FocusRight,
            FocusDirection::Up => IpcCommand::FocusUp,
            FocusDirection::Down => IpcCommand::FocusDown,
            FocusDirection::Start => IpcCommand::FocusStart,
            FocusDirection::End => IpcCommand::FocusEnd,
        },
        Commands::Scroll { direction } => match direction {
            ScrollDirection::Left { pixels } => IpcCommand::Scroll {
                delta: -(*pixels as f64),
            },
            ScrollDirection::Right { pixels } => IpcCommand::Scroll {
                delta: *pixels as f64,
            },
        },
        Commands::Move { direction } => match direction {
            MoveDirection::Left => IpcCommand::MoveColumnLeft,
            MoveDirection::Right => IpcCommand::MoveColumnRight,
            MoveDirection::Start => IpcCommand::MoveColumnToStart,
            MoveDirection::End => IpcCommand::MoveColumnToEnd,
        },
        Commands::MoveWindow { direction } => match direction {
            MoveWindowDirection::Left => IpcCommand::MoveWindowLeft,
            MoveWindowDirection::Right => IpcCommand::MoveWindowRight,
            MoveWindowDirection::Up => IpcCommand::MoveWindowUp,
            MoveWindowDirection::Down => IpcCommand::MoveWindowDown,
        },
        Commands::Expel { direction } => match direction {
            ExpelDirection::Left => IpcCommand::ExpelToLeft,
            ExpelDirection::Right => IpcCommand::ExpelToRight,
        },
        Commands::Consume { direction } => match direction {
            ConsumeDirection::Left => IpcCommand::ConsumeFromLeft,
            ConsumeDirection::Right => IpcCommand::ConsumeFromRight,
        },
        Commands::Resize { delta } => IpcCommand::Resize { delta: *delta },
        Commands::FocusMonitor { direction } => match direction {
            MonitorDirection::Left => IpcCommand::FocusMonitorLeft,
            MonitorDirection::Right => IpcCommand::FocusMonitorRight,
        },
        Commands::MoveToMonitor { direction } => match direction {
            MonitorDirection::Left => IpcCommand::MoveWindowToMonitorLeft,
            MonitorDirection::Right => IpcCommand::MoveWindowToMonitorRight,
        },
        Commands::Query { what } => match what {
            QueryType::Workspace => IpcCommand::QueryWorkspace,
            QueryType::Focused => IpcCommand::QueryFocused,
            QueryType::All => IpcCommand::QueryAllWindows,
        },
        Commands::Refresh => IpcCommand::Refresh,
        Commands::Reload => IpcCommand::Reload,
        Commands::CloseWindow => IpcCommand::CloseWindow,
        Commands::ToggleFloating => IpcCommand::ToggleFloating,
        Commands::ToggleFullscreen => IpcCommand::ToggleFullscreen,
        Commands::ScratchpadStash => IpcCommand::ScratchpadStash,
        Commands::ScratchpadToggle => IpcCommand::ScratchpadToggle,
        Commands::ToggleSticky => IpcCommand::ToggleSticky,
        Commands::ToggleNewWindowPlacement => IpcCommand::ToggleNewWindowPlacement,
        Commands::ToggleTabbed => IpcCommand::ToggleTabbed,
        Commands::SetWidth { fraction } => IpcCommand::SetColumnWidth {
            fraction: *fraction,
        },
        Commands::CenterColumn => IpcCommand::CenterColumn,
        Commands::MaximizeColumn => IpcCommand::MaximizeColumn,
        Commands::EqualizeWidths => IpcCommand::EqualizeColumnWidths,
        Commands::CycleWidthUp => IpcCommand::CycleWidthUp,
        Commands::CycleWidthDown => IpcCommand::CycleWidthDown,
        Commands::CycleHeightUp => IpcCommand::CycleHeightUp,
        Commands::CycleHeightDown => IpcCommand::CycleHeightDown,
        Commands::EqualizeHeights => IpcCommand::EqualizeColumnHeights,
        Commands::Workspace { number } => IpcCommand::SwitchWorkspace { index: *number },
        Commands::MoveToWorkspace { number } => IpcCommand::MoveToWorkspace { index: *number },
        Commands::WorkspaceNext => IpcCommand::WorkspaceNext,
        Commands::WorkspacePrev => IpcCommand::WorkspacePrev,
        Commands::ToggleOverview => IpcCommand::ToggleOverview,
        Commands::Status => IpcCommand::QueryStatus,
        Commands::PanicRevert => IpcCommand::PanicRevert,
        Commands::Run { .. } => unreachable!("Run handled separately"),
        Commands::Subscribe { .. } => unreachable!("Subscribe handled separately"),
        Commands::Doctor => unreachable!("Doctor handled separately"),
        Commands::Autostart { .. } => unreachable!("Autostart handled separately"),
        Commands::CollectLogs => unreachable!("CollectLogs handled separately"),
        Commands::Config { .. } => unreachable!("Config handled separately"),
        Commands::EmergencyUncloak => unreachable!("EmergencyUncloak handled separately"),
        Commands::Stop => IpcCommand::Stop,
        Commands::TogglePause => IpcCommand::TogglePause,
        Commands::Ghost { action } => IpcCommand::SetGhostAnimation {
            enabled: match action {
                GhostAction::Enable => Some(true),
                GhostAction::Disable => Some(false),
                GhostAction::Status => None,
            },
        },
    }
}
