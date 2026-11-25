const assert = require('node:assert/strict');
const vm = require('node:vm');
const { page, english, chinese, hotkeyCatalog, hotkeyCategories, languages: bundledLanguages } = JSON.parse(require('node:fs').readFileSync(0, 'utf8'));
const script = page.split('<script>')[1].split('</script>')[0];
const languagePopup = {};
const initialLanguageCombo = { querySelector: () => languagePopup };
const document = {
  documentElement: { lang: 'en' },
  querySelectorAll: () => [],
  getElementById: id => id === 'cb-appearance-language' ? initialLanguageCombo : null,
  addEventListener: () => {}
};
function runSettings(source, ctx) {
  const original = ctx.document.getElementById;
  const report = {};
  ctx.document.getElementById = id => id.startsWith('report-')
    ? (report[id] ||= { value: '', addEventListener() {} }) : original.call(ctx.document, id);
  ctx.window.addEventListener ||= () => {};
  vm.runInContext(source, ctx);
  ctx.document.getElementById = original;
}
const context = vm.createContext({ document, window: { _localeStrings: english, _language: 'en', _languages: bundledLanguages } });
runSettings(script, context);
const readPresetEntries = context.readPresetEntries;
const languageMarkup = languagePopup.innerHTML;
assert.equal((languageMarkup.match(/class="combobox-option"/g) || []).length, bundledLanguages.length);
for (const language of bundledLanguages) {
  assert.ok(languageMarkup.includes(`data-value="${context.escAttr(language.identifier)}">${context.escHtml(language.name)}</div>`), language.identifier);
}
const hostileLanguage = { identifier: 'x-"<&', name: '<b>&"Native</b>' };
assert.equal(context.languageOptionsMarkup([hostileLanguage]),
  '<div class="combobox-option" data-value="x-&quot;&lt;&amp;">&lt;b&gt;&amp;&quot;Native&lt;/b&gt;</div>');

const hostile = '<b>&"中文</b>';
context.localeStrings = {
  'settings.text.preset_index_percent': hostile + ' {index} ({percent}%)'
};
const row = {};
const trigger = {};
const cb = {
  dataset: {},
  classList: { remove: () => {}, add: () => {} },
  querySelector: () => trigger,
  querySelectorAll: () => []
};
document.getElementById = () => cb;
context.readPresetEntries = () => [{ row, value: 0.5 }];
context.selectedWidthPresetRow = row;
context.initCombobox = () => {};
context.refreshDefaultWidthPresetOptions();
assert.ok(cb.innerHTML.includes('&lt;b&gt;&amp;&quot;中文&lt;/b&gt; 1 (50%)'));
assert.ok(!cb.innerHTML.includes(hostile));
assert.equal(cb.dataset.value, '1');

const text = { dataset: { i18n: 'settings.text.settings' }, textContent: '' };
Object.defineProperty(text, 'innerHTML', { set: () => { throw new Error('Raw HTML translation'); } });
const attribute = {
  attributes: {},
  getAttribute: () => 'settings.text.settings',
  setAttribute(name, value) { this.attributes[name] = value; }
};
document.querySelectorAll = selector => selector === '[data-i18n]' ? [text]
  : selector.startsWith('[data-i18n-') ? [attribute] : [];
context.applyLocale({ 'settings.text.settings': hostile }, 'zh-CN');
assert.equal(text.textContent, hostile);
assert.equal(attribute.attributes.title, hostile);
assert.equal(attribute.attributes['aria-label'], hostile);
assert.equal(attribute.attributes.placeholder, hostile);
assert.equal(document.documentElement.lang, 'zh-CN');

const draft = { value: 'unfinished preset' };
const deleteButton = {};
row.isConnected = true;
row.querySelector = selector => selector === '.preset-val' ? draft : deleteButton;
const languageText = {};
const languages = [['en', 'English'], ['zh-CN', '简体中文']].map(([value, textContent]) => ({
  dataset: { value }, textContent, classList: { add: () => {}, remove: () => {}, contains: () => false }, setAttribute() {}
}));
const languageCombo = {
  dataset: { value: 'en' },
  querySelector: () => languageText,
  querySelectorAll: () => languages
};
document.querySelector = () => null;
document.querySelectorAll = selector => selector === '[data-i18n]' ? [text]
  : selector.startsWith('[data-i18n-') ? [attribute]
  : selector === '#width-presets-body tr' ? [row]
  : selector === '.combobox' ? [languageCombo] : [];
document.getElementById = id => id === 'cb-layout-default_width_preset' ? cb
  : id === 'cb-appearance-language' ? languageCombo : null;
context.readPresetEntries = readPresetEntries;
context.refreshLocale(english, 'en');
assert.equal(text.textContent, 'Settings');
context.refreshLocale(chinese, 'zh-CN');
assert.equal(text.textContent, '设置');
assert.equal(languageText.textContent, '简体中文');
assert.equal(languageCombo.dataset.value, 'zh-CN');
assert.equal(draft.value, 'unfinished preset');
assert.equal(context.selectedWidthPresetRow, row);
context.refreshLocale(english, 'en');
assert.equal(text.textContent, 'Settings');
assert.equal(languageText.textContent, 'English');
assert.equal(draft.value, 'unfinished preset');
const windowsVersion = { textContent: 'Windows 11 (24H2, Build 26100)' };
const previousGetElement = document.getElementById;
document.getElementById = id => id === 'report-windows' ? windowsVersion : previousGetElement(id);
context.window._windowsVersion = 'Windows 11 (24H2, Build 26100)';
context.refreshLocale(chinese, 'zh-CN');
assert.equal(windowsVersion.textContent, 'Windows 11 (24H2, 内部版本 26100)');
context.window._windowsVersion = 'Unknown';
context.refreshLocale(english, 'en');
assert.equal(windowsVersion.textContent, 'Unknown');
context.refreshLocale(chinese, 'zh-CN');
assert.equal(windowsVersion.textContent, '未知');
document.getElementById = previousGetElement;
const warnings = Object.fromEntries(['hotkey-warn-bar', 'hotkey-warn-title', 'hotkey-warn-msg']
  .map(id => [id, {}]));
