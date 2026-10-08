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
- `issues`: invalid chords, unknown action IDs, discarded physical-chord
  duplicates, and F-key trigger/modifier conflicts found in the loaded config.

Each hotkey record contains the canonical action ID, display label, display
group, resolved bindings (sorted for deterministic output), and `enabled`.
`enabled` means at least one binding survived configuration resolution: it parsed,
won physical-chord deduplication, and is not blocked by F-key modifier handling. An
intentionally disabled or otherwise unbound catalog action remains present with
an empty binding list. A valid non-catalog command uses a generated label and
the `Other` group so custom bindings never disappear from the query.

`focus_next`, `focus_prev`, `scroll_left`, and `scroll_right` are catalog actions
with no default keyboard binding. They appear in Settings hotkey and gesture
menus and in the hotkeys query even when unbound; Shortcut Guide exports them
only when an effective keyboard binding exists.

Entries are validated on both dimensions, so one config entry can report both
an unknown action and an invalid chord. F13-F24 terminal triggers that are also
used as modifiers elsewhere are reported as non-executable, matching the
keyboard hook's behavior.

Registration health is deliberately not part of the first contract. The query
does not prove the hook is installed or active: safe/no-hotkey startup, Windows
protected chords, and recorder suspension have different runtime semantics. A future runtime-status field
should therefore use a descriptive enum rather than a misleading per-binding
boolean.

## Shared resolution and collision precedence

`daemon::hotkey_resolution::resolve_hotkeys` is a pure resolver used by both
`setup_hotkeys` (startup/reload) and `handle_query_hotkeys`. It consumes the
already-loaded configuration, after normal config migration/default merging and
disabled-action handling; the CLI does not reread config files.

1. Sort entries lexicographically by loaded binding text, then loaded action
   identifier. This is Rust string ordering, not TOML file order.
2. Parse actions/chords using the existing command catalog and key parser.
   Action canonicalization reuses the config migration rename table.
3. Retain the first valid entry for each `Hotkey::stable_id`. Case, modifier
   order, aliases such as `Control`/`Ctrl` and `Meta`/`Win`, and token whitespace
   may produce the same physical chord. Neither invalid actions nor invalid
   chords claim an ID. Both same-action duplicates and different-action
   collisions produce an issue naming the ignored and retained entries.
4. Compute the F13-F24 modifier mask from the retained hook inputs. Mark
   conflicting F-key triggers non-executable for querying, but keep them in the
   hook input set: a blocked trigger can itself reserve another F-key modifier.
5. Return issues sorted by binding, loaded action identifier, then message.

`HotkeyIssue.action_id` preserves the identifier in the loaded config, before
query normalization. It is not a promise to recover pre-migration file text.
The effective action record uses the canonical identifier instead. Physical
stable IDs are unchanged, so queued hotkey events retain their physical meaning
across reloads. Conflicting configurations may select a different action than
an older build's unspecified HashMap iteration winner; remove the reported
collision to make intent explicit.

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
back into its first section. Each binding becomes a separate `Properties` item,
repeating the action name and containing exactly one chord in its `Shortcut`
array. PowerToys interprets multiple chords in one `Shortcut` array as sequential
steps, not alternative bindings. The golden manifest test covers two independent
bindings for the same action.

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
- Equivalent spellings, same-action duplicates, deterministic runtime/query
  winners across insertion orders, invalid candidates that must not claim IDs,
  original loaded action identifiers in diagnostics, and chained F-key masks.
- CLI query mapping and human-readable output.
- Golden PowerToys manifest output, YAML escaping, punctuation/navigation/F-key
  virtual keys, unsupported modifier warnings, and install-path selection.
- Focused crate tests followed by `cargo test --all`.

## Windows validation before release

Use the stable MSVC toolchain as required by the repository. From the checkout:

