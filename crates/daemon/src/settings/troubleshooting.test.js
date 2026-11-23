const assert = require('node:assert/strict');
const vm = require('node:vm');
const {page, english, chinese} = JSON.parse(require('node:fs').readFileSync(0, 'utf8'));
const start = page.lastIndexOf('/* Troubleshooting */');
const end = page.indexOf('/* End Troubleshooting */', start);
assert(start >= 0 && end > start);
const elements = {};
for (const id of ['health', 'files', 'refresh', 'reveal', 'create', 'status', 'status-bar', 'status-text', 'report']) {
  const classes = new Set();
  elements['troubleshooting-' + id] = {dataset: {}, hidden: id === 'report' || id === 'status-bar', listeners: {},
    classList: {add(name) { classes.add(name); }, remove(...names) { names.forEach(name => classes.delete(name)); }, contains(name) { return classes.has(name); }},
    addEventListener(type, fn) { this.listeners[type] = fn; }};
}
assert.match(page, /<div id="troubleshooting-status" role="status"><div class="info-bar" id="troubleshooting-status-bar" hidden>/);
const iconPath = {setAttribute(name, value) { this[name] = value; }};
const icon = {setAttribute(name, value) { this[name] = value; }, querySelector() { return iconPath; }};
elements['troubleshooting-status-bar'].querySelector = selector => selector === '.info-bar-icon' ? icon : elements['troubleshooting-status-text'];
const sources = {};
for (const id of ['hc-info-bar', 'hotkey-warn-bar']) {
  const source = page.slice(page.indexOf('id="' + id + '"'));
  const viewBox = source.match(/viewBox="([^"]+)"/)[1], path = source.match(/<path d="([^"]+)"/)[1];
  sources[id] = {getAttribute: () => viewBox, querySelector: () => ({getAttribute: () => path})};
}
const messages = [];
let onLanguageChange;
const document = {documentElement: {lang: 'en'}, getElementById: id => elements[id], querySelector: selector => sources[selector.split(' ')[0].slice(1)] || null};
const window = {ipc: {postMessage: message => messages.push(JSON.parse(message))}};
const context = vm.createContext({window, document, localeStrings: english, MutationObserver: class {
  constructor(callback) { onLanguageChange = callback; }
  observe() {}
}});
const functionText = name => {
  const match = page.match(new RegExp('function ' + name + '\\([^\\n]+'));
  assert(match, name);
  return match[0];
};
vm.runInContext(page.slice(page.indexOf('function t('), page.indexOf('function applyLocale(')) + '\n' + functionText('escHtml'), context);
vm.runInContext(page.slice(page.indexOf('function showInfoBar('), page.indexOf('/* ── Report an Issue ── */')), context);
vm.runInContext(page.slice(start, end), context);
assert.equal(elements['troubleshooting-report'].hidden, true);
assert.match(elements['troubleshooting-health'].innerHTML, /Loading…/);
assert.match(elements['troubleshooting-files'].innerHTML, /Loading…/);
assert.doesNotMatch(elements['troubleshooting-health'].innerHTML, /No problems found/);
assert.doesNotMatch(elements['troubleshooting-files'].innerHTML, /No saved diagnostics/);
context.localeStrings = chinese;
document.documentElement.lang = 'zh-CN';
onLanguageChange();
assert.match(elements['troubleshooting-health'].innerHTML, /正在加载/);
assert.match(elements['troubleshooting-files'].innerHTML, /正在加载/);
context.localeStrings = english;
document.documentElement.lang = 'en';
onLanguageChange();
window.lwmReceiveHealth({rows: [], unknown_keys: null});
window.lwmReceiveDiagnostics({ok: true, files: []});
assert.match(elements['troubleshooting-health'].innerHTML, /No problems found/);
assert.match(elements['troubleshooting-files'].innerHTML, /No saved diagnostics/);
assert.deepEqual(messages.map(message => message.action), ['request_health', 'list_diagnostics']);
const hostile = '<img src=x onerror="bad()">';
window.lwmReceiveHealth({rows: [
  {kind: 'config', path: hostile, reason: hostile, line: 3, column: 7, recovered: [hostile]},
  {kind: 'warning', field: hostile, message: hostile},
  {kind: 'unknown_keys', keys: [hostile]},
  {kind: 'reserved_hotkeys', binds: [hostile]},
  {kind: 'keyboard_hook'}, {kind: 'elevated_window', executable: hostile},
  {kind: 'integrity'}, {kind: 'watchdog_restart'},
  {kind: 'log_failure', path: hostile, error: hostile}, {kind: 'update', tag: hostile}
], unknown_keys: [hostile]});
const markup = elements['troubleshooting-health'].innerHTML;
assert(!markup.includes(hostile));
assert(markup.includes('&lt;img src=x onerror=&quot;bad()&quot;&gt;'));
assert.match(markup, /Line 3, column 7/);
assert.equal((markup.match(/class="field"/g) || []).length, 10);
assert.ok(markup.includes('<div class="field-label">Configuration rejected or recovered</div><div class="field-desc">Path: &lt;img src=x onerror=&quot;bad()&quot;&gt;</div>'));
assert.ok(markup.includes('<div class="field-label">Cannot write daemon log</div><div class="field-desc">Path: &lt;img src=x onerror=&quot;bad()&quot;&gt;</div>'));
assert.ok(markup.includes('&lt;img src=x onerror=&quot;bad()&quot;&gt;</div><div class="field-desc troubleshooting-hint">Review the configuration'));
assert.ok(markup.includes('&lt;img src=x onerror=&quot;bad()&quot;&gt;</div><div class="field-desc troubleshooting-hint">Check the folder permissions'));
window.lwmReceiveDiagnostics({ok: true, files: [{name: hostile, size: 48213, modified: 0}, {name: 'small', size: 1000, modified: 0}]});
assert(!elements['troubleshooting-files'].innerHTML.includes(hostile));
assert.match(elements['troubleshooting-files'].innerHTML, /47\.1 KB/);
assert.match(elements['troubleshooting-files'].innerHTML, /1,000 bytes/);
context.localeStrings = chinese;
document.documentElement.lang = 'zh-CN';
onLanguageChange();
assert.match(elements['troubleshooting-health'].innerHTML, /配置/);
assert.ok(elements['troubleshooting-health'].innerHTML.includes('<div class="field-label">配置已被拒绝或恢复</div><div class="field-desc">路径：&lt;img src=x onerror=&quot;bad()&quot;&gt;</div>'));
assert.ok(elements['troubleshooting-health'].innerHTML.includes('<div class="field-label">无法写入守护进程日志</div><div class="field-desc">路径：&lt;img src=x onerror=&quot;bad()&quot;&gt;</div>'));
assert.ok(elements['troubleshooting-health'].innerHTML.includes('&lt;img src=x onerror=&quot;bad()&quot;&gt;</div><div class="field-desc troubleshooting-hint">请检查配置并重新加载'));
assert.ok(elements['troubleshooting-health'].innerHTML.includes('&lt;img src=x onerror=&quot;bad()&quot;&gt;</div><div class="field-desc troubleshooting-hint">请检查文件夹权限'));
assert.match(elements['troubleshooting-files'].innerHTML, /47\.1 KB/);
assert.match(elements['troubleshooting-files'].innerHTML, /1,000 字节/);
(async () => {
  const first = window.lwmCreateSnapshot();
  const second = window.lwmCreateSnapshot();
  const requests = messages.filter(message => message.action === 'create_snapshot');
  assert.equal(requests.length, 2);
  assert.notEqual(requests[0].id, requests[1].id);
  window.lwmReceiveSnapshot({id: requests[1].id, ok: false, error: hostile});
  window.lwmReceiveSnapshot({id: requests[0].id, ok: true, path: hostile});
  assert.deepEqual(JSON.parse(JSON.stringify(await first)), {ok: true, path: hostile});
  assert.deepEqual(JSON.parse(JSON.stringify(await second)), {ok: false, error: hostile});
  assert.equal(messages.filter(message => message.action === 'list_diagnostics').length, 3);
  context.localeStrings = english;
  const button = elements['troubleshooting-create'], status = elements['troubleshooting-status-bar'];
  const wrapper = elements['troubleshooting-status'];
  assert.equal(wrapper.hidden, false);
  assert.equal(status.hidden, true);
  button.listeners.click.call(button);
  assert.equal(wrapper.hidden, false);
  assert.equal(status.hidden, false);
  assert.equal(button.disabled, true);
  assert.match(elements['troubleshooting-status-text'].textContent, /Creating/);
  window.lwmReceiveSnapshot({id: messages.at(-1).id, ok: true, path: 'snapshot'});
  await Promise.resolve(); await Promise.resolve();
  assert(status.classList.contains('success'));
  button.listeners.click.call(button);
  assert(!status.classList.contains('success'));
  window.lwmReceiveSnapshot({id: messages.at(-1).id, ok: false, error: 'failed'});
  await Promise.resolve(); await Promise.resolve();
  assert(status.classList.contains('error'));
  assert.equal(wrapper.hidden, false);
})().catch(error => { console.error(error); process.exitCode = 1; });
