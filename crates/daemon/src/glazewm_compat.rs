mod peer;
mod server;
pub(crate) use server::Server;

use leopardwm_ipc::{IpcCommand, WorkspaceStateRecord, WorkspaceStateSnapshot};
use serde_json::{json, Value};
use std::collections::HashMap;

const UNSUPPORTED: &str = "not supported by LeopardWM";

#[derive(Debug, PartialEq, Eq)]
enum Action<'a> {
    QueryMonitors,
    Subscribe(Vec<&'a str>),
    Unsubscribe(&'a str),
    FocusWorkspace(&'a str),
    Next,
    Previous,
}

fn parse(message: &str) -> Result<Action<'_>, &'static str> {
    let words: Vec<_> = message.split_whitespace().collect();
    match words.as_slice() {
        ["query", "monitors"] => Ok(Action::QueryMonitors),
        ["sub", "-e" | "--events", events @ ..] if !events.is_empty() => {
            let valid = [
                "workspace_activated",
                "workspace_deactivated",
                "workspace_updated",
                "focus_changed",
                "focused_container_moved",
                "all",
            ];
            if events.iter().all(|event| valid.contains(event)) {
                Ok(Action::Subscribe(events.to_vec()))
            } else {
                Err(UNSUPPORTED)
            }
        }
        ["unsub", "--id", id] if valid_uuid(id) => Ok(Action::Unsubscribe(id)),
        ["command", "focus", "--workspace", name] => Ok(Action::FocusWorkspace(name)),
        ["command", "focus", "--next-active-workspace-on-monitor" | "--next-active-workspace"] => {
            Ok(Action::Next)
        }
        ["command", "focus", "--prev-active-workspace-on-monitor" | "--prev-active-workspace"] => {
            Ok(Action::Previous)
        }
        _ => Err(UNSUPPORTED),
    }
}

fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

fn response(message: &str, result: Result<Value, &str>) -> Value {
    let (success, error, data) = match result {
        Ok(data) => (true, Value::Null, data),
        Err(error) => (false, json!(error), Value::Null),
    };
    json!({"messageType":"client_response", "clientMessage":message, "success":success, "error":error, "data":data})
}

struct Monitor<'a> {
    device: &'a str,
    id: i64,
    active: u8,
}

fn ordered_monitors<'a>(
    snapshot: &'a WorkspaceStateSnapshot,
    positions: &HashMap<i64, (i32, i32)>,
) -> Result<Vec<Monitor<'a>>, &'static str> {
    let mut monitors = Vec::new();
    for record in &snapshot.records {
        if let WorkspaceStateRecord::Monitor {
            monitor_device_name,
            monitor_id,
            active_workspace_index,
        } = record
        {
            if !positions.contains_key(monitor_id) {
                return Err("monitor position unavailable");
            }
            monitors.push(Monitor {
                device: monitor_device_name,
                id: *monitor_id,
                active: *active_workspace_index,
            });
        }
    }
    monitors.sort_by(|a, b| {
        positions[&a.id]
            .cmp(&positions[&b.id])
            .then(a.device.cmp(b.device))
    });
    Ok(monitors)
}

fn monitors_dto(
    snapshot: &WorkspaceStateSnapshot,
    positions: &HashMap<i64, (i32, i32)>,
) -> Result<Vec<Value>, &'static str> {
    let monitors = ordered_monitors(snapshot, positions)?;
    let mut result = Vec::new();
    for (ordinal, monitor) in monitors.iter().enumerate() {
        let mut children = Vec::new();
        for record in &snapshot.records {
            if let WorkspaceStateRecord::Workspace {
                monitor_device_name,
                workspace_index,
                name,
                ..
            } = record
            {
                if monitor_device_name != monitor.device {
                    continue;
                }
                let windows: Vec<_> = snapshot.records.iter().filter_map(|record| match record {
                    WorkspaceStateRecord::Window { monitor_device_name, workspace_index: index, hwnd, is_floating, .. } if monitor_device_name == monitor.device && index == workspace_index => Some(json!({
                        "type":"window", "id":format!("00000000-0000-0000-{:04x}-{:012x}", (hwnd >> 48) & 0xffff, hwnd & 0xffffffffffff),
                        "handle":hwnd, "title":"", "className":"", "processName":"", "displayState":"shown", "state":{"type":if *is_floating {"floating"} else {"tiling"}}
                    })),
                    _ => None,
                }).collect();
                let displayed = *workspace_index == monitor.active;
                if !displayed && windows.is_empty() {
                    continue;
                }
                let number = usize::from(*workspace_index) + 1;
                children.push(json!({"type":"workspace", "name":format!("{}-{number}", ordinal + 1), "displayName":name.clone().unwrap_or_else(|| number.to_string()), "hasFocus":displayed && snapshot.focused_monitor_device_name.as_deref() == Some(monitor.device), "isDisplayed":displayed, "children":windows}));
            }
        }
        result.push(json!({"type":"monitor", "handle":monitor.id, "hardwareId":monitor.device, "children":children}));
    }
    Ok(result)
}

