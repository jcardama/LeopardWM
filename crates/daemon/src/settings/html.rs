//! Embedded HTML/CSS/JS for the WebView2-based settings panel.
//!
//! WinUI 3 / Fluent Design System v2 single-page app with NavigationView
//! sidebar, seven settings sections, and IPC back to the Rust host.
//! Color tokens, typography, spacing, and component styling match the
//! official WinUI 3 theme resources.

pub const SETTINGS_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>LeopardWM Settings</title>
<style>
/* ── Reset ────────────────────────────────────────────────────────── */
*, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }

/* ── WinUI 3 Design Tokens ────────────────────────────────────────── */
:root {
  --font: 'Segoe UI Variable', 'Segoe UI', system-ui, sans-serif;
  --nav-w: 200px;
  --ctrl-radius: 4px;
  --overlay-radius: 8px;

  /* Light Theme */
  --bg: #f3f3f3;
  --bg-secondary: #eeeeee;
  --bg-tertiary: #f9f9f9;
  --bg-solid-quaternary: #ffffff;

  --card-bg: rgba(255,255,255,0.70);
  --card-bg-secondary: rgba(246,246,246,0.50);
  --card-stroke: rgba(0,0,0,0.06);

  --text-primary: rgba(0,0,0,0.89);
  --text-secondary: rgba(0,0,0,0.61);
  --text-tertiary: rgba(0,0,0,0.45);

  --ctrl-fill: rgba(255,255,255,0.70);
  --ctrl-fill-secondary: rgba(249,249,249,0.50);
  --ctrl-fill-input-active: #ffffff;
  --ctrl-stroke: rgba(0,0,0,0.06);
  --ctrl-stroke-secondary: rgba(0,0,0,0.16);
  --ctrl-strong-fill: rgba(0,0,0,0.45);
  --ctrl-strong-fill-disabled: rgba(0,0,0,0.32);

  --subtle-secondary: rgba(0,0,0,0.04);
  --subtle-tertiary: rgba(0,0,0,0.02);
  --divider-stroke: rgba(0,0,0,0.06);

  --accent: #005a9e;
  --accent-secondary: rgba(0,90,158,0.90);
  --accent-tertiary: rgba(0,90,158,0.80);
  --accent-text: #ffffff;
  --danger: #c42b1c;

  --info-bg: rgba(0, 120, 212, 0.06);
  --info-stroke: #0078d4;
  --success-bg: rgba(16, 124, 16, 0.06);
  --success-stroke: #107c10;
  --warning-bg: rgba(255, 152, 0, 0.06);
  --warning-stroke: #ff9800;
  --error-bg: rgba(200, 43, 28, 0.06);
  --error-stroke: var(--danger);

  --toggle-off-stroke: rgba(0,0,0,0.45);
  --toggle-off-thumb: rgba(0,0,0,0.61);
  --toggle-on-thumb: #ffffff;

  --scrollbar-thumb: rgba(0,0,0,0.37);
  --shadow: 0 2px 8px rgba(0,0,0,0.12), 0 0 1px rgba(0,0,0,0.08);
}

@media (prefers-color-scheme: dark) {
  :root {
    --bg: #202020;
    --bg-secondary: #1c1c1c;
    --bg-tertiary: #282828;
    --bg-solid-quaternary: #2c2c2c;

    --card-bg: rgba(255,255,255,0.05);
    --card-bg-secondary: rgba(255,255,255,0.03);
    --card-stroke: rgba(0,0,0,0.10);

    --text-primary: #ffffff;
    --text-secondary: rgba(255,255,255,0.77);
    --text-tertiary: rgba(255,255,255,0.53);

    --ctrl-fill: rgba(255,255,255,0.06);
    --ctrl-fill-secondary: rgba(255,255,255,0.08);
    --ctrl-fill-input-active: rgba(30,30,30,0.70);
    --ctrl-stroke: rgba(255,255,255,0.07);
    --ctrl-stroke-secondary: rgba(255,255,255,0.09);
    --ctrl-strong-fill: rgba(255,255,255,0.54);
    --ctrl-strong-fill-disabled: rgba(255,255,255,0.25);

    --subtle-secondary: rgba(255,255,255,0.06);
    --subtle-tertiary: rgba(255,255,255,0.04);
    --divider-stroke: rgba(0,0,0,0.30);

    --accent: #76b9ed;
    --accent-secondary: rgba(118,185,237,0.90);
    --accent-tertiary: rgba(118,185,237,0.80);
    --accent-text: #003046;
    --danger: #ff99a4;

    --info-bg: rgba(118, 185, 237, 0.10);
    --success-bg: rgba(109, 202, 70, 0.10);
    --warning-bg: rgba(255, 185, 0, 0.10);
    --error-bg: rgba(255, 153, 164, 0.08);

    --toggle-off-stroke: rgba(255,255,255,0.54);
    --toggle-off-thumb: rgba(255,255,255,0.77);
    --toggle-on-thumb: #1a1a1a;

    --scrollbar-thumb: rgba(255,255,255,0.37);
    --shadow: 0 2px 8px rgba(0,0,0,0.36), 0 0 1px rgba(0,0,0,0.24);
  }
}

/* ── High Contrast / Forced Colors ─────────────────────────────────── */
@media (forced-colors: active) {
  html, body {
    background: Canvas !important;
    color: CanvasText !important;
  }
  .sidebar {
    background: Canvas !important;
    border-color: CanvasText !important;
  }
  .nav-item {
    color: CanvasText !important;
    background: transparent !important;
    forced-color-adjust: none;
  }
  .nav-item:hover, .nav-item.active {
    background: Highlight !important;
    color: HighlightText !important;
  }
  .nav-item.active::before {
    background: HighlightText !important;
  }
  .nav-item .nav-icon {
    stroke: currentColor !important;
  }
  .section-title {
    color: CanvasText !important;
  }
  .card {
    background: Canvas !important;
    border-color: CanvasText !important;
  }
  .field + .field {
    border-color: GrayText !important;
  }
  .field-label, .field-desc {
    color: CanvasText !important;
  }
  input, select, button, .combobox-trigger {
    background: ButtonFace !important;
    color: ButtonText !important;
    border-color: ButtonBorder !important;
  }
  .info-bar {
    background: Canvas !important;
    border-color: Highlight !important;
  }
  .info-bar-icon { fill: CanvasText !important; }
  .info-bar-title, .info-bar-message { color: CanvasText !important; }
  .combobox-popup {
    background: Canvas !important;
    border-color: CanvasText !important;
  }
  .combobox-option {
    color: CanvasText !important;
    background: transparent !important;
  }
  .combobox-option:hover, .combobox-option.selected {
    background: Highlight !important;
    color: HighlightText !important;
  }
}

/* ── Typography ───────────────────────────────────────────────────── */
html, body {
  height: 100%; width: 100%;
  font-family: var(--font);
  font-size: 14px;
  line-height: 20px;
  color: var(--text-primary);
  background: transparent;
  overflow: hidden;
  -webkit-user-select: none;
  user-select: none;
}

/* ── App Shell ────────────────────────────────────────────────────── */
.app { display: flex; height: 100%; }