document.getElementById = id => warnings[id];
context.localeStrings = english;
for (const [keys, title, combos, verb] of [
  [['Win+L'], '1 hotkey is likely unsupported.', 'Win+L', 'is'],
  [['Win+L', 'Ctrl+Alt+Del'], '2 hotkeys are likely unsupported.', 'Win+L and Ctrl+Alt+Del', 'are'],
  [['A', 'B', 'C'], '3 hotkeys are likely unsupported.', 'A, B, and C', 'are']
]) {
  context.window._failedHotkeys = keys;
  context.renderFailedHotkeys();
  assert.equal(warnings['hotkey-warn-title'].textContent, title);
  assert.equal(warnings['hotkey-warn-msg'].textContent,
    `${combos} ${verb} reserved by Windows and can't be intercepted, so it likely won't fire. Pick a different combination.`);
  assert.equal(warnings['hotkey-warn-bar'].hidden, false);
}
context.localeStrings = chinese;
context.window._failedHotkeys = ['Win+L'];
context.renderFailedHotkeys();
assert.equal(warnings['hotkey-warn-title'].textContent, '1 个快捷键可能不受支持。');
assert.ok(warnings['hotkey-warn-msg'].textContent.startsWith('Win+L 已被 Windows 保留'));
context.window._failedHotkeys = [];
context.renderFailedHotkeys();
assert.equal(warnings['hotkey-warn-bar'].hidden, true);
context.localeStrings = { test: '{name} {count} {unknown}' };
assert.equal(context.t('test', { name: '{count}', count: 2 }), '{count} 2 {unknown}');

function attributeKeys(markup) {
  return Array.from(markup.matchAll(/\bdata-i18n(?:-[a-z-]+)?="([^"]+)"/g), match => match[1]);
}
function assertKnownKeys(markup, catalog = english) {
  for (const key of attributeKeys(markup)) {
    assert.ok(Object.hasOwn(catalog, key), `Unknown generated locale key: ${key}`);
  }
}
assertKnownKeys(page.split('<script>')[0]);

function ruleFixture(source, rule = {}) {
  const fields = {};
  const shell = { addEventListener() {}, querySelectorAll: () => [], classList: { contains: () => false } };
  const row = {
    querySelectorAll: () => [],
    querySelector: selector => fields[selector] || (['.rule-opts-btn', '.rule-opts-pop', '.rule-maximized', '.rule-sticky'].includes(selector)
      ? shell : null)
  };
  const doc = {
    documentElement: { lang: 'en' }, addEventListener() {}, querySelectorAll: () => [],
    getElementById: () => null
  };
  const ctx = vm.createContext({ document: doc, window: { _localeStrings: english } });
  runSettings(source, ctx);
  doc.createElement = () => row;
  doc.getElementById = () => ({ appendChild() {} });
  ctx.addRuleRow(rule);
  for (const match of row.innerHTML.matchAll(/<input\b[^>]*class="([^"]+)"[^>]*value="([^"]*)"/g)) {
    fields['.' + match[1].split(' ')[0]] = { value: match[2] };
  }
  fields['.rule-action'] = { dataset: { value: rule.action || 'tile' } };
  fields['.rule-corner'] = { dataset: { value: 'auto' } };
  for (const name of ['rule-border', 'rule-border-color-mode']) {
    const value = row.innerHTML.match(new RegExp(`class="menu-item has-sub ${name}" data-value="([^"]*)"`))[1];
    fields['.' + name] = { dataset: { value } };
  }
  fields['.rule-workspace'] = { dataset: { value: '' } };
  fields['.rule-opts-summary'] = {};
  doc.querySelectorAll = selector => selector === '#rules-body tr' ? [row] : [];
  return { ctx, row, fields };
}
const rules = ruleFixture(script).row.innerHTML;
assertKnownKeys(rules);
const ruleKeys = new Set(attributeKeys(rules));
assert.ok(ruleKeys.size > 0);
for (const key of ruleKeys) {
  const incomplete = { ...english };
  delete incomplete[key];
  assert.throws(() => assertKnownKeys(rules, incomplete), error =>
    error.message.includes(`Unknown generated locale key: ${key}`));
}

const borderFixture = ruleFixture(script, { match_class: 'App', border: false, border_color: 'A1B2C3', future_key: 'kept' });
assert.deepEqual(JSON.parse(JSON.stringify(borderFixture.ctx.readRules())), [{ match_class: 'App', action: 'tile', border: false, border_color: 'A1B2C3', future_key: 'kept' }]);
borderFixture.fields['.rule-border'].dataset.value = 'show';
borderFixture.fields['.rule-border-color-mode'].dataset.value = 'default';
assert.deepEqual(JSON.parse(JSON.stringify(borderFixture.ctx.readRules())), [{ match_class: 'App', action: 'tile', future_key: 'kept' }]);

