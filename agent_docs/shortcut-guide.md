# Hotkey Query and PowerToys Shortcut Guide Export

## Goal

Expose LeopardWM's effective, migrated hotkey configuration over the existing
named-pipe IPC protocol and render that same data as a PowerToys Shortcut Guide
user manifest.

The daemon remains the source of truth. The exporter intentionally requires a
running daemon so it does not duplicate config migration, validation, default
merging, or disabled-action semantics in the CLI.

## IPC contract

`IpcCommand::QueryHotkeys` returns `IpcResponse::HotkeyList` containing:

- `hotkeys`: every catalog action in catalog order, followed by any valid
  non-catalog config commands in stable action-ID order;
- `scroll_modifier`: the configured mouse-scroll modifier;
- `issues`: invalid chords and unknown action IDs found in the loaded config.

Each hotkey record contains the action ID, display label, display group, all
valid configured bindings (sorted for deterministic output), and `enabled`.
`enabled` means that the action has at least one valid executable binding. An
intentionally disabled or otherwise unbound catalog action remains present with
an empty binding list. A valid non-catalog command uses a generated label and
the `Other` group so custom bindings never disappear from the query.

Entries are validated on both dimensions, so one config entry can report both
an unknown action and an invalid chord. F13-F24 terminal triggers that are also
used as modifiers elsewhere are reported as non-executable, matching the
keyboard hook's behavior.

Registration health is deliberately not part of the first contract. LeopardWM
uses one low-level keyboard hook, while known protected chords and recorder
suspension have different failure semantics. A future runtime-status field
should therefore use a descriptive enum rather than a misleading per-binding
boolean.

## Export command

The CLI adds:

```text
lwm export-shortcut-guide
lwm export-shortcut-guide --output PATH
lwm export-shortcut-guide --install
```

With no destination option, YAML is written to stdout and diagnostics go only
to stderr. `--output` and `--install` are mutually exclusive. Installation
uses the stable filename `LeopardWM.LeopardWM.en-US.yml` under:

```text
%LOCALAPPDATA%\Microsoft\WinGet\KeyboardShortcuts
```

The manifest uses `BackgroundProcess: true` and matches `leopardwm.exe`,
which makes LeopardWM available while the daemon is running. Actions stay in
first-seen group and action order; a repeated non-contiguous group is merged
back into its first section. Multiple bindings become alternative `Shortcut`
entries.

## Conversion boundary

The exporter reuses
`leopardwm-platform-win32::parse_hotkey_string` rather than maintaining a
second LeopardWM key parser. Standard Win/Ctrl/Alt/Shift modifiers map to
PowerToys modifier fields; the trigger is emitted as its Win32 virtual-key
number.

PowerToys does not expose fields for LeopardWM's F13-F24-as-modifier extension.
Those chords are skipped with a warning until compatibility is verified.
F13-F24 remain valid when used as the terminal trigger key.

YAML is rendered deterministically using double-quoted, JSON-escaped scalar
values. This avoids a new serialization dependency while remaining valid YAML.
Installation writes a sibling temporary file and replaces the destination.

## Change boundaries

- `leopardwm-ipc`: public query/response/data types.
- `leopardwm-daemon`: derive effective records and diagnostics from the
  loaded config plus the central hotkey catalog.
- `leopardwm-cli`: query mapping/output and PowerToys manifest
  rendering/install.

## Verification

- IPC command/response serialization round trips.
- Catalog order, disabled/unbound actions, multiple bindings, invalid chords,
  unknown actions, valid non-catalog commands, and F-key conflicts.
- CLI query mapping and human-readable output.
- Golden PowerToys manifest output, YAML escaping, punctuation/navigation/F-key
  virtual keys, unsupported modifier warnings, and install-path selection.
- Focused crate tests followed by `cargo test --all`.
