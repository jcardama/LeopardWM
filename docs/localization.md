# Localization

Settings, tray menus/tooltips, and daemon-owned toast and dialog text support all
bundled catalogs (currently `en` and `zh-CN`). Select
**Settings → Appearance → Language**, or set
`[appearance] language = "zh-CN"` in `config.toml` and reload. English is the default;
unsupported values become `en` with one config-validation warning. No OS-language
detection or `auto` setting is provided. The open Settings window and tray menu
change language without restarting the daemon.

**Simplified Chinese is a human-reviewed translation.** Its catalog header records
this status; completeness checks do not establish translation quality. For future
languages, label machine drafts honestly and review terminology, clarity,
punctuation, and layout before claiming human review.

## Files and lookup

- `crates/daemon/locales/en.toml`: English source of truth.
- `crates/daemon/locales/zh-CN.toml`: Human-reviewed Simplified Chinese translation.
- `crates/daemon/build.rs`: discovers and embeds every `locales/*.toml` file.
- `crates/daemon/src/locale.rs`: parsing, bundled identifiers/autonyms, English
  fallback, and named-placeholder substitution.
- `crates/daemon/src/settings/html.rs`: page consumers; the host injects the resolved
  table. Static text uses `data-i18n` and attribute-specific key bindings; generated
  HTML escapes translations with `escHtml`/`escAttr`.

Each locale is one **flat TOML table** of strings. Quote dotted keys so TOML does not
interpret them as nested tables:

```toml
"settings.language.label" = "Language"
"notification.tiling_paused.body" = "Window placement did not finish within {seconds} seconds, so tiling was automatically paused. Choose Resume Tiling from the tray after the windows settle."
```

Keys are stable dotted identifiers grouped by surface (`settings`, `tray`,
`notification`, `dialog`, `hotkeys`). Use semantic names such as
`settings.layout.gap.description`; retain existing keys when changing wording.
Settings hotkey command names and gesture-command choices use
`hotkeys.command.<id>` keys with English catalog labels as fallback. Translate these
keys when adding a language. In `en.toml`, their values must match
`ipc::hotkeys::hotkey_catalog()` labels; the
`english_hotkey_names_match_the_action_catalog` test enforces this and rejects
orphan keys. Do not put other English fallbacks into Rust or JavaScript: add the
English entry here.
Values are plain text, not HTML. Formatting markup belongs to the page, never to a
translation. Brand names, author names, URLs, license identifiers, numeric examples,
and command/key tokens retain their literal identity. The language selector uses
each catalog's `"language.name"` autonym, such as
`English` and `简体中文`; this name does not change with the selected language.

Named placeholders use `{name}` (ASCII letters/underscores followed by letters,
underscores, or digits). Preserve each key's placeholder names exactly. Substitution
is single-pass: inserted values are never reinterpreted as placeholders. Unknown
placeholders remain visible, making missing arguments diagnosable. Do not introduce
HTML markup or translate placeholder names. Plural-sensitive sentences use separate
keys where needed; this is not a general pluralization engine.

A missing translated key falls back to English at runtime. A missing English key is
a product bug, logged and displayed as its key rather than causing a panic. Invalid
TOML, duplicate keys, nested tables, and non-string values return loader errors. A
broken bundled non-English catalog falls back to English with a warning. Bundled
catalog tests still reject missing translations and orphan keys, so runtime fallback
is not a substitute for completing a translation.

The build script generates an `include_str!` table for every catalog: **editing a
locale file requires rebuilding the daemon**. Locale files are not loaded from the installed filesystem; installer/WiX
changes are unnecessary.

## Adding or editing a language

1. Edit an existing UTF-8 catalog, or copy `crates/daemon/locales/en.toml` to
   `crates/daemon/locales/<identifier>.toml` and translate every value. **Only that
   catalog file is needed to add a language**; no Rust, HTML, tests, or config-template
   registration is needed. The build discovers every `.toml` file in this directory.
2. Use the filename stem as the exact, case-sensitive identifier, for example `fr`,
   `pt-BR`, or `zh-CN`. Identifiers contain 2–8 ASCII letters followed by optional
   hyphen-separated groups of 1–8 ASCII letters or digits. Do not rename `en.toml`.
   Set `"language.name"` to the language's own display name. Preserve all quoted
   keys and `{placeholders}`; translate plain-text values only.
3. Run from the repository root:

   ```powershell
   cargo test -p leopardwm-daemon locale::tests
   cargo fmt --all -- --check
   git diff --check
   pwsh -NoProfile -File tools/check.ps1
   ```

   Catalog tests automatically check every file: valid flat string TOML, valid
   identifier, nonempty display name, every English key, no orphan keys, and
   placeholder-name/count parity. Failures identify the file and affected key.
   Config tests cover every bundled identifier, default/legacy configs, round-trip,
   and normalization with one warning. These standard checks do not require Node.js.
4. Optionally, with Node.js on `PATH`, run exactly this extra Settings check:

   ```powershell
   cargo test -p leopardwm-daemon translations_render_literally_in_settings_and_switch_live -- --ignored
   ```

   It executes the embedded page script without a browser or WebView and checks
   literal text/attribute insertion, escaped generated markup and language options,
   generated locale keys, and relabeling while a preset save is pending. Do not run
   all ignored tests: other ignored tests require explicit desktop-test authorization.
5. Include the language identifier, test results, and review status in your PR.
   Native-speaker review is welcome; label machine drafts honestly. Rebuild the
   daemon to try changes. Arrange separately authorized native UI acceptance for
   long labels, text scaling, high contrast, and non-ASCII WebView/tray rendering.
   Unit tests do not prove native visual or physical-input behavior.

## Deliberately untranslated surfaces

These remain English in this change:

- CLI output and help.
- Logs, including config-validation and locale-loader warnings.
- IPC/config keys and values, command identifiers, and key-chord tokens.
- Hotkey command labels returned over IPC and used by CLI output and the PowerToys
  Shortcut Guide export: they come from `ipc::hotkeys::hotkey_catalog()`, not the
  Settings locale.
- The watchdog process toast.
- The installer.

Operating-system-owned dialog buttons use Windows' language, not this setting.
Error details originating in IPC/Win32 may remain English inside an otherwise
localized daemon dialog; the daemon-owned explanation is localized.