const sizeRule = { match_executable: 'QQ.exe', action: 'float', match_max_width: 400, match_max_height: 300, width: 900, height: 700, x: -25, y: 0 };
const sizeFixture = ruleFixture(script, sizeRule);
const savedRules = () => JSON.parse(JSON.stringify(sizeFixture.ctx.readRules()));
assert.deepEqual(savedRules(), [sizeRule]);
sizeFixture.ctx.updateRuleSummary(sizeFixture.row);
assert.equal(sizeFixture.fields['.rule-opts-summary'].textContent, 'Max width 400 · Max height 300');
sizeFixture.ctx.localeStrings = chinese;
sizeFixture.ctx.updateRuleSummary(sizeFixture.row);
assert.equal(sizeFixture.fields['.rule-opts-summary'].textContent, '最大宽度 400 · 最大高度 300');
for (const [width, height, expected] of [
  ['500', '350', { match_max_width: 500, match_max_height: 350 }],
  ['', '', {}],
  ['0', '-1', {}],
  ['1.5', 'invalid', {}],
  ['1', '', { match_max_width: 1 }],
  ['', '1', { match_max_height: 1 }]
]) {
  sizeFixture.fields['.rule-max-width'].value = width;
  sizeFixture.fields['.rule-max-height'].value = height;
  assert.deepEqual(savedRules(), [{ match_executable: 'QQ.exe', action: 'float', width: 900, height: 700, x: -25, y: 0, ...expected }]);
}
assert.equal(sizeRule.match_max_width, 400);
assert.equal(sizeRule.match_max_height, 300);
sizeFixture.fields['.rule-exe'].value = '';
assert.deepEqual(savedRules(), []);