/* ── NavigationView ───────────────────────────────────────────────── */
.nav {
  width: var(--nav-w);
  min-width: var(--nav-w);
  padding: 12px 4px;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.nav-brand {
  font-size: 14px;
  font-weight: 600;
  line-height: 20px;
  padding: 8px 16px 20px;
}

.nav-item {
  display: flex;
  align-items: center;
  gap: 12px;
  height: 36px;
  padding: 0 12px;
  margin: 0 4px;
  border-radius: var(--ctrl-radius);
  color: var(--text-primary);
  text-decoration: none;
  font-size: 14px;
  line-height: 20px;
  cursor: pointer;
  background: transparent;
  border: none;
  width: calc(100% - 8px);
  transition: background 0.08s;
  position: relative;
}
.nav-item:hover { background: var(--subtle-secondary); }
.nav-item:active { background: var(--subtle-tertiary); }
.nav-item.active {
  background: var(--subtle-secondary);
  font-weight: 600;
}
.nav-item.active::before {
  content: '';
  position: absolute;
  left: 0;
  top: 50%;
  transform: translateY(-50%);
  width: 3px;
  height: 16px;
  background: var(--accent);
  border-radius: 2px;
}

.nav-spacer { flex: 1; }

.nav-icon {
  width: 16px;
  height: 16px;
  flex-shrink: 0;
  stroke: currentColor;
  fill: none;
  stroke-width: 1.2;
  stroke-linecap: round;
  stroke-linejoin: round;
}

/* ── Main ─────────────────────────────────────────────────────────── */
.main { flex: 1; display: flex; flex-direction: column; min-width: 0; }

.content {
  flex: 1;
  overflow-y: auto;
  scrollbar-gutter: stable;
  padding: 12px 2px 12px 12px;
}

/* Scrollbar — thin, transparent track */
.content::-webkit-scrollbar { width: 10px; }
.content::-webkit-scrollbar-track { background: transparent; }
.content::-webkit-scrollbar-thumb {
  background: var(--scrollbar-thumb);
  border-radius: 10px;
  border: 3px solid transparent;
  background-clip: content-box;
  min-height: 30px;
}
.content::-webkit-scrollbar-thumb:hover { border-width: 2px; }


/* ── Sections ─────────────────────────────────────────────────────── */
.section { display: none; }
.section.active { display: block; }
.section-title {
  font-size: 20px;
  font-weight: 600;
  line-height: 28px;
  margin-bottom: 16px;
}
.section-subtitle {
  font-size: 14px;
  font-weight: 600;
  line-height: 20px;
  margin: 16px 0 4px;
}
.section-desc {
  font-size: 12px;
  line-height: 16px;
  color: var(--text-secondary);
  margin-bottom: 8px;
}

/* ── Card ─────────────────────────────────────────────────────────── */
.card {
  background: var(--card-bg);
  border: 1px solid var(--card-stroke);
  border-radius: var(--overlay-radius);
  padding: 2px 16px;
  margin-bottom: 12px;
}

/* ── Settings Row ─────────────────────────────────────────────────── */
.field {
  display: flex;
  align-items: center;
  justify-content: space-between;
  min-height: 48px;
  padding: 10px 0;
  margin: 0 -16px;
  padding-left: 16px;
  padding-right: 16px;
}
.field + .field { border-top: 1px solid var(--divider-stroke); }
.field-info { flex: 1; min-width: 0; padding-right: 16px; }
.field-label { font-size: 14px; line-height: 20px; }
.field-desc { font-size: 12px; line-height: 16px; color: var(--text-secondary); margin-top: 2px; }

/* ── Input Wrapper (for ::after accent line) ─────────────────────── */
.input-wrap {
  position: relative;
  display: inline-block;
  border-radius: var(--ctrl-radius);
  overflow: hidden;
}
.input-wrap::after {
  content: '';
  position: absolute;
  left: 0; right: 0; bottom: 0;
  height: 2px;
  background: var(--accent);
  transform: scaleX(0);
  transition: transform 0.15s cubic-bezier(0.85, 0, 0.15, 1);
}
.input-wrap:focus-within::after { transform: scaleX(1); }

/* ── Text / Number Inputs ─────────────────────────────────────────── */
input[type="text"],
input[type="number"] {
  font-family: var(--font);
  font-size: 14px;
  line-height: 20px;
  color: var(--text-primary);
  background: var(--ctrl-fill);
  border: 1px solid var(--ctrl-stroke);
  border-bottom: 1px solid var(--ctrl-stroke-secondary);
  border-radius: var(--ctrl-radius);
  padding: 5px 11px 6px;
  min-height: 32px;
  outline: none;
  transition: background 0.08s, border-color 0.08s;
}
input[type="text"]:hover,
input[type="number"]:hover {
  background: var(--ctrl-fill-secondary);
}
input[type="text"]:focus,
input[type="number"]:focus {
  background: var(--ctrl-fill-input-active);
  border-color: var(--ctrl-stroke);
  border-bottom-color: transparent;
}
input[type="number"] { width: 100px; }
input[type="text"] { width: 180px; }

/* Hide native number spinners */
input[type="number"]::-webkit-inner-spin-button,
input[type="number"]::-webkit-outer-spin-button {
  -webkit-appearance: none;
  margin: 0;
}

/* ── Color Picker ─────────────────────────────────────────────────── */
input[type="color"] {
  width: 40px; height: 32px;
  border: 1px solid var(--ctrl-stroke);
  border-radius: var(--ctrl-radius);
  padding: 3px;
  cursor: pointer;
  background: var(--ctrl-fill);
  -webkit-appearance: none;
}
input[type="color"]::-webkit-color-swatch-wrapper { padding: 0; }
input[type="color"]::-webkit-color-swatch { border: none; border-radius: 2px; }

/* ── Custom ComboBox ──────────────────────────────────────────────── */
.combobox {
  position: relative;
  display: inline-block;
  min-width: 140px;
}
.combobox-trigger {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  width: 100%;
  font-family: var(--font);
  font-size: 14px;
  line-height: 20px;
  color: var(--text-primary);
  background: var(--ctrl-fill);
  border: 1px solid var(--ctrl-stroke);
  border-bottom: 1px solid var(--ctrl-stroke-secondary);
  border-radius: var(--ctrl-radius);
  padding: 5px 8px 6px 11px;
  min-height: 32px;
  cursor: pointer;
  text-align: left;
  transition: background 0.08s;
}
.combobox-trigger:hover { background: var(--ctrl-fill-secondary); }
.combobox-text { flex: 1; }
.combobox-chevron {
  width: 12px; height: 12px;
  flex-shrink: 0;
  fill: var(--text-secondary);
  transition: transform 0.12s ease;
}
.combobox.open .combobox-chevron { transform: rotate(180deg); }
.combobox-popup {
  display: none;
  position: fixed;
  background: var(--bg-solid-quaternary);
  border: 1px solid var(--card-stroke);
  border-radius: var(--overlay-radius);
  padding: 4px;
  z-index: 10000;
  box-shadow: var(--shadow);
  overflow-y: auto;
}
.combobox-popup::-webkit-scrollbar { width: 3px; }
.combobox-popup::-webkit-scrollbar-track { background: transparent; }
.combobox-popup::-webkit-scrollbar-thumb { background: var(--text-tertiary); border-radius: 3px; }
.combobox-popup::-webkit-scrollbar-thumb:hover { background: var(--text-secondary); }
.combobox.open .combobox-popup { display: block; }
.combobox-option {
  padding: 6px 12px;
  border-radius: var(--ctrl-radius);
  cursor: pointer;
  font-size: 14px;
  line-height: 20px;
  color: var(--text-primary);
  transition: background 0.06s;
  white-space: nowrap;
}
.combobox-option:hover { background: var(--subtle-secondary); }
.combobox-option.selected {
  background: var(--subtle-secondary);
  font-weight: 600;
  position: relative;
  padding-left: 28px;
}
.combobox-option.selected::before {
  content: '';
  position: absolute;
  left: 12px;
  top: 50%;
  transform: translateY(-50%);
  width: 4px;
  height: 4px;
  border-radius: 2px;
  background: var(--accent);
}

/* ── Toggle Switch (40x20, WinUI 3) ──────────────────────────────── */
.toggle {
  position: relative;
  width: 40px;
  height: 20px;
  cursor: pointer;
  flex-shrink: 0;
}
.toggle input { display: none; }
.toggle .track {
  position: absolute;
  inset: 0;
  border-radius: 10px;
  background: transparent;
  border: 1px solid var(--toggle-off-stroke);
  transition: background 0.12s, border-color 0.12s;
}
.toggle .thumb {
  position: absolute;
  width: 12px;
  height: 12px;
  left: 4px;
  top: 4px;
  background: var(--toggle-off-thumb);
  border-radius: 6px;
  transition: transform 0.15s cubic-bezier(0.85, 0, 0.15, 1),
              width 0.08s, height 0.08s, left 0.08s, top 0.08s,
              background 0.12s;
  pointer-events: none;
}
.toggle:hover .thumb {
  width: 14px; height: 14px;
  left: 3px; top: 3px;
}
.toggle input:checked ~ .track {
  background: var(--accent);
  border-color: var(--accent);
}
.toggle input:checked ~ .thumb {
  background: var(--toggle-on-thumb);
  transform: translateX(20px);
}
.toggle:hover input:checked ~ .thumb {
  width: 14px; height: 14px;
  left: 3px; top: 3px;
}

/* ── Buttons ──────────────────────────────────────────────────────── */
.btn {
  font-family: var(--font);
  font-size: 14px;
  line-height: 20px;
  padding: 5px 11px 6px;
  min-height: 32px;
  border-radius: var(--ctrl-radius);
  border: 1px solid var(--ctrl-stroke);
  border-bottom: 1px solid var(--ctrl-stroke-secondary);
  background: var(--ctrl-fill);
  color: var(--text-primary);
  cursor: pointer;
  transition: background 0.08s;
}
.btn:hover { background: var(--ctrl-fill-secondary); }
.btn:active {
  background: var(--ctrl-fill-secondary);
  color: var(--text-secondary);
  border-bottom-color: var(--ctrl-stroke);
}
.btn-accent {
  background: var(--accent);
  color: var(--accent-text);
  border: 1px solid transparent;
  border-bottom: 1px solid var(--accent-tertiary);
}
.btn-accent:hover { background: var(--accent-secondary); }
.btn-accent:active { background: var(--accent-tertiary); border-bottom-color: transparent; }
.btn-sm { font-size: 12px; line-height: 16px; padding: 4px 12px; min-height: 24px; }

/* ── Editable Tables ──────────────────────────────────────────────── */
.table-wrap {
  background: var(--card-bg);
  border: 1px solid var(--card-stroke);
  border-radius: var(--overlay-radius);
  overflow: hidden;
}
table { width: 100%; border-collapse: collapse; font-size: 14px; line-height: 20px; }
th {
  background: var(--card-bg-secondary);
  padding: 8px 8px;
  text-align: left;
  font-weight: 600;
  font-size: 12px;
  line-height: 16px;
  color: var(--text-secondary);
  border-bottom: 1px solid var(--divider-stroke);
}
th:first-child { padding-left: 12px; }
th:last-child { padding-right: 12px; }
td {
  padding: 4px 0;
  border-bottom: 1px solid var(--divider-stroke);
  vertical-align: middle;
}
td:first-child { padding-left: 4px; }
.hk-cmd-label { padding-left: 12px !important; color: var(--text-primary); }
td:last-child { padding-right: 4px; }
tr:last-child td { border-bottom: none; }
td input[type="text"] {
  width: 100%; border: none; background: transparent;
  padding: 4px 8px; min-height: unset;
  border-radius: var(--ctrl-radius);
  border-bottom: 1px solid transparent;
  transition: background 0.08s, border-color 0.08s;
}
td input[type="text"]:hover { background: var(--ctrl-fill-secondary); }
td input[type="text"]:focus {
  background: var(--ctrl-fill-input-active);
  border-bottom-color: transparent;
}
td .input-wrap { display: block; }
td .input-wrap::after { height: 2px; }
/* ── Inline ComboBox (for table cells) ────────────────────────────── */
td .combobox { min-width: 80px; }
td .combobox-trigger {
  border: none;
  background: transparent;
  padding: 4px 8px;
  min-height: unset;
  border-radius: var(--ctrl-radius);
  transition: background 0.08s;
}
td .combobox-trigger:hover { background: var(--ctrl-fill-secondary); }
.table-actions { display: flex; gap: 8px; margin-top: 12px; margin-bottom: 4px; }

.row-delete {
  color: var(--text-tertiary);
  cursor: pointer;
  padding: 0;
  border: none;
  background: none;
  border-radius: var(--ctrl-radius);
  display: flex;
  align-items: center;
  justify-content: center;
  width: 28px; height: 28px;
  transition: background 0.08s, color 0.08s;
}
.row-delete:hover { color: var(--danger); background: var(--subtle-secondary); }
.row-delete svg { width: 12px; height: 12px; stroke: currentColor; fill: none; stroke-width: 1.5; stroke-linecap: round; }

/* ── Slider ───────────────────────────────────────────────────────── */
.slider-group { display: flex; align-items: center; gap: 12px; }
input[type="range"] {
  -webkit-appearance: none;
  width: 140px; height: 4px;
  background: var(--ctrl-strong-fill);
  border-radius: 2px;
  outline: none;
}
input[type="range"]::-webkit-slider-thumb {
  -webkit-appearance: none;
  width: 20px; height: 20px;
  border-radius: 10px;
  background: var(--accent);
  cursor: pointer;
  border: 4px solid var(--bg-solid-quaternary);
  box-shadow: 0 0 0 1px var(--ctrl-stroke-secondary);
}
.slider-val {
  font-size: 12px; line-height: 16px;
  color: var(--text-secondary);
  min-width: 32px; text-align: right;
}

/* ── About ───────────────────────────────────────────────────────── */
.about-card { display: flex; flex-direction: column; gap: 16px; padding: 20px 16px; }
.about-header { display: flex; align-items: center; gap: 16px; }
.about-icon { width: 48px; height: 48px; border-radius: 8px; }
.about-name { font-size: 20px; font-weight: 600; line-height: 28px; }
.about-version { font-size: 13px; color: var(--text-secondary); line-height: 18px; }
.about-desc { font-size: 13px; color: var(--text-secondary); line-height: 20px; }
.about-divider { height: 1px; background: var(--divider-stroke); }
.about-meta { display: flex; flex-direction: column; gap: 8px; }
.about-row { display: flex; justify-content: space-between; align-items: center; font-size: 13px; }
.about-label { color: var(--text-secondary); }
.about-value { color: var(--text-primary); }
.about-link {
  color: var(--accent);
  text-decoration: none;
  cursor: pointer;
}
.about-link:hover { text-decoration: underline; }
.about-coffee {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  padding: 8px 16px;
  border-radius: var(--ctrl-radius);
  background: #ffdd00;
  color: #000000;
  font-size: 13px;
  font-weight: 600;
  text-decoration: none;
  cursor: pointer;
  align-self: flex-start;
  transition: opacity 0.1s;
}
.about-coffee:hover { opacity: 0.85; }
.about-legal-title { font-size: 13px; font-weight: 600; margin-bottom: 4px; }
.about-legal-text {
  font-size: 12px; line-height: 18px;
  color: var(--text-secondary);
  -webkit-user-select: text; user-select: text;
}
.about-legal-text code {
  font-family: 'Cascadia Code', 'Consolas', monospace;
  font-size: 11px;
  background: var(--subtle-secondary);
  padding: 1px 4px;
  border-radius: 3px;
}

/* ── InfoBar (WinUI 3 Fluent) ──────────────────────────────────────── */
.info-bar {
  display: flex;
  align-items: flex-start;
  gap: 12px;
  padding: 12px 16px;
  border-radius: var(--ctrl-radius);
  border: 1px solid var(--divider-stroke);
  border-left: 3px solid var(--info-stroke);
  background: var(--info-bg);
  margin: 8px 0;
}
.info-bar-icon { flex-shrink: 0; width: 16px; height: 16px; margin-top: 2px; fill: var(--info-stroke); }
.info-bar-content { flex: 1; min-width: 0; }
.info-bar-title { font-size: 14px; line-height: 20px; font-weight: 600; color: var(--text-primary); }
.info-bar-message { font-size: 12px; line-height: 16px; color: var(--text-secondary); margin-top: 2px; }
.info-bar[hidden] { display: none; }
.info-bar.success { border-left-color: var(--success-stroke); background: var(--success-bg); }
.info-bar.success .info-bar-icon { fill: var(--success-stroke); }
.info-bar.warning { border-left-color: var(--warning-stroke); background: var(--warning-bg); }
.info-bar.warning .info-bar-icon { fill: var(--warning-stroke); }
.info-bar.error  { border-left-color: var(--error-stroke); background: var(--error-bg); }
.info-bar.error .info-bar-icon { fill: var(--error-stroke); }

</style>
</head>
<body>
<div class="app">
  <nav class="nav">
    <div class="nav-brand">Settings</div>
    <a href="#" data-section="layout" class="nav-item active">
      <svg class="nav-icon" viewBox="0 0 16 16"><rect x="1.5" y="2.5" width="5" height="11" rx="1"/><rect x="9.5" y="2.5" width="5" height="11" rx="1"/></svg>
      Layout
    </a>
    <a href="#" data-section="appearance" class="nav-item">
      <svg class="nav-icon" viewBox="0 0 16 16"><circle cx="8" cy="8" r="5.5"/><path d="M8 2.5v11" stroke-dasharray="1.5 2"/></svg>
      Appearance
    </a>
    <a href="#" data-section="behavior" class="nav-item">
      <svg class="nav-icon" viewBox="0 0 16 16"><circle cx="8" cy="8" r="2.5"/><path d="M8 1.5v2m0 9v2m-6.5-6.5h2m9 0h2M3.17 3.17l1.42 1.42m6.82 6.82l1.42 1.42M3.17 12.83l1.42-1.42m6.82-6.82l1.42-1.42"/></svg>
      Behavior
    </a>
    <a href="#" data-section="hotkeys" class="nav-item">
      <svg class="nav-icon" viewBox="0 0 16 16"><rect x="1" y="3.5" width="14" height="9" rx="1.5"/><rect x="3.5" y="5.5" width="2" height="1.5" rx="0.3"/><rect x="7" y="5.5" width="2" height="1.5" rx="0.3"/><rect x="10.5" y="5.5" width="2" height="1.5" rx="0.3"/><rect x="5" y="9" width="6" height="1.5" rx="0.3"/></svg>
      Hotkeys
    </a>
    <a href="#" data-section="rules" class="nav-item">
      <svg class="nav-icon" viewBox="0 0 16 16"><rect x="3" y="1.5" width="10" height="13" rx="1.5"/><line x1="5.5" y1="5" x2="10.5" y2="5"/><line x1="5.5" y1="8" x2="10.5" y2="8"/><line x1="5.5" y1="11" x2="8.5" y2="11"/></svg>
      Rules
    </a>
    <a href="#" data-section="gestures" class="nav-item">
      <svg class="nav-icon" viewBox="0 0 16 16"><path d="M8 2v7"/><path d="M5.5 6.5L8 9l2.5-2.5"/><path d="M4 12.5c0-1 1-2 4-2s4 1 4 2" /></svg>
      Gestures
    </a>
    <a href="#" data-section="snaphints" class="nav-item">
      <svg class="nav-icon" viewBox="0 0 16 16" stroke-width="1.5"><path d="M2 5.5V3.5a1.5 1.5 0 011.5-1.5H5.5"/><path d="M10.5 2H12.5A1.5 1.5 0 0114 3.5V5.5"/><path d="M14 10.5v2a1.5 1.5 0 01-1.5 1.5H10.5"/><path d="M5.5 14H3.5A1.5 1.5 0 012 12.5V10.5"/></svg>
      Snap Hints
    </a>
    <div class="nav-spacer"></div>
    <a href="#" data-section="about" class="nav-item">
      <svg class="nav-icon" viewBox="0 0 16 16"><circle cx="8" cy="8" r="6.5" fill="none" stroke-width="1.2"/><text x="8" y="11.5" text-anchor="middle" font-size="9" font-weight="600" fill="currentColor" stroke="none">i</text></svg>
      About
    </a>
  </nav>

  <div class="main">
    <div class="content">
      <!-- Layout -->
      <div id="sec-layout" class="section active">
        <h2 class="section-title">Layout</h2>
        <div class="card">
          <div class="field">
            <div class="field-info"><div class="field-label">Gap</div><div class="field-desc">Space between columns and between stacked windows (px)</div></div>
            <input type="number" id="layout-gap" min="0" max="100">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Outer gap left</div><div class="field-desc">Space at the left edge (px)</div></div>
            <input type="number" id="layout-outer_gap_left" min="0" max="100">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Outer gap right</div><div class="field-desc">Space at the right edge (px)</div></div>
            <input type="number" id="layout-outer_gap_right" min="0" max="100">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Outer gap top</div><div class="field-desc">Space at the top edge (px)</div></div>
            <input type="number" id="layout-outer_gap_top" min="0" max="100">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Outer gap bottom</div><div class="field-desc">Space at the bottom edge (px)</div></div>
            <input type="number" id="layout-outer_gap_bottom" min="0" max="100">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Centering mode</div><div class="field-desc">How the focused column is positioned in the viewport</div></div>
            <div class="combobox" id="cb-layout-centering_mode">
              <button class="combobox-trigger" type="button"><span class="combobox-text">Center</span><svg class="combobox-chevron" viewBox="0 0 12 12"><path d="M2.15 4.65a.5.5 0 01.7 0L6 7.79l3.15-3.14a.5.5 0 11.7.7l-3.5 3.5a.5.5 0 01-.7 0l-3.5-3.5a.5.5 0 010-.7z"/></svg></button>
              <div class="combobox-popup">
                <div class="combobox-option selected" data-value="center">Center</div>
                <div class="combobox-option" data-value="just_in_view">Just in view</div>
              </div>
            </div>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Center past edges</div><div class="field-desc">Allow centering to scroll past content boundaries</div></div>
            <label class="toggle"><input type="checkbox" id="layout-center_past_edges"><span class="track"></span><span class="thumb"></span></label>
          </div>
        </div>
        <h3 class="section-subtitle">Width presets</h3>
        <p class="section-desc">Column width presets as viewport fractions. The first preset is the default width for new columns.</p>
        <div class="table-wrap">
          <table>
            <thead><tr><th>Fraction</th><th style="width:36px"></th></tr></thead>
            <tbody id="width-presets-body"></tbody>
          </table>
        </div>
        <div class="table-actions"><button class="btn btn-sm" onclick="addPresetRow('width',null)">+ Add preset</button></div>
        <h3 class="section-subtitle">Height presets</h3>
        <p class="section-desc">Window height presets as column fractions for cycling window heights.</p>
        <div class="table-wrap">
          <table>
            <thead><tr><th>Fraction</th><th style="width:36px"></th></tr></thead>
            <tbody id="height-presets-body"></tbody>
          </table>
        </div>
        <div class="table-actions"><button class="btn btn-sm" onclick="addPresetRow('height',null)">+ Add preset</button></div>
      </div>

      <!-- Appearance -->
      <div id="sec-appearance" class="section">
        <h2 class="section-title">Appearance</h2>
        <div class="card">
          <div class="info-bar" id="hc-info-bar" hidden>
            <svg class="info-bar-icon" viewBox="0 0 16 16">
              <path d="M8 1a7 7 0 100 14A7 7 0 008 1zm-.75 3.5a.75.75 0 011.5 0v4a.75.75 0 01-1.5 0v-4zm.75 7a.75.75 0 110-1.5.75.75 0 010 1.5z"/>
            </svg>
            <div class="info-bar-content">
              <div class="info-bar-title">High contrast mode</div>
              <div class="info-bar-message">Border color is overridden by the system highlight color.</div>
            </div>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Active border</div><div class="field-desc">Highlight the focused window border</div></div>
            <label class="toggle"><input type="checkbox" id="appearance-active_border"><span class="track"></span><span class="thumb"></span></label>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Border color</div><div class="field-desc">Active window border color</div></div>
            <input type="color" id="appearance-active_border_color">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Border width</div><div class="field-desc">Active window border thickness (px)</div></div>
            <input type="number" id="appearance-active_border_width" min="1" max="20">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Border position</div><div class="field-desc">Draw border outside or inside the window frame</div></div>
            <div class="combobox" id="cb-appearance-active_border_position">
              <button class="combobox-trigger" type="button"><span class="combobox-text">Outside</span><svg class="combobox-chevron" viewBox="0 0 12 12"><path d="M2.15 4.65a.5.5 0 01.7 0L6 7.79l3.15-3.14a.5.5 0 11.7.7l-3.5 3.5a.5.5 0 01-.7 0l-3.5-3.5a.5.5 0 010-.7z"/></svg></button>
              <div class="combobox-popup">
                <div class="combobox-option selected" data-value="outside">Outside</div>
                <div class="combobox-option" data-value="inside">Inside</div>
              </div>
            </div>
          </div>
        </div>
      </div>

      <!-- Behavior -->
      <div id="sec-behavior" class="section">
        <h2 class="section-title">Behavior</h2>
        <div class="card">
          <div class="field">
            <div class="field-info"><div class="field-label">Focus new windows</div><div class="field-desc">Automatically focus newly opened windows</div></div>
            <label class="toggle"><input type="checkbox" id="behavior-focus_new_windows"><span class="track"></span><span class="thumb"></span></label>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Track focus changes</div><div class="field-desc">Follow Windows focus changes</div></div>
            <label class="toggle"><input type="checkbox" id="behavior-track_focus_changes"><span class="track"></span><span class="thumb"></span></label>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Focus follows mouse</div><div class="field-desc">Focus windows on mouse enter</div></div>
            <label class="toggle"><input type="checkbox" id="behavior-focus_follows_mouse"><span class="track"></span><span class="thumb"></span></label>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Focus delay</div><div class="field-desc">Delay before focus change on mouse enter (ms)</div></div>
            <input type="number" id="behavior-focus_follows_mouse_delay_ms" min="50" max="2000">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Disable snap layouts</div><div class="field-desc">Prevent Windows 11 edge-drag snapping for tiled windows</div></div>
            <label class="toggle"><input type="checkbox" id="behavior-disable_snap_layouts"><span class="track"></span><span class="thumb"></span></label>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Log level</div><div class="field-desc">Daemon logging verbosity</div></div>
            <div class="combobox" id="cb-behavior-log_level">
              <button class="combobox-trigger" type="button"><span class="combobox-text">Info</span><svg class="combobox-chevron" viewBox="0 0 12 12"><path d="M2.15 4.65a.5.5 0 01.7 0L6 7.79l3.15-3.14a.5.5 0 11.7.7l-3.5 3.5a.5.5 0 01-.7 0l-3.5-3.5a.5.5 0 010-.7z"/></svg></button>
              <div class="combobox-popup">
                <div class="combobox-option" data-value="trace">Trace</div>
                <div class="combobox-option" data-value="debug">Debug</div>
                <div class="combobox-option selected" data-value="info">Info</div>
                <div class="combobox-option" data-value="warn">Warn</div>
                <div class="combobox-option" data-value="error">Error</div>
              </div>
            </div>
          </div>
        </div>
      </div>

      <!-- Hotkeys -->
      <div id="sec-hotkeys" class="section">
        <h2 class="section-title">Hotkeys</h2>
        <div class="table-wrap">
          <table>
            <thead><tr><th>Command</th><th>Key binding</th><th style="width:36px"></th></tr></thead>
            <tbody id="hotkeys-body"></tbody>
          </table>
        </div>
        <div class="table-actions"><button class="btn btn-sm" onclick="resetHotkeys()">Reset to defaults</button></div>
      </div>

      <!-- Rules -->
      <div id="sec-rules" class="section">
        <h2 class="section-title">Window rules</h2>
        <div class="table-wrap">
          <table>
            <thead><tr><th>Class</th><th>Title</th><th>Executable</th><th>Action</th><th style="width:36px"></th></tr></thead>
            <tbody id="rules-body"></tbody>
          </table>
        </div>
        <div class="table-actions"><button class="btn btn-sm" onclick="addRuleRow({})">+ Add rule</button></div>
      </div>

      <!-- Gestures -->
      <div id="sec-gestures" class="section">
        <h2 class="section-title">Gestures</h2>
        <div class="card">
          <div class="field">
            <div class="field-info"><div class="field-label">Enable gestures</div><div class="field-desc">Enable touchpad gesture support</div></div>
            <label class="toggle"><input type="checkbox" id="gestures-enabled"><span class="track"></span><span class="thumb"></span></label>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Swipe left</div><div class="field-desc">Three-finger swipe left command</div></div>
            <div class="combobox" id="cb-gestures-swipe_left" data-value="focus_left"></div>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Swipe right</div><div class="field-desc">Three-finger swipe right command</div></div>
            <div class="combobox" id="cb-gestures-swipe_right" data-value="focus_right"></div>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Swipe up</div><div class="field-desc">Three-finger swipe up command</div></div>
            <div class="combobox" id="cb-gestures-swipe_up" data-value="focus_up"></div>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Swipe down</div><div class="field-desc">Three-finger swipe down command</div></div>
            <div class="combobox" id="cb-gestures-swipe_down" data-value="focus_down"></div>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label" id="lbl-scroll-up">Scroll up</div><div class="field-desc">Scroll wheel up command</div></div>
            <div class="combobox" id="cb-gestures-scroll_up" data-value="focus_next"></div>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label" id="lbl-scroll-down">Scroll down</div><div class="field-desc">Scroll wheel down command</div></div>
            <div class="combobox" id="cb-gestures-scroll_down" data-value="focus_prev"></div>
          </div>
        </div>
      </div>

      <!-- Snap Hints -->
      <div id="sec-snaphints" class="section">
        <h2 class="section-title">Snap hints</h2>
        <div class="card">
          <div class="field">
            <div class="field-info"><div class="field-label">Enable snap hints</div><div class="field-desc">Show visual feedback during resize operations</div></div>
            <label class="toggle"><input type="checkbox" id="snaphints-enabled"><span class="track"></span><span class="thumb"></span></label>
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Duration</div><div class="field-desc">How long hints are shown (ms)</div></div>
            <input type="number" id="snaphints-duration_ms" min="50" max="2000">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Opacity</div><div class="field-desc">Hint overlay opacity</div></div>
            <div class="slider-group">
              <input type="range" id="snaphints-opacity" min="0" max="255" oninput="document.getElementById('opacity-val').textContent=this.value">
              <span class="slider-val" id="opacity-val">128</span>
            </div>
          </div>
        </div>
      </div>

      <!-- About -->
      <div id="sec-about" class="section">
        <h2 class="section-title">About</h2>
        <div class="card about-card">
          <div class="about-header">
            <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="55.8 55 137.72 139.27" class="about-icon"><path fill="#30373b" d="M193.42 113.77c-4.61 34.32-25.38 66.18-64.94 80.49-1.37-11.15-8.1-35.25-19.51-37.32-8.88-1.65-11.91 10.04-24.29 10.04-10.61 0-17.14-8.12-17.14-15.47-5.42-1.4-5.45-9.13-7.45-14.2-.8-2.17-4.26-3.54-4.26-9.4 0-5.2 4.95-11 12.93-21.72 2.85-3.82 3.62-6 3.62-10.1 0-8.35 9.34-15.98 22.43-25.78-.74-6.7 2.37-15.31 8.4-15.31 3.2 0 4.55 2.65 13.24 7.5 3.71-.59 9.28-.91 14.7-.79 5.12-5.26 8.92-6.71 15.04-6.71 10.82 0 15.52 8.88 15.52 16.46v3.64c12.92 10.63 22.31 20.41 31.48 37.12.42.64.33.97.23 1.55"/><path fill="#f0e9d4" d="M130.01 186.78c-1.9-9.77-4.29-16.39-4.88-18.06-.32-.91.45-1.09.68-1.29 13.44-11.28 27.96-24.34 27.21-50.55-.1-1.65-2.54-3.79-2.91 0-1.99 20.16-11.99 31.08-27.2 30.6-8.1-.27-14.02.63-17.72 3.05 2.64-2.72 4.57-4.9 3.82-7.92-1.27-4.42-8.93-3.52-10.53-5.36-.76-1.88-2.29-1.53-2.57-.41-.32 1.22-1.72 1.47-2.88 1.67-4.92.86-10.85.46-17.32.46-1.55 0-1.64 2.02 0 2.25 4.9.69 11 .87 15.68-.11.78 1.5-.98 3.77-5.37 3.65-2.26-.05-4.06-.07-5.44-.05-1.67.03-1.92 2.11 0 2.45 1.82.35 4.14.65 4.51 2.53.32 1.5-1.61 1.5-3.01 1.03-3.45-.99-6.19-1.6-10.69-3.81-3.58-1.85-5.31-8.85-1.11-11.6 2.89-1.85 3.45-2.7 5.09-5.99.95-2.04-.68-4.07-2.95-2.3-2.26 1.76-2.21 1.91-4.61.5-1.99-1.11-4-.38-6.4.29-1.63.52-1.54-1.89-.23-4.18 3.53-6.02 12.31-15.67 14.99-21.47 1.27-3.02 1.13-5.1 1.45-7.97.75-6.01 16.49-19.11 27.29-23.73 6.32-2.82 14.07-3.73 23.06-3.64-1.22 2.61-2.49 4.94-5.29 5.84-1.4.23-1.67 1.05-1.58 2.05.14 1.07 1.03 1.36 2.1 1.27 3.98-.23 5.47-3.09 7.64-7.37 3.25-6.46 8.27-8.88 14.2-8.88 8.68 0 11.75 7.2 11.75 13.98 0 5.01-3.68 8.3-3.68 12.4 0 1.95.9 2.77.8 3.99-.23 2.86-4.73 6.16-5 10.01-.19 2.17.71 2.17 1.71 1.47 6.99-4.28 9.25-8.86 10.1-19.75 1.73.9 3.56 3.08 4.21 4.28-.85.47-2.02 1.21-2.02 2.86 0 1.52.42 1.75.42 3.4 0 1.85 2.59 2.59 3.19.64.6-1.94 1.29-2.85 3.29-2.61 2.26.32 2.91 2.38 2.91 4.7 0 1.31.41 2.61-.18 3.41-.85 1.08-2.35.12-3.62-1.21-1.84-2.08-3.78 0-3.6 1.85.37 3.01 4.06 5.19 7.13 4.72 2.63-.46 4.26-2.12 5.62-3.64 3.94 4.02 8.64 11.38 10.09 14.13-1.07 8.35-2.8 14.8-3.97 18.92-.42 1.41-1.36.23-1.78-.47-1.54-2.53-3.7-3-6.1-2.88-6.22.35-7.39 8.82-5.56 12.68 1.27 3.02 2.81 3.02 4.08 2.04 1.36-1.08.9-2.48.14-5.5-.65-2.85 1.66-4.13 3.2-3.43s1.64 2.47 2.19 5.99c.32 2.06-1.27 4.7-2.33 6.75-4.12 7.46-9.91 16.01-12.98 18.63-1.36.58-3.04-2.93-6.83-3.28-7.09-.72-9.84 5.91-8.3 12.25.51 2.27.19 2.62-.98 3.35-6.85 4.7-12.87 6.65-18.93 7.37"/><path fill="#f0e9d4" d="M71.81 150.52c.32 6.64 5.44 11.01 12.67 11.01 5.03 0 7.71-2.42 10.15-4.37-2.35-.75-4.61-.63-6.01-2.03-.85-.98-1.45-.87-2.9-.75-4.74.35-7.91-1.59-13.91-3.86M155.01 174.01l5.21-5c-1.17-1.65-3.1-1.77-4.17-1.3-2.26 1.07-2.44 3.6-1.84 6.02z"/><path fill="#f9f8f5" d="M77.02 101.01c.6.59 2.33 1.32 4.59 1.08 5.37-.61 7.48-2.38 8.5-3.46 2.17-2.42 2.92-2.07 6.26-1.96 4.04.15 8.12.74 11.96.86 1.55.12 1.36 2.54-.62 2.89-1.78.35-2.63.94-3.27 3.8-1 4.42-4.35 6.59-8.43 6.24-3.3-.35-5.99 0-7.98 2.18-2.4 2.85-8 8.65-9.12 12.51-.59 2.27-.27 2.81-.97 3.28-.96.53-1.86-1.32-4.21-.82-2.12.47-2.71 1.06-4.55-.09-1.55-.99-3.94-.5-5.77.09-2.4.82-3.2-.5-2.03-2.82 2.54-5.01 11.74-15.54 14.33-19.96 1.01-1.92 1.01-4.17 1.31-3.82"/><path fill="#30373b" d="M145.51 66.06c-7.34 0-11.69 11.92-12.86 16.3-.46 1.75.39 2.45 1.46 2.33 3.6-.35 6.14 1.05 6.88 4.07.75 3.13.84 5.2 2.77 5.2 2.4 0 3.05-5.2 3.05-9.05 0-3.54 4.31-6.07 4.31-11.55 0-4.28-2.17-7.3-5.61-7.3M130.97 101.36c-1.45-2.17-.44-3.25 1.24-2.9 3.02.73 3.87 5.53 3.64 7.6-.23 1.65-3.16 2.12-3.58 0-.55-2.31-.33-3.25-1.3-4.7M122.51 109.48c-1.84-1.65-1.37-3.42.61-3.42 3.49 0 5.75 3.13 5.61 7.39-.09 1.95.42 2.56.19 3.74-.33 1.52-3.91 1.52-3.91-1.13 0-2.87 0-4.64-2.5-6.58M134.61 113.9c1.4-1.53 4.14 0 5.1 2.52.6 1.65 1.55 1.3 2.1 1.65 1.36.7 1.36 2.1.94 3.94-.46 1.95-4.04 1.6-4.23-.67-.23-2.63-1.5-2.51-2.76-3.5-1.45-1.07-1.95-2.92-1.15-3.94M142.23 107.83c-.55-2.07.1-.95-.04-1.3-.65-1.75 2.75-.47 3.02 1.48.51 3.02-.04 5.08-1.68 5.08-1.72 0-.87-3.02-1.3-5.26M158.17 103.75c.55-2.63 2.19-2.04 2.94-1.45 1.84 1.33 2.48 2.85.94 5.71-1.17 2.32-2.02 2.1-3.33 2.06-1.94-.23-2.31-2.18-1.4-4.01.42-.85.65-1.28.85-2.31M171.92 109.95c1.27-3.86 5.48-3.13 7.74-1.61 2.45 1.65 1.9 4.18.17 4.3-1.72.11-2.19-1.41-3.26-.95-1.26.59-.46 2.36-.56 3.67-.22 1.65-1.86 1.77-2.93.37-1.36-1.84-1.77-4.12-1.16-5.78M177.98 118.42c.85-.7 1.31-.47 1.96-2 .8-1.94 4.2-.77 3.88 2.26-.33 2.74-1.87 4.8-4.31 4.68-3.03-.23-3.49-3.35-1.53-4.94M161.72 129.69c3.07 0 3.44 2.63 2.69 5.16-.46 1.64-.18 1.88-.69 2.73-1 1.65-4.4.73-4.91-1.33-.27-1.07.37-1.64-.27-3.29-.66-1.85.69-3.27 3.18-3.27M165.81 147.51c1.17-2.32 3.24-2.09 3.89.55 1.06 4.16-.97 9.63-6.57 10.1-2.85.23-3.12-2.04-2.32-3.69.95-2.06 3.02-1.6 3.77-3.36.65-1.42.51-2.15 1.23-3.6M155.72 145.42c1.89-.7 3.19.96 2.59 3.49-.55 2.28-3.29 2.4-4.74 4.34-1.64 2.43-4.85 2.78-5.22-.24-.42-3.29 1.12-4.6 3.06-4.6 2.4 0 2.86-2.42 4.31-2.99M143.21 161.45c2.3-1.41 3.89.54 3.14 2.81-.74 2.27-.56 1.92-.74 3.45-.33 2.38-4.26 2.61-5.43.18-1.27-2.86.09-4.8 3.03-6.44M136.03 174.36c1.4-1.76 3.8-1.41 3.98 1.01.18 2.53-1.18 4.47-3 4.35-2.17-.22-2.26-3.25-.98-5.36M163.04 115.73c2.69-.24 2.55 3.5 2.18 6.13-.46 2.75-2.09 2.87-2.84 2.75-2.31-.35-2.26-2.98-2.07-5.62.23-2.42 1.03-3.15 2.73-3.26M170.09 123.87c.23-1.77 1.39-1.78 2.04-1.66 1.59.35 1.59 1.9 1.22 3.1-.74 2.17-3.72 1.58-3.26-1.44M88.81 90.95c.6-3.74 2.14-6.49 4.54-6.26s2.77 3.86.78 6.99c-1.35 2.33-2 4.09-3.26 4.82-1.64.91-2.71-1.83-2.06-5.55M83.44 93.6c1.63-.73 2.1.53 1.87 2.18-.33 1.94-2.68 1.71-2.91.12-.23-1.29 0-1.89 1.04-2.3M104.11 74.91c2.68-.99 3.64 1.07 2.63 2.92-1.01 1.95-3.69 1.37-4.91 3.02-1.45 2.16-3.22 2.39-3.59.33-.6-2.86 2.09-4.92 5.87-6.27M113.71 71.4c2.69-.47 4.23 1.29 4.14 3.36-.14 2.06-2.13 1.95-3.13 1.25-1.11-.73-1.58-.38-2.23-.5-1.54-.35-1.68-3.56 1.22-4.11M111.68 79.63c1.94-.24 2.45.84 2.35 1.91-.22 1.66-1.95 1.77-3.22.86-1.26-1.08-.94-2.51.87-2.77M119.87 81.57c1.84-.47 2.75 1.48 2.52 2.89-.32 1.65-2.67 1.65-3.57.12-1.01-1.77-.41-2.68 1.05-3.01M123.07 87.71c2.17-.99 4.76.78 5.71 2.19 1.17 1.85-.66 3.17-2.2 2.58-1.55-.73-2.51-1.57-3.67-2.58-1.17-1.08-.8-1.67.16-2.19M138.92 99.21c.85-.98 2.3.09 2.12 1.74-.23 1.41-2.22 1.18-2.68-.23-.28-.91.05-1.02.56-1.51M123.39 95.36c2.31-.24 2.82 1.29 2.59 2.36-.42 1.86-2.54 1.74-3.61.91-1.45-1.22-.94-3.03 1.02-3.27M115.31 92.72c2.78-1.41 4 .44 3.31 2.5-.9 2.64-4.59 2.41-5.29 1.94-1.31-.95.09-3.38 1.98-4.44M107.93 86.11c2.49-.73 4.75.92 4.8 2.33.09 1.66-.46 2.28-1.91 2.28-1.68 0-3.18-.91-3.92-1.38-.85-.73-.58-2.67 1.03-3.23M98.92 88.18c.8-1.53 2.16-1.65 3.28-.92 1.27.92.52 2.2-.88 3.72-1.36 1.53-2.26 2.44-3.27 2.56-1.27.12-.67-2.63.87-5.36M87.41 109.13c.95-2.86 3.12-4.12 4.07-6.65.85-2.53.25-5.07 4.89-4.95 3.69.12 6.52.7 10.45.93 1.99.12 1.89 2.29-.65 2.41-2.35.11-3.1.58-3.75 2.85-1.11 3.73-4.41 5.04-7.35 4.46-3.02-.73-4.47.58-6.06 2.85-1.4 1.75-2.4.35-1.6-1.9"/><path fill="#f0e9d4" d="M98.83 100.31c1.45-.59 2.3.39 2.21 1.46-.23 1.52-1.96 1.64-2.87.84-1.06-.99-.46-1.89.66-2.3"/><path fill="#30373b" d="M116.01 101.36c2.9-.59 3.17 2.16 2.52 4.58s-2.48 2.19-3.03 1.95c-1.54-.6-.99-2.25-1.37-3.78-.51-1.85.29-2.4 1.88-2.75M104.89 113.71c2.54-.7 4.43.19 4.34 2.04-.18 1.95-2.4 2.3-4.39 1.83-2.12-.57-2.44-3.1.05-3.87M99.47 119.23c1.54-.12 3.27 1.19 3.64 2.38.42 1.3-1.03 1.65-2.58.75-1.93-1.17-2.78-3.02-1.06-3.13M92.72 118.81c.7-.98 2.01-.39 2.52.79.64 1.41-.1 2.14-1.32 1.55-1.21-.73-1.68-1.64-1.2-2.34M95.51 124.72c.91-.73 1.92-.11 2.1.97.23 1.21-1.08 1.67-1.88.82-.7-.73-.7-1.31-.22-1.79M102.31 128.08c1.01-1.41 2.41-.82 2.6.47.32 1.76-.58 2.67-1.8 2.44-1.5-.35-1.68-1.67-.8-2.91M108.11 130.81c1.22-1.53 3.39-1.18 3.53.35.18 1.85-1.13 2.75-2.72 2.52-1.59-.35-1.64-1.67-.81-2.87M117.76 131.34c2.85-.59 5.06.23 5.15 1.76.14 1.85-.82 2.21-2.89 2.21-2.99 0-3.99-.7-3.99-1.98-.09-1.17.42-1.64 1.73-1.99M116.03 139.61c2.17-.73 3.08.35 2.75 2-.42 1.85-2.68 1.73-3.15 1.62-1.4-.35-1.3-2.88.4-3.62M128.64 137.76c4.08-.9 5.97-.55 6.48 1.39.69 2.74-1.99 4.27-6.11 4.51-2.94.23-3.49-1.72-3.26-3.31.18-1.32 1.34-2.16 2.89-2.59M133.81 128.43c2.12-1.94 4.91 0 5.82 1.95 1.01 2.27-1.21 4.03-2.89 3.56-2.68-.84-4.72-3.7-2.93-5.51M119.07 121.39c1.54-1.09 3.04.22 4.58.8 1.64.59 3.62 0 4.47 1.76.8 1.76-.05 4.03-2.21 3.8-1.99-.23-3.1-2-5.5-2.35-2.07-.35-2.72-2.77-1.34-4.01M115.72 114.31c1.45-.69 2.2 0 2.11.91-.14 1.2-1.59 1.55-2.43 1.31-1.07-.35-.89-1.52.32-2.22M80.21 132.81c3.94-.12 8.5-.59 12.07-2.12 1.84-.73 2.79 1.53 1.16 2.51-2.4 1.41-8.72 1.98-13.32 1.86-1.78 0-1.83-2.16.09-2.25"/></svg>
            <div class="about-title-block">
              <div class="about-name">LeopardWM</div>
              <div class="about-version">Version 0.1.0</div>
            </div>
          </div>
          <div class="about-desc">A scrollable tiling window manager for Windows 10/11. Scroll-first layout with vsync-aligned animations, written in Rust.</div>
          <div class="about-divider"></div>
          <div class="about-meta">
            <div class="about-row"><span class="about-label">Created by</span><span class="about-value">Jose Cardama</span></div>
            <div class="about-row"><span class="about-label">Contributors</span><a href="#" class="about-link" onclick="window.ipc.postMessage(JSON.stringify({action:'open_url',url:'https://github.com/jcardama/LeopardWM/graphs/contributors'}));return false;">View on GitHub</a></div>
            <div class="about-row"><span class="about-label">License</span><span class="about-value">GPL-3.0</span></div>
            <div class="about-row"><span class="about-label">Source</span><a href="#" class="about-link" onclick="window.ipc.postMessage(JSON.stringify({action:'open_url',url:'https://github.com/jcardama/LeopardWM'}));return false;">github.com/jcardama/LeopardWM</a></div>
          </div>
          <div class="about-divider"></div>
          <div class="about-legal">
            <div class="about-legal-title">Third-party notices</div>
            <div class="about-legal-text">LeopardWM uses open-source libraries licensed under MIT, Apache-2.0, MPL-2.0, and other permissive licenses. Key dependencies include the <strong>windows</strong> crate (Microsoft, MIT/Apache-2.0), <strong>wry</strong> and <strong>tray-icon/muda</strong> (Tauri Programme, MIT/Apache-2.0), <strong>tokio</strong> (MIT), and <strong>WebView2</strong> (Microsoft). Full dependency list is available via <code>cargo tree</code> in the source repository.</div>
          </div>
          <div class="about-divider"></div>
          <a href="#" class="about-coffee" onclick="window.ipc.postMessage(JSON.stringify({action:'open_url',url:'https://buymeacoffee.com/jcardama'}));return false;">
            <svg viewBox="0 0 24 24" width="20" height="20" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M17 8h1a4 4 0 010 8h-1"/><path d="M3 8h14v9a4 4 0 01-4 4H7a4 4 0 01-4-4V8z"/><line x1="6" y1="2" x2="6" y2="4"/><line x1="10" y1="2" x2="10" y2="4"/><line x1="14" y1="2" x2="14" y2="4"/></svg>
            Buy me a coffee
          </a>
        </div>
      </div>
    </div>

  </div>
</div>

<script>
/* ── Navigation ─────────────────────────────────────────────────────── */
document.querySelectorAll('.nav-item[data-section]').forEach(function(link) {
  link.addEventListener('click', function(e) {
    e.preventDefault();
    document.querySelectorAll('.nav-item').forEach(function(a) { a.classList.remove('active'); });
    link.classList.add('active');
    document.querySelectorAll('.section').forEach(function(s) { s.classList.remove('active'); });
    document.getElementById('sec-' + link.dataset.section).classList.add('active');
  });
});

/* ── ComboBox ───────────────────────────────────────────────────────── */
function initCombobox(cb) {
  var trigger = cb.querySelector('.combobox-trigger');
  var popup = cb.querySelector('.combobox-popup');
  trigger.addEventListener('click', function(e) {
    e.stopPropagation();
    document.querySelectorAll('.combobox.open').forEach(function(other) {
      if (other !== cb) other.classList.remove('open');
    });
    if (!cb.classList.contains('open')) {
      /* Position popup using fixed coords from trigger rect */
      var rect = trigger.getBoundingClientRect();
      popup.style.minWidth = Math.max(rect.width, 100) + 'px';
      popup.style.width = 'auto';
      popup.style.maxWidth = (window.innerWidth - rect.left - 8) + 'px';
      popup.style.left = rect.left + 'px';
      var spaceBelow = window.innerHeight - rect.bottom - 8;
      var spaceAbove = rect.top - 8;
      if (spaceBelow >= spaceAbove) {
        popup.style.top = (rect.bottom + 4) + 'px';
        popup.style.bottom = 'auto';
        popup.style.maxHeight = (spaceBelow - 4) + 'px';
      } else {
        popup.style.bottom = (window.innerHeight - rect.top + 4) + 'px';
        popup.style.top = 'auto';
        popup.style.maxHeight = (spaceAbove - 4) + 'px';
      }
    }
    cb.classList.toggle('open');
  });
  cb.querySelectorAll('.combobox-option').forEach(function(opt) {
    opt.addEventListener('click', function(e) {
      e.stopPropagation();
      cb.querySelector('.combobox-text').textContent = opt.textContent;
      cb.dataset.value = opt.dataset.value;
      cb.classList.remove('open');
      cb.querySelectorAll('.combobox-option').forEach(function(o) { o.classList.remove('selected'); });
      opt.classList.add('selected');
      autoSave(0);
    });
  });
}
document.querySelectorAll('.combobox').forEach(function(cb) {
  if (cb.querySelector('.combobox-trigger')) initCombobox(cb);
});
document.addEventListener('click', function() {
  document.querySelectorAll('.combobox.open').forEach(function(cb) { cb.classList.remove('open'); });
});

/* ── High Contrast live detection ─────────────────────────────────── */
if (window.matchMedia) {
  window.matchMedia('(forced-colors: active)').addEventListener('change', function(e) {
    var bar = document.getElementById('hc-info-bar');
    var picker = document.getElementById('appearance-active_border_color');
    if (bar) bar.hidden = !e.matches;
    if (picker) picker.disabled = e.matches;
  });
}

/* ── Helpers ─────────────────────────────────────────────────────────── */
function val(id) { return document.getElementById(id).value; }
function num(id) { return parseInt(document.getElementById(id).value, 10) || 0; }
function addPresetRow(kind, value) {
  var tbody = document.getElementById(kind + '-presets-body');
  var tr = document.createElement('tr');
  var v = (value != null) ? value : '';
  tr.innerHTML =
    '<td><input type="text" class="preset-val" value="' + v + '" placeholder="0.5"></td>' +
    '<td><button class="row-delete" onclick="this.closest(\'tr\').remove();autoSave(0)">' + deleteIcon + '</button></td>';
  tbody.appendChild(tr);
  wrapAllInputs(tr);
  tr.querySelector('input').addEventListener('input', function() { autoSave(500); });
}
function readPresets(kind) {
  var vals = [];
  document.querySelectorAll('#' + kind + '-presets-body tr').forEach(function(tr) {
    var v = parseFloat(tr.querySelector('.preset-val').value.trim());
    if (!isNaN(v) && v > 0 && v <= 1) vals.push(v);
  });
  return vals;
}
function checked(id) { return document.getElementById(id).checked; }
function setVal(id, v) { document.getElementById(id).value = v; }
function setChecked(id, v) { document.getElementById(id).checked = !!v; }
function cbVal(id) {
  var el = document.getElementById(id);
  return el ? (el.dataset.value || '') : '';
}
function setCb(id, value) {
  var cb = document.getElementById(id);
  if (!cb) return;
  cb.dataset.value = value;
  var opts = cb.querySelectorAll('.combobox-option');
  opts.forEach(function(o) {
    o.classList.remove('selected');
    if (o.dataset.value === value) {
      o.classList.add('selected');
      cb.querySelector('.combobox-text').textContent = o.textContent;
    }
  });
}
function hexToInput(hex) { return '#' + (hex || '000000').toLowerCase(); }
function inputToHex(v) { return v.replace('#', '').toUpperCase(); }

/* ── Init ────────────────────────────────────────────────────────────── */
function init(cfg) {
  setVal('layout-gap', cfg.layout.gap);
  setVal('layout-outer_gap_left', cfg.layout.outer_gap_left);
  setVal('layout-outer_gap_right', cfg.layout.outer_gap_right);
  setVal('layout-outer_gap_top', cfg.layout.outer_gap_top);
  setVal('layout-outer_gap_bottom', cfg.layout.outer_gap_bottom);
  document.getElementById('width-presets-body').innerHTML = '';
  (cfg.layout.width_presets || [0.333,0.5,0.667]).forEach(function(v) { addPresetRow('width', v); });
  document.getElementById('height-presets-body').innerHTML = '';
  (cfg.layout.height_presets || [0.333,0.5,0.667]).forEach(function(v) { addPresetRow('height', v); });
  setCb('cb-layout-centering_mode', cfg.layout.centering_mode);
  setChecked('layout-center_past_edges', cfg.layout.center_past_edges);

  setChecked('appearance-active_border', cfg.appearance.active_border);
  setVal('appearance-active_border_color', hexToInput(cfg.appearance.active_border_color));
  setVal('appearance-active_border_width', cfg.appearance.active_border_width);
  setCb('cb-appearance-active_border_position', cfg.appearance.active_border_position);

  if (cfg.high_contrast) {
    document.getElementById('hc-info-bar').hidden = false;
    document.getElementById('appearance-active_border_color').disabled = true;
  }

  setChecked('behavior-focus_new_windows', cfg.behavior.focus_new_windows);
  setChecked('behavior-track_focus_changes', cfg.behavior.track_focus_changes);
  setChecked('behavior-focus_follows_mouse', cfg.behavior.focus_follows_mouse);
  setVal('behavior-focus_follows_mouse_delay_ms', cfg.behavior.focus_follows_mouse_delay_ms);
  setChecked('behavior-disable_snap_layouts', cfg.behavior.disable_snap_layouts !== false);
  setCb('cb-behavior-log_level', cfg.behavior.log_level);

  if (cfg.hotkeys) {
    var raw = cfg.hotkeys.bindings || cfg.hotkeys;
    var bindings = {};
    var scrollMod = 'Ctrl+Alt';
    for (var k in raw) {
      if (k === 'scroll_modifier') { scrollMod = raw[k]; }
      else { bindings[k] = raw[k]; }
    }
    loadHotkeysSorted(bindings, scrollMod);
  }

  document.getElementById('rules-body').innerHTML = '';
  if (cfg.window_rules) { cfg.window_rules.forEach(function(r) { addRuleRow(r); }); }

  setChecked('gestures-enabled', cfg.gestures.enabled);
  setCb('cb-gestures-swipe_left', cfg.gestures.swipe_left);
  setCb('cb-gestures-swipe_right', cfg.gestures.swipe_right);
  setCb('cb-gestures-swipe_up', cfg.gestures.swipe_up);
  setCb('cb-gestures-swipe_down', cfg.gestures.swipe_down);
  setCb('cb-gestures-scroll_up', cfg.gestures.scroll_up);
  setCb('cb-gestures-scroll_down', cfg.gestures.scroll_down);
  updateScrollLabels();

  setChecked('snaphints-enabled', cfg.snap_hints.enabled);
  setVal('snaphints-duration_ms', cfg.snap_hints.duration_ms);
  setVal('snaphints-opacity', cfg.snap_hints.opacity);
  document.getElementById('opacity-val').textContent = cfg.snap_hints.opacity;
}

/* ── Delete icon (X) ─────────────────────────────────────────────────── */
var deleteIcon = '<svg viewBox="0 0 12 12"><line x1="2" y1="2" x2="10" y2="10"/><line x1="10" y1="2" x2="2" y2="10"/></svg>';

var CMD_LABELS = {
  focus_left: 'Focus left', focus_right: 'Focus right',
  focus_up: 'Focus up', focus_down: 'Focus down',
  focus_next: 'Focus next', focus_prev: 'Focus prev',
  move_column_left: 'Move column left', move_column_right: 'Move column right',
  move_window_left: 'Move window left', move_window_right: 'Move window right',
  expel_to_left: 'Expel to left', expel_to_right: 'Expel to right',
  move_window_up: 'Move window up', move_window_down: 'Move window down',
  cycle_width_down: 'Cycle width down', cycle_width_up: 'Cycle width up',
  equalize_widths: 'Equalize widths', center_column: 'Center column', maximize_column: 'Maximize column',
  cycle_height_down: 'Cycle height down', cycle_height_up: 'Cycle height up',
  equalize_heights: 'Equalize heights',
  focus_monitor_left: 'Focus monitor left', focus_monitor_right: 'Focus monitor right',
  move_to_monitor_left: 'Move to monitor left', move_to_monitor_right: 'Move to monitor right',
  close_window: 'Close window', toggle_floating: 'Toggle floating',
  toggle_fullscreen: 'Toggle fullscreen', toggle_pause: 'Toggle pause',
  refresh: 'Refresh', reload: 'Reload config',
  panic_revert: 'Emergency restore',
  switch_workspace_1: 'Workspace 1', switch_workspace_2: 'Workspace 2',
  switch_workspace_3: 'Workspace 3', switch_workspace_4: 'Workspace 4',
  switch_workspace_5: 'Workspace 5', switch_workspace_6: 'Workspace 6',
  switch_workspace_7: 'Workspace 7', switch_workspace_8: 'Workspace 8',
  switch_workspace_9: 'Workspace 9',
  move_to_workspace_1: 'Move to Workspace 1', move_to_workspace_2: 'Move to Workspace 2',
  move_to_workspace_3: 'Move to Workspace 3', move_to_workspace_4: 'Move to Workspace 4',
  move_to_workspace_5: 'Move to Workspace 5', move_to_workspace_6: 'Move to Workspace 6',
  move_to_workspace_7: 'Move to Workspace 7', move_to_workspace_8: 'Move to Workspace 8',
  move_to_workspace_9: 'Move to Workspace 9'
};

/* Defines display order — grouped by category */
var CMD_ORDER = [
  'focus_left', 'focus_right', 'focus_up', 'focus_down',
  'move_column_left', 'move_column_right',
  'move_window_left', 'move_window_right',
  'expel_to_left', 'expel_to_right',
  'move_window_up', 'move_window_down',
  'cycle_width_down', 'cycle_width_up', 'equalize_widths',
  'cycle_height_down', 'cycle_height_up', 'equalize_heights',
  'focus_monitor_left', 'focus_monitor_right',
  'move_to_monitor_left', 'move_to_monitor_right',
  'center_column', 'maximize_column',
  'close_window', 'toggle_floating', 'toggle_fullscreen',
  'toggle_pause', 'refresh', 'reload',
  'panic_revert',
  'switch_workspace_1', 'switch_workspace_2', 'switch_workspace_3',
  'switch_workspace_4', 'switch_workspace_5', 'switch_workspace_6',
  'switch_workspace_7', 'switch_workspace_8', 'switch_workspace_9',
  'move_to_workspace_1', 'move_to_workspace_2', 'move_to_workspace_3',
  'move_to_workspace_4', 'move_to_workspace_5', 'move_to_workspace_6',
  'move_to_workspace_7', 'move_to_workspace_8', 'move_to_workspace_9'
];

var DEFAULT_HOTKEYS = {
  "Ctrl+Alt+H": "focus_left", "Ctrl+Alt+L": "focus_right",
  "Ctrl+Alt+K": "focus_up", "Ctrl+Alt+J": "focus_down",
  "Ctrl+Alt+Shift+H": "move_column_left", "Ctrl+Alt+Shift+L": "move_column_right",
  "Ctrl+Alt+Bracket_Left": "move_window_left", "Ctrl+Alt+Bracket_Right": "move_window_right",
  "Ctrl+Alt+Shift+Bracket_Left": "expel_to_left", "Ctrl+Alt+Shift+Bracket_Right": "expel_to_right",
  "Ctrl+Alt+Shift+K": "move_window_up", "Ctrl+Alt+Shift+J": "move_window_down",
  "Ctrl+Alt+Minus": "cycle_width_down", "Ctrl+Alt+Equals": "cycle_width_up",
  "Ctrl+Alt+0": "equalize_widths",
  "Ctrl+Alt+Shift+Minus": "cycle_height_down", "Ctrl+Alt+Shift+Equals": "cycle_height_up",
  "Ctrl+Alt+Shift+0": "equalize_heights",
  "Ctrl+Alt+Win+Comma": "focus_monitor_left", "Ctrl+Alt+Win+Period": "focus_monitor_right",
  "Ctrl+Alt+Win+Shift+Comma": "move_to_monitor_left", "Ctrl+Alt+Win+Shift+Period": "move_to_monitor_right",
  "Ctrl+Alt+C": "center_column",
  "Ctrl+Alt+M": "maximize_column",
  "Ctrl+Alt+W": "close_window", "Ctrl+Alt+F": "toggle_floating",
  "Ctrl+Alt+Shift+F": "toggle_fullscreen", "Ctrl+Alt+P": "toggle_pause",
  "Ctrl+Alt+R": "refresh", "Ctrl+Alt+Shift+R": "reload",
  "Win+Ctrl+Escape": "panic_revert",
  "Ctrl+Alt+1": "switch_workspace_1", "Ctrl+Alt+2": "switch_workspace_2",
  "Ctrl+Alt+3": "switch_workspace_3", "Ctrl+Alt+4": "switch_workspace_4",
  "Ctrl+Alt+5": "switch_workspace_5", "Ctrl+Alt+6": "switch_workspace_6",
  "Ctrl+Alt+7": "switch_workspace_7", "Ctrl+Alt+8": "switch_workspace_8",
  "Ctrl+Alt+9": "switch_workspace_9",
  "Ctrl+Alt+Shift+1": "move_to_workspace_1", "Ctrl+Alt+Shift+2": "move_to_workspace_2",
  "Ctrl+Alt+Shift+3": "move_to_workspace_3", "Ctrl+Alt+Shift+4": "move_to_workspace_4",
  "Ctrl+Alt+Shift+5": "move_to_workspace_5", "Ctrl+Alt+Shift+6": "move_to_workspace_6",
  "Ctrl+Alt+Shift+7": "move_to_workspace_7", "Ctrl+Alt+Shift+8": "move_to_workspace_8",
  "Ctrl+Alt+Shift+9": "move_to_workspace_9"
};

function cmdLabel(cmd) { return CMD_LABELS[cmd] || cmd; }

function updateScrollLabels() {
  var mod = readScrollModifier();
  var modDisplay = mod.split('+').map(function(s) { return s.trim(); }).join(' + ');
  var up = document.getElementById('lbl-scroll-up');
  var dn = document.getElementById('lbl-scroll-down');
  if (up) up.textContent = modDisplay + ' + Scroll up';
  if (dn) dn.textContent = modDisplay + ' + Scroll down';
}

/* Build gesture comboboxes from CMD_ORDER/CMD_LABELS */
function initGestureCombo(id) {
  var cb = document.getElementById(id);
  if (!cb) return;
  var chevron = '<svg class="combobox-chevron" viewBox="0 0 12 12"><path d="M2.15 4.65a.5.5 0 01.7 0L6 7.79l3.15-3.14a.5.5 0 11.7.7l-3.5 3.5a.5.5 0 01-.7 0l-3.5-3.5a.5.5 0 010-.7z"/></svg>';
  var current = cb.dataset.value || '';
  var options = CMD_ORDER.map(function(cmd) {
    return '<div class="combobox-option' + (cmd === current ? ' selected' : '') + '" data-value="' + cmd + '">' + cmdLabel(cmd) + '</div>';
  }).join('');
  cb.innerHTML = '<button class="combobox-trigger" type="button"><span class="combobox-text">' + cmdLabel(current) + '</span>' + chevron + '</button>' +
    '<div class="combobox-popup">' + options + '</div>';
  initCombobox(cb);
}
['cb-gestures-swipe_left','cb-gestures-swipe_right','cb-gestures-swipe_up','cb-gestures-swipe_down','cb-gestures-scroll_up','cb-gestures-scroll_down'].forEach(initGestureCombo);

function addHotkeyRow(key, cmd) {
  var tbody = document.getElementById('hotkeys-body');
  var tr = document.createElement('tr');
  tr.dataset.cmd = cmd;
  tr.innerHTML =
    '<td class="hk-cmd-label">' + escHtml(cmdLabel(cmd)) + '</td>' +
    '<td><input type="text" class="hk-key" value="' + escAttr(key) + '" placeholder="e.g. Ctrl+Alt+H"></td>' +
    '<td><button class="row-delete" title="Reset to default" onclick="resetHotkeyRow(this.closest(\'tr\'))">' + resetIcon + '</button></td>';
  tbody.appendChild(tr);
  wrapAllInputs(tr);
  tr.querySelector('.hk-key').addEventListener('input', function() { autoSave(500); });
}

function escHtml(s) { var d = document.createElement('div'); d.textContent = s; return d.innerHTML; }

var resetIcon = '<svg viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"><path d="M1.5 2v3h3"/><path d="M2.1 7.5a4 4 0 1 0 .5-4L1.5 5"/></svg>';

function defaultKeyForCmd(cmd) {
  for (var k in DEFAULT_HOTKEYS) { if (DEFAULT_HOTKEYS[k] === cmd) return k; }
  return '';
}

function resetHotkeyRow(tr) {
  var defKey = defaultKeyForCmd(tr.dataset.cmd);
  if (defKey) { tr.querySelector('.hk-key').value = defKey; autoSave(0); }
}

function loadHotkeysSorted(bindings, scrollModifier) {
  var tbody = document.getElementById('hotkeys-body');
  tbody.innerHTML = '';
  /* Build cmd→key map from config */
  var cmdToKey = {};
  Object.entries(bindings).forEach(function(e) { cmdToKey[e[1]] = e[0]; });
  /* Render in defined order */
  CMD_ORDER.forEach(function(cmd) {
    var key = cmdToKey[cmd] || defaultKeyForCmd(cmd);
    addHotkeyRow(key, cmd);
  });
  /* Append any custom commands not in CMD_ORDER */
  Object.entries(bindings).forEach(function(e) {
    if (CMD_ORDER.indexOf(e[1]) === -1) addHotkeyRow(e[0], e[1]);
  });
  /* Scroll modifier row — insert after focus navigation group */
  var tr = document.createElement('tr');
  tr.dataset.special = 'scroll_modifier';
  tr.innerHTML =
    '<td class="hk-cmd-label">Scroll modifier</td>' +
    '<td><input type="text" class="hk-key" value="' + escAttr(scrollModifier || 'Ctrl+Alt') + '" placeholder="e.g. Ctrl+Alt"></td>' +
    '<td><button class="row-delete" title="Reset to default" onclick="this.closest(\'tr\').querySelector(\'.hk-key\').value=\'Ctrl+Alt\';updateScrollLabels();autoSave(0);">' + resetIcon + '</button></td>';
  var focusRows = tbody.querySelectorAll('tr[data-cmd="focus_down"]');
  var anchor = focusRows.length ? focusRows[0] : null;
  if (anchor && anchor.nextSibling) { tbody.insertBefore(tr, anchor.nextSibling); }
  else { tbody.appendChild(tr); }
  wrapAllInputs(tr);
  tr.querySelector('.hk-key').addEventListener('input', function() { updateScrollLabels(); autoSave(500); });
}

function resetHotkeys() {
  loadHotkeysSorted(DEFAULT_HOTKEYS, 'Ctrl+Alt');
  updateScrollLabels();
  autoSave(0);
}

function addRuleRow(r) {
  var tbody = document.getElementById('rules-body');
  var tr = document.createElement('tr');
  var action = r.action || 'tile';
  var actionLabel = action.charAt(0).toUpperCase() + action.slice(1);
  var chevron = '<svg class="combobox-chevron" viewBox="0 0 12 12"><path d="M2.15 4.65a.5.5 0 01.7 0L6 7.79l3.15-3.14a.5.5 0 11.7.7l-3.5 3.5a.5.5 0 01-.7 0l-3.5-3.5a.5.5 0 010-.7z"/></svg>';
  var options = ['tile','float','ignore'].map(function(a) {
    var label = a.charAt(0).toUpperCase() + a.slice(1);
    return '<div class="combobox-option' + (a === action ? ' selected' : '') + '" data-value="' + a + '">' + label + '</div>';
  }).join('');
  tr.innerHTML =
    '<td><input type="text" class="rule-class" value="' + escAttr(r.match_class||'') + '" placeholder="regex"></td>' +
    '<td><input type="text" class="rule-title" value="' + escAttr(r.match_title||'') + '" placeholder="regex"></td>' +
    '<td><input type="text" class="rule-exe" value="' + escAttr(r.match_executable||'') + '" placeholder="app.exe"></td>' +
    '<td><div class="combobox" data-value="' + action + '">' +
      '<button class="combobox-trigger" type="button"><span class="combobox-text">' + actionLabel + '</span>' + chevron + '</button>' +
      '<div class="combobox-popup">' + options + '</div>' +
    '</div></td>' +
    '<td><button class="row-delete" onclick="this.closest(\'tr\').remove();autoSave(0)">' + deleteIcon + '</button></td>';
  tbody.appendChild(tr);
  wrapAllInputs(tr);
  tr.querySelectorAll('input').forEach(function(el) { el.addEventListener('input', function() { autoSave(500); }); });
  initCombobox(tr.querySelector('.combobox'));
}

function escAttr(s) { return (s||'').replace(/&/g,'&amp;').replace(/"/g,'&quot;').replace(/</g,'&lt;'); }

function readConfig() {
  return {
    layout: {
      gap: num('layout-gap'),
      outer_gap_left: num('layout-outer_gap_left'),
      outer_gap_right: num('layout-outer_gap_right'),
      outer_gap_top: num('layout-outer_gap_top'),
      outer_gap_bottom: num('layout-outer_gap_bottom'),
      width_presets: readPresets('width'),
      height_presets: readPresets('height'),
      centering_mode: cbVal('cb-layout-centering_mode'),
      center_past_edges: checked('layout-center_past_edges')
    },
    appearance: {
      active_border: checked('appearance-active_border'),
      active_border_color: inputToHex(val('appearance-active_border_color')),
      active_border_width: num('appearance-active_border_width'),
      active_border_position: cbVal('cb-appearance-active_border_position')
    },
    behavior: {
      focus_new_windows: checked('behavior-focus_new_windows'),
      track_focus_changes: checked('behavior-track_focus_changes'),
      focus_follows_mouse: checked('behavior-focus_follows_mouse'),
      focus_follows_mouse_delay_ms: num('behavior-focus_follows_mouse_delay_ms'),
      disable_snap_layouts: checked('behavior-disable_snap_layouts'),
      log_level: cbVal('cb-behavior-log_level')
    },
    hotkeys: Object.assign(readHotkeys(), {
      scroll_modifier: readScrollModifier()
    }),
    window_rules: readRules(),
    gestures: {
      enabled: checked('gestures-enabled'),
      swipe_left: cbVal('cb-gestures-swipe_left'), swipe_right: cbVal('cb-gestures-swipe_right'),
      swipe_up: cbVal('cb-gestures-swipe_up'), swipe_down: cbVal('cb-gestures-swipe_down'),
      scroll_up: cbVal('cb-gestures-scroll_up'), scroll_down: cbVal('cb-gestures-scroll_down')
    },
    snap_hints: {
      enabled: checked('snaphints-enabled'),
      duration_ms: num('snaphints-duration_ms'),
      opacity: num('snaphints-opacity')
    }
  };
}

function readHotkeys() {
  var b = {};
  document.querySelectorAll('#hotkeys-body tr').forEach(function(tr) {
    if (tr.dataset.special) return;
    var k = tr.querySelector('.hk-key').value.trim();
    var c = tr.dataset.cmd;
    if (k && c) b[k] = c;
  });
  return b;
}

function readScrollModifier() {
  var tr = document.querySelector('#hotkeys-body tr[data-special="scroll_modifier"]');
  return tr ? tr.querySelector('.hk-key').value.trim() || 'Ctrl+Alt' : 'Ctrl+Alt';
}

function readRules() {
  var rules = [];
  document.querySelectorAll('#rules-body tr').forEach(function(tr) {
    var r = {};
    var cls = tr.querySelector('.rule-class').value.trim();
    var title = tr.querySelector('.rule-title').value.trim();
    var exe = tr.querySelector('.rule-exe').value.trim();
    var cb = tr.querySelector('.combobox');
    r.action = cb ? (cb.dataset.value || 'tile') : 'tile';
    if (cls) r.match_class = cls;
    if (title) r.match_title = title;
    if (exe) r.match_executable = exe;
    if (cls || title || exe) rules.push(r);
  });
  return rules;
}

/* ── Wrap inputs in .input-wrap for ::after accent line ───────────── */
function wrapInput(input) {
  if (input.parentElement && input.parentElement.classList.contains('input-wrap')) return;
  var wrap = document.createElement('span');
  wrap.className = 'input-wrap';
  input.parentNode.insertBefore(wrap, input);
  wrap.appendChild(input);
}
function wrapAllInputs(root) {
  (root || document).querySelectorAll('input[type="text"], input[type="number"]').forEach(wrapInput);
}
wrapAllInputs();

/* ── Auto-save ────────────────────────────────────────────────────── */
var _saveTimer = null;
function autoSave(delay) {
  clearTimeout(_saveTimer);
  _saveTimer = setTimeout(function() {
    window.ipc.postMessage(JSON.stringify({ action: 'save', config: readConfig() }));
  }, delay || 0);
}

/* Immediate: toggles, color pickers, comboboxes, sliders */
document.querySelectorAll('input[type="checkbox"], input[type="color"], input[type="range"]').forEach(function(el) {
  el.addEventListener('input', function() { autoSave(0); });
});

/* Debounced: text and number inputs */
document.querySelectorAll('input[type="text"], input[type="number"]').forEach(function(el) {
  el.addEventListener('input', function() { autoSave(500); });
});
</script>
</body>
</html>
"##;