```powershell
cargo fmt --all -- --check
cargo test -p leopardwm-cli --bin lwm shortcut_guide::tests
cargo test -p leopardwm-daemon test_cmd_query_hotkeys
cargo test -p leopardwm-daemon hotkey_resolution::tests
cargo test --all
cargo clippy --all -- -D warnings
cargo build --release
pwsh ./.github/verify-gui-subsystems.ps1
```

A cross-target `cargo check --tests` validates compilation only; it does not
execute these tests or validate PowerToys import behavior.

Perform the following with a real Windows desktop and record the tested commit,
Windows version, PowerToys version, keyboard layout, and results. Preserve the
existing config and manifest before using test bindings, and restore them after.

1. Start the matching daemon and CLI build with hotkeys active. Enable Shortcut
   Guide in PowerToys. Assign `Ctrl+Alt+H` and `Win+Left` to `focus_left`, reload,
   and check `lwm query hotkeys` reports both bindings.
2. Run `lwm export-shortcut-guide --output guide.yml`, inspect the YAML, then run
   `lwm export-shortcut-guide --install`. Open Shortcut Guide using its configured
   activation shortcut. Confirm two independent "Focus left" entries, correctly
   displayed modifiers/keys, and no sequential shortcut. Dismiss the guide and
   verify each binding performs its action.
3. Add `Alt+Control+H = "focus_right"` alongside `Ctrl+Alt+H = "focus_left"` in
   the `[hotkeys]` table. Reload. The lexicographically earlier `Alt+Control+H`
   must win; the query must report the ignored `Ctrl+Alt+H` and its winner.
   Verify the key performs `focus_right`, and the exported guide omits the
   discarded left-action binding. Repeat after restart.
4. Exercise a disabled action, an unknown action, and an invalid chord. Check
   query diagnostics and confirm invalid/disabled shortcuts do not appear in the
   guide. With `F13+H` configured, verify the export warns and omits it. Test an
   ordinary `Ctrl+F14` trigger while F14 is not used as a modifier; it should
   remain exportable. Check letters, digits, navigation, and punctuation render
   correctly for the recorded keyboard layout.
5. Change a binding, reload, and re-run `--install` with the manifest already
   present. Reopen the guide and verify the new shortcut replaces the old one,
   with no duplicate application entry. Record whether the tested PowerToys
   version needs a restart to reload manifests. Confirm LeopardWM remains
   discoverable while another application is foreground and the daemon runs.
6. Restore the original config, reload, and restore/re-export the original
   manifest. Record any remaining failures before release.

Reference: [PowerToys user-manifest documentation](https://learn.microsoft.com/en-us/windows/powertoys/shortcut-guide)
and [the manifest schema](https://github.com/microsoft/PowerToys/blob/main/doc/specs/WinGet%20Manifest%20Keyboard%20Shortcuts%20schema.md).
The schema defines `Shortcut` as a sequence and numeric keys as virtual-key codes.

### PR #110 review validation status (2026-09-12)

- Formatting passed with Rust 1.98.1 / rustfmt 1.9.0.
- CLI and IPC production/test targets passed `cargo check --locked --tests`
  for `x86_64-pc-windows-msvc`.
- The real config and shared resolver modules, including their unit tests,
  passed an isolated MSVC-target compile check. That check excluded the rest
  of the daemon and does not validate its command-handler integration.
- The corrected golden YAML fixture parsed as three properties with one chord
  each; two properties retain the same action label for its alternatives.
- The PR author subsequently reviewed the prepared changes locally and reported
  that all runtime tests passed. This is author-reported Windows validation;
  the earlier Linux cross-target checks did not execute those tests.
- The Linux full-daemon compile check was blocked in the `ring` dependency because
  that host lacks Microsoft's `lib.exe`. Required CI build/test/clippy checks
  still need to pass for the published revision. The exact PowerToys version,
  import/reload results, and GUI subsystem results have not been separately
  recorded in this validation note. Compilation checks are not release approval.
