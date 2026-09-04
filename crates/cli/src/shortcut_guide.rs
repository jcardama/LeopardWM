//! PowerToys Shortcut Guide manifest export.

use crate::ipc_client::send_command;
use anyhow::{Context, Result};
use directories::BaseDirs;
use leopardwm_ipc::{HotkeyBindingInfo, HotkeyIssue, IpcCommand, IpcResponse};
use leopardwm_platform_win32::parse_hotkey_string;
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as IoWrite};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{
    MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
};

const MANIFEST_FILE_NAME: &str = "LeopardWM.LeopardWM.en-US.yml";

#[derive(Debug, Clone, PartialEq, Eq)]
struct PowerToysChord {
    win: bool,
    ctrl: bool,
    alt: bool,
    shift: bool,
    key: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PowerToysAction {
    name: String,
    chords: Vec<PowerToysChord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PowerToysSection {
    name: String,
    actions: Vec<PowerToysAction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedManifest {
    yaml: String,
    warnings: Vec<String>,
}

pub(crate) async fn handle_export_shortcut_guide(
    output: Option<PathBuf>,
    install: bool,
) -> Result<()> {
    let response = send_command(IpcCommand::QueryHotkeys)
        .await
        .context("Could not query hotkeys. Start LeopardWM with 'lwm run' and try again.")?;

    let (hotkeys, issues) = match response {
        IpcResponse::HotkeyList {
            hotkeys, issues, ..
        } => (hotkeys, issues),
        IpcResponse::Error { message } => anyhow::bail!("Failed to query hotkeys: {message}"),
        other => anyhow::bail!("Unexpected daemon response while querying hotkeys: {other:?}"),
    };

    emit_query_issues(&issues);
    let rendered = render_manifest(&hotkeys);
    for warning in &rendered.warnings {
        eprintln!("Warning: {warning}");
    }

    let destination = if install {
        Some(default_install_path()?)
    } else {
        output
    };

    if let Some(path) = destination {
        write_atomic(&path, rendered.yaml.as_bytes())?;
        println!("Shortcut Guide manifest written to: {}", path.display());
    } else {
        print!("{}", rendered.yaml);
        io::stdout()
            .flush()
            .context("Failed to flush manifest output")?;
    }

    Ok(())
}

fn emit_query_issues(issues: &[HotkeyIssue]) {
    for issue in issues {
        eprintln!(
            "Warning: {} -> {}: {}",
            issue.binding, issue.action_id, issue.message
        );
    }
}

fn render_manifest(hotkeys: &[HotkeyBindingInfo]) -> RenderedManifest {
    let mut sections: Vec<PowerToysSection> = Vec::new();
    let mut section_indexes: HashMap<String, usize> = HashMap::new();
    let mut warnings = Vec::new();

    for hotkey in hotkeys {
        if !hotkey.enabled {
            continue;
        }

        let mut chords = Vec::new();
        for binding in &hotkey.bindings {
            let Some((modifiers, key)) = parse_hotkey_string(binding) else {
                warnings.push(format!(
                    "skipped invalid binding '{}' for {}",
                    binding, hotkey.action_id
                ));
                continue;
            };

            if modifiers.fn_mods != 0 {
                warnings.push(format!(
                    "skipped '{}' for {} because PowerToys cannot represent F13-F24 modifiers",
                    binding, hotkey.action_id
                ));
                continue;
            }

            chords.push(PowerToysChord {
                win: modifiers.win,
                ctrl: modifiers.ctrl,
                alt: modifiers.alt,
                shift: modifiers.shift,
                key,
            });
        }

        if chords.is_empty() {
            continue;
        }

        let action = PowerToysAction {
            name: hotkey.label.clone(),
            chords,
        };
        if let Some(index) = section_indexes.get(&hotkey.group).copied() {
            sections[index].actions.push(action);
        } else {
            section_indexes.insert(hotkey.group.clone(), sections.len());
            sections.push(PowerToysSection {
                name: hotkey.group.clone(),
                actions: vec![action],
            });
        }
    }

    RenderedManifest {
        yaml: render_yaml(&sections),
        warnings,
    }
}

fn render_yaml(sections: &[PowerToysSection]) -> String {
    let mut out = String::new();
    writeln!(out, "PackageName: LeopardWM.LeopardWM").expect("write to string");
    writeln!(out, "Name: LeopardWM").expect("write to string");
    writeln!(out, "BackgroundProcess: true").expect("write to string");
    writeln!(out, "WindowFilter: \"leopardwm.exe\"").expect("write to string");

    if sections.is_empty() {
        writeln!(out, "Shortcuts: []").expect("write to string");
        return out;
    }

    writeln!(out, "Shortcuts:").expect("write to string");
    for section in sections {
        writeln!(out, "  - SectionName: {}", yaml_quote(&section.name)).expect("write to string");
        writeln!(out, "    Properties:").expect("write to string");
        for action in &section.actions {
            writeln!(out, "      - Name: {}", yaml_quote(&action.name)).expect("write to string");
            writeln!(out, "        Shortcut:").expect("write to string");
            for chord in &action.chords {
                writeln!(out, "          - Win: {}", chord.win).expect("write to string");
                writeln!(out, "            Ctrl: {}", chord.ctrl).expect("write to string");
                writeln!(out, "            Alt: {}", chord.alt).expect("write to string");
                writeln!(out, "            Shift: {}", chord.shift).expect("write to string");
                writeln!(out, "            Keys:").expect("write to string");
                writeln!(out, "              - {}", chord.key).expect("write to string");
            }
        }
    }
    out
}

fn yaml_quote(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn default_install_path() -> Result<PathBuf> {
    let base = BaseDirs::new().context("Could not determine the local application-data path")?;
    Ok(install_path_from_local_app_data(base.data_local_dir()))
}

fn install_path_from_local_app_data(local_app_data: &Path) -> PathBuf {
    local_app_data
        .join("Microsoft")
        .join("WinGet")
        .join("KeyboardShortcuts")
        .join(MANIFEST_FILE_NAME)
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create directory: {}", parent.display()))?;

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_name = format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("leopardwm-shortcuts"),
        std::process::id(),
        nonce
    );
    let temp_path = parent.join(temp_name);

    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .with_context(|| format!("Failed to create temporary file: {}", temp_path.display()))?;
        file.write_all(contents)
            .with_context(|| format!("Failed to write temporary file: {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to flush temporary file: {}", temp_path.display()))?;
        replace_file(&temp_path, path)
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();

    unsafe {
        MoveFileExW(
            PCWSTR(source_wide.as_ptr()),
            PCWSTR(destination_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .with_context(|| {
        format!(
            "Failed to replace manifest {} with {}",
            destination.display(),
            source.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hotkey(action_id: &str, label: &str, group: &str, bindings: &[&str]) -> HotkeyBindingInfo {
        HotkeyBindingInfo {
            action_id: action_id.to_string(),
            label: label.to_string(),
            group: group.to_string(),
            bindings: bindings
                .iter()
                .map(|binding| (*binding).to_string())
                .collect(),
            enabled: !bindings.is_empty(),
        }
    }

    #[test]
    fn renders_deterministic_grouped_manifest_with_alternatives() {
        let hotkeys = [
            hotkey(
                "focus_left",
                "Focus \"left\"",
                "Focus",
                &["Ctrl+Alt+H", "Win+Left"],
            ),
            hotkey("focus_right", "Focus right", "Focus", &["Ctrl+Alt+L"]),
            hotkey("reload", "Reload", "Session", &[]),
        ];
        let rendered = render_manifest(&hotkeys);
        let expected = r#"PackageName: LeopardWM.LeopardWM
Name: LeopardWM
BackgroundProcess: true
WindowFilter: "leopardwm.exe"
Shortcuts:
  - SectionName: "Focus"
    Properties:
      - Name: "Focus \"left\""
        Shortcut:
          - Win: false
            Ctrl: true
            Alt: true
            Shift: false
            Keys:
              - 72
          - Win: true
            Ctrl: false
            Alt: false
            Shift: false
            Keys:
              - 37
      - Name: "Focus right"
        Shortcut:
          - Win: false
            Ctrl: true
            Alt: true
            Shift: false
            Keys:
              - 76
"#;

        assert!(rendered.warnings.is_empty());
        assert_eq!(rendered.yaml, expected);
        assert_eq!(render_manifest(&hotkeys), rendered);
    }

    #[test]
    fn skips_f_key_modifiers_but_keeps_f_key_triggers() {
        let rendered = render_manifest(&[hotkey(
            "focus_left",
            "Focus left",
            "Focus",
            &["F13+H", "Ctrl+F13"],
        )]);

        assert_eq!(rendered.warnings.len(), 1);
        assert!(rendered.warnings[0].contains("F13-F24 modifiers"));
        assert!(rendered.yaml.contains("Ctrl: true"));
        assert!(rendered.yaml.contains("- 124"));
        assert!(!rendered.yaml.contains("- 72"));
    }

    #[test]
    fn merges_non_contiguous_actions_into_one_section() {
        let rendered = render_manifest(&[
            hotkey("move_previous", "Move previous", "Move", &["Ctrl+PageUp"]),
            hotkey("switch_one", "Switch one", "Switch", &["Ctrl+1"]),
            hotkey("move_one", "Move one", "Move", &["Ctrl+Shift+1"]),
        ]);

        assert_eq!(rendered.yaml.matches("SectionName: \"Move\"").count(), 1);
        assert_eq!(rendered.yaml.matches("SectionName: \"Switch\"").count(), 1);
        assert!(
            rendered.yaml.find("Move previous").unwrap() < rendered.yaml.find("Move one").unwrap()
        );
    }

    #[test]
    fn install_path_matches_powertoys_user_manifest_location() {
        let base = Path::new(r"C:\Users\test\AppData\Local");
        assert_eq!(
            install_path_from_local_app_data(base),
            base.join("Microsoft")
                .join("WinGet")
                .join("KeyboardShortcuts")
                .join(MANIFEST_FILE_NAME)
        );
    }

    #[test]
    fn atomic_write_replaces_existing_manifest() {
        let directory = std::env::temp_dir().join(format!(
            "leopardwm-shortcut-guide-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create test directory");
        let path = directory.join(MANIFEST_FILE_NAME);

        write_atomic(&path, b"first").expect("initial write");
        write_atomic(&path, b"second").expect("replacement write");
        assert_eq!(fs::read(&path).expect("read manifest"), b"second");

        fs::remove_dir_all(directory).expect("remove test directory");
    }
}