function eventElement() {
  const listeners = {};
  const classes = new Set();
  return {
    value: '', checked: false, dataset: {}, style: {}, textContent: '',
    classList: {
      add: name => classes.add(name), remove: name => classes.delete(name),
      contains: name => classes.has(name)
    },
    setAttribute() {}, getAttribute() { return null; }, removeAttribute() {}, closest() { return null; },
    addEventListener(name, callback) { (listeners[name] ||= []).push(callback); },
    emit(name, event = {}) {
      for (const callback of listeners[name] || []) callback({ target: this, stopPropagation() {}, ...event });
      if (name === 'click' && this.parent) this.parent.emit(name, { target: this });
    }
  };
}
function presetFixture(values = [0.333, 0.5, 0.667]) {
  const rows = [];
  const fields = {};
  const timers = new Map();
  const saves = [];
  let nextTimer = 0;
  const doc = {
    documentElement: { lang: 'en' }, addEventListener() {}, querySelectorAll: () => [],
    getElementById: () => null, querySelector: () => null
  };
  const ctx = vm.createContext({
    document: doc, window: { _localeStrings: english, ipc: { postMessage: json => saves.push(JSON.parse(json)) } },
    setTimeout(callback, delay) { timers.set(++nextTimer, { callback, delay }); return nextTimer; },
    clearTimeout(id) { timers.delete(id); }
  });
  runSettings(script, ctx);
  const trigger = eventElement(); trigger.id = 'preset-trigger';
  const triggerText = eventElement();
  const popup = eventElement();
  const combo = eventElement();
  let options = [];
  popup.contains = option => options.includes(option);
  Object.defineProperty(combo, 'innerHTML', {
    set(markup) {
      triggerText.textContent = markup.match(/class="combobox-text">([^<]*)/)[1];
      options = Array.from(markup.matchAll(/class="combobox-option([^"]*)" data-value="([^"]+)">([^<]*)/g), match => {
        const option = eventElement();
        if (match[1].includes('selected')) option.classList.add('selected');
        option.dataset.value = match[2]; option.textContent = match[3];
        option.parent = popup; option.closest = () => option;
        return option;
      });
    }
  });
  combo.querySelectorAll = () => options;
  combo.querySelector = selector => selector === '.combobox-trigger' ? trigger
    : selector === '.combobox-text' ? triggerText : popup;
  const language = eventElement();
  const languageLabel = eventElement();
  const languageOptions = [['en', 'English'], ['zh-CN', '简体中文']].map(([value, textContent]) => {
    const option = eventElement(); option.dataset.value = value; option.textContent = textContent; return option;
  });
  language.querySelectorAll = () => languageOptions;
  language.querySelector = () => languageLabel;
  fields['cb-layout-default_width_preset'] = combo;
  fields['cb-appearance-language'] = language;
  fields['width-presets-body'] = { appendChild: row => rows.push(row) };
  doc.getElementById = id => fields[id] ||= eventElement();
  doc.querySelectorAll = selector => selector === '#width-presets-body tr' ? rows
    : selector === '.combobox' ? [combo, language] : [];
  doc.createElement = () => {
    const input = eventElement();
    const button = eventElement();
    const row = {
      isConnected: true, input, querySelectorAll: () => [],
      querySelector: selector => selector === '.row-delete' ? button : input
    };
    Object.defineProperty(row, 'innerHTML', { set(markup) { input.value = markup.match(/value="([^"]*)"/)[1]; } });
    return row;
  };
  values.forEach(value => ctx.addPresetRow('width', value));
  ctx.refreshDefaultWidthPresetOptions(1);
  return { ctx, rows, combo, options: () => options, timers, saves,
    flush() {
      const pending = Array.from(timers.values()); timers.clear(); pending.forEach(timer => timer.callback());
      const save = saves.at(-1);
      if (save?.action === 'save') ctx.window.lwmSaveResult(save.id, true);
    } };
}
function presetState(fixture) {
  const { ctx, combo } = fixture;
  return {
    selected: ctx.selectedWidthPresetRow,
    value: combo.dataset.value,
    widths: Array.from(ctx.lastValidWidthPresets),
    defaultPreset: ctx.lastValidDefaultWidthPreset
  };
}
const pending = presetFixture();
pending.options()[1].emit('click');
assert.equal(pending.timers.size, 1);
assert.equal(Array.from(pending.timers.values())[0].delay, 0);
assert.equal(pending.ctx.selectedWidthPresetRow, pending.rows[1]);
const selectedBeforePush = presetState(pending);
pending.ctx.refreshLocale(chinese, 'zh-CN');
assert.deepEqual(presetState(pending), selectedBeforePush);
pending.flush();
assert.equal(pending.saves[0].config.layout.default_width_preset, 2);
assert.deepEqual(pending.saves[0].config.layout.width_presets, [0.333, 0.5, 0.667]);

for (const [draft, expectedWidths, expectedDefault] of [
  ['0.6', [0.333, 0.6, 0.667], 2],
  ['unfinished', [0.333, 0.667], 1],
  ['', [0.333, 0.667], 1]
]) {
  const fixture = presetFixture();
  fixture.options()[1].emit('click');
  fixture.flush();
  fixture.rows[1].input.value = draft;
  fixture.rows[1].input.emit('input');
  assert.equal(fixture.timers.size, 1);
  assert.equal(Array.from(fixture.timers.values())[0].delay, 500);
  const beforePush = presetState(fixture);
  fixture.ctx.refreshLocale(chinese, 'zh-CN');
  assert.deepEqual(presetState(fixture), beforePush);
  assert.equal(fixture.rows[1].input.value, draft);
  fixture.flush();
  assert.equal(fixture.saves[1].config.layout.default_width_preset, expectedDefault);
  assert.deepEqual(fixture.saves[1].config.layout.width_presets, expectedWidths);
}
const invalid = presetFixture([0.5]);
invalid.rows[0].input.value = 'unfinished';
invalid.rows[0].input.emit('input');
assert.equal(Array.from(invalid.timers.values())[0].delay, 500);
const fallbackBeforePush = presetState(invalid);
invalid.ctx.refreshLocale(chinese, 'zh-CN');
assert.deepEqual(presetState(invalid), fallbackBeforePush);
assert.equal(invalid.rows[0].input.value, 'unfinished');
invalid.flush();
assert.deepEqual(invalid.saves[0].config.layout.width_presets, [0.5]);
assert.equal(invalid.saves[0].config.layout.default_width_preset, 1);

const reloadedPresets = presetFixture([0.5]);
const presetDoc = reloadedPresets.ctx.document;
const widthBody = presetDoc.getElementById('width-presets-body');
Object.defineProperty(widthBody, 'innerHTML', { set() { reloadedPresets.rows.length = 0; } });
presetDoc.getElementById('height-presets-body').appendChild = () => {};
presetDoc.getElementById('workspace-names-card').hasChildNodes = () => true;
reloadedPresets.ctx.setCb = () => {};
reloadedPresets.ctx.renderFailedHotkeys = () => {};
reloadedPresets.ctx.updateScrollLabels = () => {};
reloadedPresets.ctx.applyReloadConfig({
  layout: { width_presets: [1.25, 1.5], height_presets: [], default_width_preset: 2 },
  appearance: {}, behavior: {}, gestures: {}, snap_hints: {}
});
assert.deepEqual(Array.from(reloadedPresets.ctx.readConfig().layout.width_presets), [1.25, 1.5]);
assert.equal(reloadedPresets.ctx.readConfig().layout.default_width_preset, 2);

function commandFixture({ saving = false } = {}) {
  const rows = [];
  const combos = [];
  const fields = {};
  const saves = [];
  let saveCallback;
  let scheduledSaves = 0;
  const doc = {
    documentElement: { lang: 'en' }, addEventListener() {}, querySelectorAll: () => [],
    getElementById: () => null, querySelector: () => null
  };
  const ctx = vm.createContext({
    document: doc, window: { _localeStrings: english, _hotkeyCatalog: hotkeyCatalog, _hotkeyCategories: hotkeyCategories,
      ipc: { postMessage(json) {
        if (!saving) throw new Error('Language refresh sent IPC');
        saves.push(JSON.parse(json));
      } } },
    setTimeout(callback, delay) {
      if (!saving) throw new Error('Language refresh scheduled a save');
      if (delay === 2000) return -1;
      saveCallback = callback;
      scheduledSaves++;
      return 1;
    }, clearTimeout(id) { if (id !== -1) saveCallback = undefined; }
  });
  runSettings(script, ctx);
  const decode = text => text.replace(/&quot;/g, '"').replace(/&gt;/g, '>').replace(/&lt;/g, '<').replace(/&amp;/g, '&');
  doc.createElement = () => {
    const label = eventElement();
    const attributes = new Map();
    label.setAttribute = (name, value) => attributes.set(name, String(value));
    label.getAttribute = name => attributes.get(name) ?? null;
    label.removeAttribute = name => attributes.delete(name);
    Object.defineProperty(label, 'title', {
      get: () => label.getAttribute('title') || '',
      set: value => label.setAttribute('title', value)
    });
    const input = eventElement();
    const note = eventElement();
    const button = eventElement();
    button.setAttribute = (name, value) => button[name] = value;
    const row = { dataset: {}, label, input, note, button, querySelectorAll: () => [],
      querySelector: selector => ({ '.hk-cmd-label': label, '.hk-key': input, '.hk-dup-note': note, button })[selector] };
    Object.defineProperty(row, 'innerHTML', { set(markup) {
      if (row.className === 'hk-group') {
        button['aria-expanded'] = markup.match(/aria-expanded="([^"]*)"/)[1];
        const heading = markup.match(/<span data-i18n="([^"]*)">([^<]*)<\/span>/);
        label.dataset.i18n = heading[1]; label.textContent = decode(heading[2]);
        return;
      }
      const match = markup.match(/<td class="hk-cmd-label"(?: title="([^"]*)")?[^>]*>([^<]*)<\/td>/);
      if (match[1] === undefined) label.removeAttribute('title');
      else label.title = decode(match[1]);
      label.textContent = decode(match[2]);
      input.value = decode(markup.match(/class="hk-key(?: hk-record)?" value="([^"]*)"/)[1]);
    } });
    Object.defineProperty(row, 'nextSibling', { get: () => rows[rows.indexOf(row) + 1] || null });
    return row;
  };
  const tbody = {
    appendChild: row => rows.push(row),
    insertBefore: (row, anchor) => rows.splice(rows.indexOf(anchor), 0, row),
    querySelectorAll(selector) {
      if (selector === 'tr[data-category]') return rows.filter(row => row.dataset.category);
      const match = /^tr\[data-cmd="([^"]+)"\]$/.exec(selector);
      assert.ok(match, `Unsupported hotkey table selector: ${selector}`);
      return rows.filter(row => row.dataset.cmd === match[1]);
    }
  };
  Object.defineProperty(tbody, 'innerHTML', { set() { rows.length = 0; } });
  fields['hotkeys-body'] = tbody;
  fields['glazewm_compat-enabled'] = eventElement();
  const presetCombo = eventElement();
  presetCombo.querySelector = () => eventElement();
  presetCombo.querySelectorAll = () => [];
  fields['cb-layout-default_width_preset'] = presetCombo;
  doc.getElementById = id => fields[id] ||
    (saving && /^(layout|appearance|behavior|gestures|snaphints|animation|workspace-name)-/.test(id) ? eventElement() : null);
  doc.querySelector = selector => selector === '#hotkeys-body tr[data-special="scroll_modifier"]'
    ? rows.find(row => row.dataset.special === 'scroll_modifier') : null;
  doc.querySelectorAll = selector => selector === '#hotkeys-body tr[data-cmd]' ? rows.filter(row => row.dataset.cmd)
    : selector === '[data-i18n]' ? rows.filter(row => row.className === 'hk-group').map(row => row.label)
    : selector === '#hotkeys-body tr' ? rows
    : selector === '.combobox' || selector === '[id^="cb-gestures-"]' ? combos : [];
  function addCombo(id, value) {
    const cb = eventElement(); cb.dataset.value = value;
    const trigger = eventElement(); trigger.id = id + '-trigger';
    const text = eventElement();
    const popup = eventElement();
    let options = [];
    Object.defineProperty(cb, 'innerHTML', { set(markup) {
      text.textContent = decode(markup.match(/class="combobox-text">([^<]*)/)[1]);
      options = Array.from(markup.matchAll(/class="combobox-option([^"]*)" data-value="([^"]*)"[^>]*>([^<]*)/g), match => {
        const option = eventElement(); option.dataset.value = match[2]; option.textContent = decode(match[3]);
        if (match[1].includes('selected')) option.classList.add('selected');
        return option;
      });
    } });
    cb.querySelector = selector => selector === '.combobox-trigger' ? trigger
      : selector === '.combobox-text' ? text : popup;
    cb.querySelectorAll = () => options;
    fields[id] = cb; combos.push(cb); ctx.initGestureCombo(id);
    return { cb, text, options };
  }
  return { ctx, rows, combos, fields, addCombo, saves, scheduledSaves: () => scheduledSaves, flush() {
    saveCallback();
    const save = saves.at(-1);
    if (save?.action === 'save') ctx.window.lwmSaveResult(save.id, true);
  } };
}

