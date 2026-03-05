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

    --toggle-off-stroke: rgba(255,255,255,0.54);
    --toggle-off-thumb: rgba(255,255,255,0.77);
    --toggle-on-thumb: #1a1a1a;

    --scrollbar-thumb: rgba(255,255,255,0.37);
    --shadow: 0 2px 8px rgba(0,0,0,0.36), 0 0 1px rgba(0,0,0,0.24);
  }
}

/* ── Typography ───────────────────────────────────────────────────── */
html, body {
  height: 100%; width: 100%;
  font-family: var(--font);
  font-size: 14px;
  line-height: 20px;
  color: var(--text-primary);
  background: var(--bg);
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
}
.combobox.open .combobox-popup { display: block; }
.combobox-option {
  padding: 6px 12px;
  border-radius: var(--ctrl-radius);
  cursor: pointer;
  font-size: 14px;
  line-height: 20px;
  color: var(--text-primary);
  transition: background 0.06s;
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
  border: 4px solid var(--bg);
  box-shadow: 0 0 0 1px var(--ctrl-stroke-secondary);
}
.slider-val {
  font-size: 12px; line-height: 16px;
  color: var(--text-secondary);
  min-width: 32px; text-align: right;
}
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
  </nav>

  <div class="main">
    <div class="content">
      <!-- Layout -->
      <div id="sec-layout" class="section active">
        <h2 class="section-title">Layout</h2>
        <div class="card">
          <div class="field">
            <div class="field-info"><div class="field-label">Gap</div><div class="field-desc">Space between columns (px)</div></div>
            <input type="number" id="layout-gap" min="0" max="100">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Outer gap</div><div class="field-desc">Space at viewport edges (px)</div></div>
            <input type="number" id="layout-outer_gap" min="0" max="100">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Default column width</div><div class="field-desc">Initial width for new columns (px)</div></div>
            <input type="number" id="layout-default_column_width" min="100" max="4000">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Min column width</div><div class="field-desc">Minimum allowed column width (px)</div></div>
            <input type="number" id="layout-min_column_width" min="0" max="4000">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Max column width</div><div class="field-desc">Maximum allowed column width (px)</div></div>
            <input type="number" id="layout-max_column_width" min="0" max="8000">
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
        </div>
      </div>

      <!-- Appearance -->
      <div id="sec-appearance" class="section">
        <h2 class="section-title">Appearance</h2>
        <div class="card">
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
            <thead><tr><th>Key binding</th><th>Command</th><th style="width:36px"></th></tr></thead>
            <tbody id="hotkeys-body"></tbody>
          </table>
        </div>
        <div class="table-actions"><button class="btn btn-sm" onclick="addHotkeyRow('','')">+ Add binding</button></div>
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
            <input type="text" id="gestures-swipe_left">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Swipe right</div><div class="field-desc">Three-finger swipe right command</div></div>
            <input type="text" id="gestures-swipe_right">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Swipe up</div><div class="field-desc">Three-finger swipe up command</div></div>
            <input type="text" id="gestures-swipe_up">
          </div>
          <div class="field">
            <div class="field-info"><div class="field-label">Swipe down</div><div class="field-desc">Three-finger swipe down command</div></div>
            <input type="text" id="gestures-swipe_down">
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
      popup.style.left = rect.left + 'px';
      popup.style.width = Math.max(rect.width, 100) + 'px';
      var spaceBelow = window.innerHeight - rect.bottom - 8;
      if (spaceBelow < 160) {
        popup.style.bottom = (window.innerHeight - rect.top + 4) + 'px';
        popup.style.top = 'auto';
      } else {
        popup.style.top = (rect.bottom + 4) + 'px';
        popup.style.bottom = 'auto';
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
document.querySelectorAll('.combobox').forEach(function(cb) { initCombobox(cb); });
document.addEventListener('click', function() {
  document.querySelectorAll('.combobox.open').forEach(function(cb) { cb.classList.remove('open'); });
});

/* ── Helpers ─────────────────────────────────────────────────────────── */
function val(id) { return document.getElementById(id).value; }
function num(id) { return parseInt(document.getElementById(id).value, 10) || 0; }
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
  setVal('layout-outer_gap', cfg.layout.outer_gap);
  setVal('layout-default_column_width', cfg.layout.default_column_width);
  setVal('layout-min_column_width', cfg.layout.min_column_width);
  setVal('layout-max_column_width', cfg.layout.max_column_width);
  setCb('cb-layout-centering_mode', cfg.layout.centering_mode);

  setChecked('appearance-active_border', cfg.appearance.active_border);
  setVal('appearance-active_border_color', hexToInput(cfg.appearance.active_border_color));
  setVal('appearance-active_border_width', cfg.appearance.active_border_width);
  setCb('cb-appearance-active_border_position', cfg.appearance.active_border_position);

  setChecked('behavior-focus_new_windows', cfg.behavior.focus_new_windows);
  setChecked('behavior-track_focus_changes', cfg.behavior.track_focus_changes);
  setChecked('behavior-focus_follows_mouse', cfg.behavior.focus_follows_mouse);
  setVal('behavior-focus_follows_mouse_delay_ms', cfg.behavior.focus_follows_mouse_delay_ms);
  setCb('cb-behavior-log_level', cfg.behavior.log_level);

  document.getElementById('hotkeys-body').innerHTML = '';
  if (cfg.hotkeys) {
    var bindings = cfg.hotkeys.bindings || cfg.hotkeys;
    Object.entries(bindings).forEach(function(e) { addHotkeyRow(e[0], e[1]); });
  }

  document.getElementById('rules-body').innerHTML = '';
  if (cfg.window_rules) { cfg.window_rules.forEach(function(r) { addRuleRow(r); }); }

  setChecked('gestures-enabled', cfg.gestures.enabled);
  setVal('gestures-swipe_left', cfg.gestures.swipe_left);
  setVal('gestures-swipe_right', cfg.gestures.swipe_right);
  setVal('gestures-swipe_up', cfg.gestures.swipe_up);
  setVal('gestures-swipe_down', cfg.gestures.swipe_down);

  setChecked('snaphints-enabled', cfg.snap_hints.enabled);
  setVal('snaphints-duration_ms', cfg.snap_hints.duration_ms);
  setVal('snaphints-opacity', cfg.snap_hints.opacity);
  document.getElementById('opacity-val').textContent = cfg.snap_hints.opacity;
}