fn focus_command(
    name: &str,
    snapshot: &WorkspaceStateSnapshot,
    positions: &HashMap<i64, (i32, i32)>,
) -> Result<IpcCommand, &'static str> {
    let (ordinal, index) = name.split_once('-').ok_or("invalid workspace name")?;
    let ordinal: usize = ordinal.parse().map_err(|_| "invalid workspace name")?;
    let index: u8 = index.parse().map_err(|_| "invalid workspace name")?;
    if ordinal == 0 || !(1..=9).contains(&index) || format!("{ordinal}-{index}") != name {
        return Err("invalid workspace name");
    }
    let monitors = ordered_monitors(snapshot, positions)?;
    let monitor = monitors.get(ordinal - 1).ok_or("monitor not found")?;
    Ok(IpcCommand::SwitchWorkspaceOnMonitor {
        monitor_device_name: monitor.device.to_string(),
        index,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yasb_commands_and_rejections_preserve_messages() {
        let cases = [
            ("sub -e workspace_activated workspace_deactivated workspace_updated focus_changed focused_container_moved", Action::Subscribe(vec!["workspace_activated", "workspace_deactivated", "workspace_updated", "focus_changed", "focused_container_moved"])),
            ("query monitors", Action::QueryMonitors),
            ("command focus --workspace 1-3", Action::FocusWorkspace("1-3")),
            ("command focus --next-active-workspace-on-monitor", Action::Next),
            ("command focus --prev-active-workspace-on-monitor", Action::Previous),
            ("command focus --next-active-workspace", Action::Next),
            ("command focus --prev-active-workspace", Action::Previous),
            ("sub --events all", Action::Subscribe(vec!["all"])),
            ("unsub --id 00000000-0000-0000-0000-000000000001", Action::Unsubscribe("00000000-0000-0000-0000-000000000001")),
        ];
        for (message, expected) in cases {
            assert_eq!(parse(message), Ok(expected));
        }
        for message in [
            "  query windows\n",
            "sub -e",
            "unsub --id invalid",
            "command focus --workspace",
        ] {
            let reply = response(message, Err(parse(message).unwrap_err()));
            assert_eq!(reply["clientMessage"], message);
            assert_eq!(reply["success"], false);
            assert_eq!(reply["error"], UNSUPPORTED);
        }
    }

    fn fixture() -> (WorkspaceStateSnapshot, HashMap<i64, (i32, i32)>) {
        let mut records = Vec::new();
        for (device, id, active) in [("DISPLAY1", 11, 0), ("DISPLAY2", 22, 2)] {
            records.push(WorkspaceStateRecord::Monitor {
                monitor_device_name: device.into(),
                monitor_id: id,
                active_workspace_index: active,
            });
            for index in 0..3 {
                records.push(WorkspaceStateRecord::Workspace {
                    monitor_device_name: device.into(),
                    workspace_index: index,
                    name: (index == 0).then(|| "code".into()),
                    rows: None,
                });
            }
        }
        records.push(WorkspaceStateRecord::Window {
            monitor_device_name: "DISPLAY1".into(),
            workspace_index: 1,
            hwnd: 42,
            is_floating: true,
            is_sticky: true,
            row_index: None,
        });
        records.push(WorkspaceStateRecord::Window {
            monitor_device_name: "DISPLAY2".into(),
            workspace_index: 2,
            hwnd: 43,
            is_floating: false,
            is_sticky: false,
            row_index: None,
        });
        (
            WorkspaceStateSnapshot {
                session_id: "s".into(),
                revision: 1,
                focused_monitor_device_name: Some("DISPLAY2".into()),
                records,
            },
            [(11, (100, 0)), (22, (0, 0))].into_iter().collect(),
        )
    }

    #[test]
    fn monitor_mapping_filters_and_names_workspaces_by_position() {
        let (snapshot, positions) = fixture();
        let result = monitors_dto(&snapshot, &positions).unwrap();
        assert_eq!(result[0]["hardwareId"], "DISPLAY2");
        assert_eq!(result[0]["handle"], 22);
        assert_eq!(result[0]["children"].as_array().unwrap().len(), 1);
        assert_eq!(result[0]["children"][0]["name"], "1-3");
        assert_eq!(result[0]["children"][0]["displayName"], "3");
        assert_eq!(result[0]["children"][0]["hasFocus"], true);
        assert_eq!(result[0]["children"][0]["isDisplayed"], true);
        assert_eq!(
            result[0]["children"][0]["children"][0]["state"]["type"],
            "tiling"
        );
        assert_eq!(result[1]["children"].as_array().unwrap().len(), 2);
        assert_eq!(result[1]["children"][0]["name"], "2-1");
        assert_eq!(result[1]["children"][0]["displayName"], "code");
        let window = &result[1]["children"][1]["children"][0];
        assert_eq!(window["type"], "window");
        assert!(valid_uuid(window["id"].as_str().unwrap()));
        assert_eq!(window["handle"], 42);
        assert_eq!(window["title"], "");
        assert_eq!(window["className"], "");
        assert_eq!(window["processName"], "");
        assert_eq!(window["displayState"], "shown");
        assert_eq!(window["state"]["type"], "floating");
        let workspaces: Vec<_> = result
            .iter()
            .flat_map(|m| m["children"].as_array().unwrap())
            .collect();
        assert_eq!(
            workspaces.iter().filter(|w| w["hasFocus"] == true).count(),
            1
        );
        let names: std::collections::HashSet<_> = workspaces
            .iter()
            .map(|w| w["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), workspaces.len());
    }

    #[test]
    fn workspace_command_uses_screen_ordinal_without_fallback() {
        let (snapshot, positions) = fixture();
        assert!(
            matches!(focus_command("2-3", &snapshot, &positions), Ok(IpcCommand::SwitchWorkspaceOnMonitor { monitor_device_name, index: 3 }) if monitor_device_name == "DISPLAY1")
        );
        for name in ["3-3", "0-1", "1-0", "1-10", "01-1", "x-1", "1-"] {
            assert!(
                focus_command(name, &snapshot, &positions).is_err(),
                "{name}"
            );
        }
    }
}