const scrollSettings = commandFixture({ saving: true });
scrollSettings.fields['lbl-scroll-up'] = eventElement();
scrollSettings.fields['lbl-scroll-down'] = eventElement();
scrollSettings.ctx.loadHotkeysSorted({}, ' NONE ');
let scrollRow = scrollSettings.rows.find(row => row.dataset.special === 'scroll_modifier');
assert.equal(scrollRow.input.value, '');
scrollSettings.ctx.updateScrollLabels();
assert.equal(scrollSettings.fields['lbl-scroll-up'].textContent, english['settings.text.scroll_up_modifier_off']);
assert.equal(scrollSettings.fields['lbl-scroll-down'].textContent, english['settings.text.scroll_down_modifier_off']);
scrollSettings.ctx.refreshLocale(chinese, 'zh-CN');
assert.equal(scrollSettings.fields['lbl-scroll-up'].textContent, chinese['settings.text.scroll_up_modifier_off']);
scrollSettings.ctx.refreshLocale(english, 'en');
scrollRow.input.value = 'Ctrl+Alt';
scrollRow.input.emit('input');
assert.equal(scrollSettings.fields['lbl-scroll-up'].textContent, 'Ctrl + Alt + Scroll up');
scrollRow.input.value = '';
scrollRow.input.emit('input');
scrollSettings.flush();
assert.equal(scrollSettings.saves.at(-1).config.hotkeys.scroll_modifier, 'none');
scrollRow.input.value = ' NoNe ';
scrollRow.input.emit('input');
scrollSettings.flush();
assert.equal(scrollSettings.saves.at(-1).config.hotkeys.scroll_modifier, 'none');
assert.equal(scrollSettings.fields['lbl-scroll-up'].textContent, english['settings.text.scroll_up_modifier_off']);
scrollSettings.ctx.resetHotkeys();
scrollSettings.flush();
scrollRow = scrollSettings.rows.find(row => row.dataset.special === 'scroll_modifier');
assert.equal(scrollRow.input.value, 'Ctrl+Alt');
assert.equal(scrollSettings.saves.at(-1).config.hotkeys.scroll_modifier, 'Ctrl+Alt');