/* ── Delete icon (X) ─────────────────────────────────────────────────── */
var deleteIcon = '<svg viewBox="0 0 12 12"><line x1="2" y1="2" x2="10" y2="10"/><line x1="10" y1="2" x2="2" y2="10"/></svg>';

function addHotkeyRow(key, cmd) {
  var tbody = document.getElementById('hotkeys-body');
  var tr = document.createElement('tr');
  tr.innerHTML =
    '<td><input type="text" class="hk-key" value="' + escAttr(key) + '" placeholder="e.g. Win+H"></td>' +
    '<td><input type="text" class="hk-cmd" value="' + escAttr(cmd) + '" placeholder="e.g. focus_left"></td>' +
    '<td><button class="row-delete" onclick="this.closest(\'tr\').remove();autoSave(0)">' + deleteIcon + '</button></td>';
  tbody.appendChild(tr);
  wrapAllInputs(tr);
  tr.querySelectorAll('input').forEach(function(el) { el.addEventListener('input', function() { autoSave(500); }); });
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
      gap: num('layout-gap'), outer_gap: num('layout-outer_gap'),
      default_column_width: num('layout-default_column_width'),
      min_column_width: num('layout-min_column_width'),
      max_column_width: num('layout-max_column_width'),
      centering_mode: cbVal('cb-layout-centering_mode')
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
      log_level: cbVal('cb-behavior-log_level')
    },
    hotkeys: readHotkeys(),
    window_rules: readRules(),
    gestures: {
      enabled: checked('gestures-enabled'),
      swipe_left: val('gestures-swipe_left'), swipe_right: val('gestures-swipe_right'),
      swipe_up: val('gestures-swipe_up'), swipe_down: val('gestures-swipe_down')
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
    var k = tr.querySelector('.hk-key').value.trim();
    var c = tr.querySelector('.hk-cmd').value.trim();
    if (k && c) b[k] = c;
  });
  return b;
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