const dragSettings = commandFixture({ saving: true });
dragSettings.addCombo('cb-behavior-plain_drag_action', 'merge');
const dragDocument = dragSettings.ctx.document;
const originalGetElement = dragDocument.getElementById;
const initFields = {};
dragDocument.getElementById = id => originalGetElement(id) ||
  (id.startsWith('cb-') || id === 'workspace-names-card' || id === 'workspace-rows-body' ? null :
    (initFields[id] ||= eventElement()));
dragSettings.ctx.addPresetRow = () => {};
dragSettings.ctx.refreshDefaultWidthPresetOptions = () => {};
dragSettings.ctx.renderFailedHotkeys = () => {};
dragSettings.ctx.updateScrollLabels = () => {};
dragSettings.ctx.init({ layout: { width_presets: [], height_presets: [] }, appearance: {},
  behavior: { plain_drag_action: 'reorder' }, gestures: {}, snap_hints: {} });
dragSettings.ctx.autoSave(0);
dragSettings.flush();
assert.equal(dragSettings.saves.at(-1).config.behavior.plain_drag_action, 'reorder');

const shortcuts = commandFixture({ saving: true });
shortcuts.ctx.configSnapshot.glazewm_compat = { port: 17000 };
shortcuts.ctx.document.getElementById('glazewm_compat-enabled').checked = true;
const configuredShortcuts = {
  'Ctrl+Alt+N': 'focus_next', 'Ctrl+Alt+B': 'focus_next',
  'Ctrl+Alt+F21': 'focus_prev', 'Ctrl+Alt+F22': 'focus_prev',
  'Ctrl+Alt+H': 'focus_left', 'Ctrl+Alt+F23': 'focus_left',
  'Ctrl+Alt+F24': 'custom_command', 'Ctrl+Alt+G': 'custom_command'
};
shortcuts.ctx.loadHotkeysSorted(configuredShortcuts, 'Ctrl+Shift');
function savedShortcuts() {
  shortcuts.ctx.autoSave(0);
  shortcuts.flush();
  return shortcuts.saves.at(-1).config.hotkeys;
}
let saved = savedShortcuts();
assert.deepEqual(shortcuts.saves.at(-1).config.glazewm_compat, { enabled: true, port: 17000 });
for (const [key, command] of Object.entries(configuredShortcuts)) {
  assert.equal(saved[key], command, `Settings save lost ${key} for ${command}`);
}
assert.equal(saved.scroll_modifier, 'Ctrl+Shift');
const rowsFor = command => shortcuts.rows.filter(row => row.dataset.cmd === command);
const leftRows = rowsFor('focus_left');
assert.equal(leftRows.length, 2);
leftRows[1].input.value = '';
saved = savedShortcuts();
assert.equal(saved['Ctrl+Alt+H'], 'focus_left');
assert.ok(!Object.hasOwn(saved, 'Ctrl+Alt+F23'));
assert.ok(!saved.disabled.includes('focus_left'));
leftRows[0].input.value = '';
saved = savedShortcuts();
assert.equal(saved.disabled.filter(command => command === 'focus_left').length, 1);
shortcuts.ctx.resetHotkeyRow(leftRows[0]);
shortcuts.flush();
assert.equal(shortcuts.saves.at(-1).config.hotkeys['Ctrl+Alt+H'], 'focus_left');
assert.ok(!shortcuts.saves.at(-1).config.hotkeys.disabled.includes('focus_left'));
leftRows[1].input.value = 'Ctrl+Alt+F23';
shortcuts.ctx.resetHotkeyRow(leftRows[1]);
shortcuts.flush();
assert.equal(leftRows[1].input.value, '');
assert.equal(leftRows[0].input.value, 'Ctrl+Alt+H');
const nextRows = rowsFor('focus_next');
shortcuts.ctx.resetHotkeyRow(nextRows[1]);
shortcuts.flush();
assert.equal(nextRows[1].input.value, '');
assert.equal(nextRows[0].input.value, 'Ctrl+Alt+N');
nextRows[1].input.value = 'Ctrl+Alt+H';
shortcuts.ctx.refreshLocale(chinese, 'zh-CN');
assert.ok(nextRows.every(row => row.label.textContent === '焦点移到下一个窗口'));
assert.ok(leftRows[0].note.textContent.includes('焦点移到下一个窗口'));
assert.ok(nextRows[1].note.textContent.includes(leftRows[0].label.textContent));
const previousRows = rowsFor('focus_prev');
assert.equal(previousRows.length, 2);
assert.equal(shortcuts.rows[shortcuts.rows.indexOf(previousRows[0]) + 1], previousRows[1]);
const headers = shortcuts.rows.filter(row => row.className === 'hk-group');
assert.deepEqual(headers.map(row => row.dataset.category), [...hotkeyCategories.map(category => category.id), 'other']);
assert.equal(headers.find(row => row.dataset.category === 'focus').label.textContent, '焦点');
assert.equal(headers.at(-1).label.textContent, '其他');
let currentCategory;
for (const row of shortcuts.rows) {
  if (row.className === 'hk-group') currentCategory = row.dataset.category;
  else assert.equal(row.dataset.category, currentCategory);
}
for (const category of hotkeyCategories) {
  const header = headers.find(row => row.dataset.category === category.id);
  const categoryRows = shortcuts.rows.filter(row => row.dataset.category === category.id && row !== header);
  assert.ok(categoryRows.length);
  assert.ok(categoryRows.every(row => row.hidden === category.collapsed));
  assert.equal(header.button['aria-expanded'], String(!category.collapsed));
}
const leopardHeader = headers.find(row => row.dataset.category === 'leopardwm');
assert.equal(shortcuts.rows[shortcuts.rows.indexOf(leopardHeader) + 1].dataset.special, 'scroll_modifier');
const monitorHeader = headers.find(row => row.dataset.category === 'monitor');
const monitorRow = rowsFor('focus_monitor_left')[0];
assert.ok(monitorRow.hidden);
assert.equal(savedShortcuts()['Ctrl+Alt+Win+,'], 'focus_monitor_left');
monitorRow.input.value = 'Ctrl+Alt+H';
shortcuts.ctx.refreshDuplicateWarnings();
assert.ok(monitorRow.note.textContent.includes(leftRows[0].label.textContent));
assert.ok(leftRows[0].note.textContent.includes(monitorRow.label.textContent));
monitorRow.input.value = '';
assert.ok(savedShortcuts().disabled.includes('focus_monitor_left'));
monitorHeader.button.emit('click');
assert.equal(monitorRow.hidden, false);
assert.equal(monitorHeader.button['aria-expanded'], 'true');
shortcuts.ctx.loadHotkeysSorted(shortcuts.ctx.DEFAULT_HOTKEYS, 'Ctrl+Alt');
assert.equal(rowsFor('focus_monitor_left')[0].hidden, false);
assert.ok(!shortcuts.rows.some(row => row.className === 'hk-group' && row.dataset.category === 'other'));
const newMonitorHeader = shortcuts.rows.find(row => row.className === 'hk-group' && row.dataset.category === 'monitor');
newMonitorHeader.button.emit('click');
assert.equal(rowsFor('focus_monitor_left')[0].hidden, true);
saved = savedShortcuts();
assert.deepEqual(saved, { ...shortcuts.ctx.DEFAULT_HOTKEYS, scroll_modifier: 'Ctrl+Alt', disabled: [] });
shortcuts.ctx.loadHotkeysSorted({}, 'Ctrl+Alt', ['focus_left']);
assert.equal(rowsFor('focus_left')[0].input.value, '');
assert.ok(savedShortcuts().disabled.includes('focus_left'));

const recorder = commandFixture({ saving: true });
recorder.ctx.loadHotkeysSorted({ 'Ctrl+Alt+Left': 'focus_left' }, 'Ctrl+Alt');
const [leftRow] = recorder.rows.filter(row => row.dataset.cmd === 'focus_left');
const [rightRow] = recorder.rows.filter(row => row.dataset.cmd === 'focus_right');
const boundValues = () => recorder.rows.map(row => row.input.value);
assert.equal(leftRow.input.value, 'Ctrl+Alt+Left');
function assertChordIgnored(chord) {
  const values = boundValues();
  const sent = recorder.saves.length;
  const scheduled = recorder.scheduledSaves();
  recorder.ctx.onRecordedChord(chord);
  assert.deepEqual(boundValues(), values);
  assert.equal(recorder.saves.length, sent);
  assert.equal(recorder.scheduledSaves(), scheduled);
}
assertChordIgnored('Ctrl+Alt+H');
rightRow.input.emit('mousedown', { button: 0 });
rightRow.input.emit('blur');
assert.ok(!rightRow.input.classList.contains('recording'));
recorder.saves.length = 0;
assertChordIgnored('Ctrl+Alt+H');
leftRow.input.emit('mousedown', { button: 0 });
assert.ok(leftRow.input.classList.contains('recording'));
assert.deepEqual(recorder.saves, [{ action: 'set_recording', recording: true }]);
recorder.ctx.onRecordedChord('Ctrl+Alt+H');
assert.equal(leftRow.input.value, 'Ctrl+Alt+H');
assert.ok(!leftRow.input.classList.contains('recording'));
assert.deepEqual(recorder.saves, [
  { action: 'set_recording', recording: true },
  { action: 'set_recording', recording: false }
]);
assert.equal(recorder.scheduledSaves(), 1);
recorder.flush();
const recordedBindings = recorder.saves.at(-1).config.hotkeys;
assert.equal(recorder.saves.at(-1).action, 'save');
assert.equal(recordedBindings['Ctrl+Alt+H'], 'focus_left');
assert.ok(!Object.hasOwn(recordedBindings, 'Ctrl+Alt+Left'));
assertChordIgnored('Ctrl+Alt+X');

for (const [code, keyCode, token, symbol] of [
  ['Quote', 0xBD, 'Minus', '-'], ['Minus', 0xBB, 'Equals', '='],
  ['Equal', 0xBC, 'Comma', ','], ['Comma', 0xBE, 'Period', '.'],
  ['Period', 0xDB, 'Bracket_Left', '['], ['BracketLeft', 0xDD, 'Bracket_Right', ']'],
  ['BracketRight', 0xBA, 'Semicolon', ';'], ['Semicolon', 0xBF, 'Slash', '/'],
  ['Slash', 0xC0, 'Backtick', '`'], ['Backquote', 0xDC, 'Backslash', '\\'],
  ['Backslash', 0xDE, 'Apostrophe', "'"]
]) {
  recorder.ctx.loadHotkeysSorted({ ['Ctrl+' + token]: 'focus_left' }, 'Ctrl+Alt');
  const input = recorder.rows.find(row => row.dataset.cmd === 'focus_left').input;
  const chord = 'Ctrl+' + symbol;
  assert.equal(input.value, chord, token + ' display');
  input.emit('mousedown', { button: 0 });
  input.emit('keydown', { code, keyCode, key: 'non-US symbol', ctrlKey: true, preventDefault() {} });
  assert.equal(input.value, chord, code + ' recording');
  assert.ok(!input.classList.contains('recording'), code);
  recorder.flush();
  assert.equal(recorder.saves.at(-1).config.hotkeys[chord], 'focus_left', code + ' save');
}

const azertyInput = recorder.rows.find(row => row.dataset.cmd === 'focus_left').input;
azertyInput.emit('mousedown', { button: 0 });
azertyInput.emit('keydown', { code: 'KeyM', keyCode: 0xBC, key: ',', ctrlKey: true, preventDefault() {} });
assert.equal(azertyInput.value, 'Ctrl+,', 'AZERTY comma recording');
assert.ok(!azertyInput.classList.contains('recording'));
recorder.flush();
assert.equal(recorder.saves.at(-1).config.hotkeys['Ctrl+,'], 'focus_left', 'AZERTY comma save');

const unsupportedInput = recorder.rows.find(row => row.dataset.cmd === 'focus_left').input;
unsupportedInput.emit('mousedown', { button: 0 });
const savesBeforeUnsupported = recorder.saves.length;
const scheduledBeforeUnsupported = recorder.scheduledSaves();
unsupportedInput.emit('keydown', { code: 'Quote', keyCode: 0xDF, key: 'non-US symbol', ctrlKey: true, altKey: true, preventDefault() {} });
assert.equal(unsupportedInput.value, '');
assert.ok(unsupportedInput.classList.contains('recording'));
assert.equal(recorder.saves.length, savesBeforeUnsupported);
assert.equal(recorder.scheduledSaves(), scheduledBeforeUnsupported);
unsupportedInput.emit('blur');

for (const [full, short] of [
  ['Command (detail)', 'Command'], ['Command(detail)', 'Command'],
  ['命令（详情）', '命令'], ['命令 （详情）', '命令'],
  ['Command（first） (second)', 'Command'], ['Command without detail', 'Command without detail']
]) {
  const fixture = commandFixture();
  fixture.ctx.localeStrings = { 'hotkeys.command.toggle_new_window_placement': full };
  fixture.ctx.addHotkeyRow('', 'toggle_new_window_placement');
  assert.equal(fixture.rows[0].label.textContent, short, full);
  assert.equal(fixture.rows[0].label.getAttribute('title'), full === short ? null : full, full);
}

const commands = commandFixture();
commands.ctx.addHotkeyRow('Ctrl+Alt+H', 'toggle_new_window_placement');
commands.ctx.addHotkeyRow('Ctrl+Alt+H', 'focus_next');
commands.ctx.addHotkeyRow('', 'custom_command');
commands.rows[2].input.value = 'unfinished shortcut';
commands.rows[1].label.title = 'Previous detail';
const commandRows = commands.rows.slice();
const gestureValues = ['toggle_new_window_placement', 'focus_next', '', 'custom_command', 'scroll_left', 'focus_prev'];
const gestureIds = ['swipe_left', 'swipe_right', 'swipe_up', 'swipe_down', 'scroll_up', 'scroll_down'];
const gestureCombos = gestureIds.map((id, index) => commands.addCombo('cb-gestures-' + id, gestureValues[index]));
for (const [strings, language, short, full, next, noAction] of [
  [chinese, 'zh-CN', '切换新窗口放置方式', '切换新窗口放置方式（新建列／堆叠到列）', '焦点移到下一个窗口', '无操作'],
  [english, 'en', 'Toggle new-window placement', 'Toggle new-window placement (new column / in column)', 'Focus next window', 'No action']
]) {
  commands.ctx.refreshLocale(strings, language);
  assert.equal(commands.rows.length, commandRows.length);
  commands.rows.forEach((row, index) => assert.equal(row, commandRows[index]));
  assert.equal(commands.rows[0].label.textContent, short);
  assert.equal(commands.rows[0].label.title, full);
  assert.equal(commands.rows[1].label.textContent, next);
  assert.equal(commands.rows[1].label.getAttribute('title'), null);
  assert.equal(commands.rows[2].label.textContent, 'custom_command');
  assert.deepEqual(commands.rows.map(row => row.input.value), ['Ctrl+Alt+H', 'Ctrl+Alt+H', 'unfinished shortcut']);
  assert.equal(commands.rows[0].note.textContent, strings['settings.text.also_bound_to_commands'].replace('{commands}', next));
  assert.equal(commands.rows[1].note.textContent, strings['settings.text.also_bound_to_commands'].replace('{commands}', short));
  gestureCombos.forEach(({ cb, text, options }, index) => {
    assert.equal(cb.dataset.value, gestureValues[index]);
    assert.equal(text.textContent, gestureValues[index] === '' ? noAction
      : strings['hotkeys.command.' + gestureValues[index]] || gestureValues[index]);
    assert.deepEqual(cb.querySelectorAll('.combobox-option'), options);
    assert.deepEqual(options.filter(option => option.classList.contains('selected')).map(option => option.dataset.value),
      gestureValues[index] === 'custom_command' ? [] : [gestureValues[index]]);
    assert.deepEqual(options.map(option => option.dataset.value), ['', ...hotkeyCatalog.map(action => action.id)]);
    options.forEach(option => assert.equal(option.textContent, option.dataset.value === '' ? noAction
      : strings['hotkeys.command.' + option.dataset.value]));
  });
}
