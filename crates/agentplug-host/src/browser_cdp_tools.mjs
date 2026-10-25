import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';

const TOOL_OPS = new Set([
  'snapshot', 'click', 'dblclick', 'hover', 'click_at', 'fill', 'type', 'press', 'upload',
  'wait_for', 'reload', 'back', 'forward', 'console', 'network', 'dialog',
  'screenshot', 'viewport', 'trace_start', 'trace_stop', 'console_clear', 'clean_html', 'page_markdown',
  'locator', 'ref_to_locator', 'inspect', 'resize',
  'cursor', 'debugger', 'editor',
]);
const INTERACTIVE_ROLES = new Set([
  'button', 'link', 'textbox', 'searchbox', 'checkbox', 'radio', 'combobox', 'listbox', 'menuitem',
  'menuitemcheckbox', 'menuitemradio', 'option', 'slider', 'spinbutton', 'switch', 'tab', 'treeitem',
]);
const AGENT_MAX_DIMENSION = 1568;
const CLEAN_HTML_STRIP = 'script, style, noscript, template, svg, link, meta';
const CAPTURE_LIST_LIMIT = 20;
const MODIFIER_BITS = { Alt: 1, Control: 2, Meta: 4, Shift: 8 };
const MODIFIER_DEFS = {
  Alt: { key: 'Alt', code: 'AltLeft', vk: 18 },
  Control: { key: 'Control', code: 'ControlLeft', vk: 17 },
  Meta: { key: 'Meta', code: 'MetaLeft', vk: 91 },
  Shift: { key: 'Shift', code: 'ShiftLeft', vk: 16 },
};
const NAMED_KEYS = {
  Enter: { key: 'Enter', code: 'Enter', vk: 13, text: '\r' },
  Tab: { key: 'Tab', code: 'Tab', vk: 9 },
  Escape: { key: 'Escape', code: 'Escape', vk: 27 },
  Backspace: { key: 'Backspace', code: 'Backspace', vk: 8 },
  Delete: { key: 'Delete', code: 'Delete', vk: 46 },
  Space: { key: ' ', code: 'Space', vk: 32, text: ' ' },
  Home: { key: 'Home', code: 'Home', vk: 36 },
  End: { key: 'End', code: 'End', vk: 35 },
  PageUp: { key: 'PageUp', code: 'PageUp', vk: 33 },
  PageDown: { key: 'PageDown', code: 'PageDown', vk: 34 },
  ArrowUp: { key: 'ArrowUp', code: 'ArrowUp', vk: 38 },
  ArrowDown: { key: 'ArrowDown', code: 'ArrowDown', vk: 40 },
  ArrowLeft: { key: 'ArrowLeft', code: 'ArrowLeft', vk: 37 },
  ArrowRight: { key: 'ArrowRight', code: 'ArrowRight', vk: 39 },
};
const PUNCTUATION_KEYS = {
  '+': ['Equal', 187], '=': ['Equal', 187], '-': ['Minus', 189], '.': ['Period', 190],
  ',': ['Comma', 188], '/': ['Slash', 191], ';': ['Semicolon', 186], "'": ['Quote', 222],
  '[': ['BracketLeft', 219], ']': ['BracketRight', 221], '\\': ['Backslash', 220], '`': ['Backquote', 192],
};
const SNAPSHOT_PROPERTIES = new Set(['level', 'checked', 'selected', 'disabled', 'expanded', 'focused', 'required', 'url']);
const TRACE_CATEGORIES = [
  'devtools.timeline', 'disabled-by-default-devtools.timeline', 'disabled-by-default-devtools.timeline.frame',
  'blink.user_timing', 'loading', 'rail', 'toplevel', 'v8.execute',
].join(',');
const LONG_TASK_US = 50000;
const SESSION_VERSION = 1;
const SESSION_LOCK_WAIT_MS = 60000;
const SESSION_LOCK_STALE_MS = 120000;
const SELECT_ALL_SCRIPT = `function () {
  if (typeof this.select === 'function') { this.select(); return true; }
  const range = document.createRange();
  range.selectNodeContents(this);
  const selection = window.getSelection();
  selection.removeAllRanges();
  selection.addRange(range);
  return true;
}`;
const SELECT_OPTION_SCRIPT = `function (wanted) {
  const option = Array.from(this.options).find((o) => o.value === wanted || o.text.trim() === wanted);
  if (!option) return null;
  this.value = option.value;
  this.dispatchEvent(new Event('input', { bubbles: true }));
  this.dispatchEvent(new Event('change', { bubbles: true }));
  return option.value;
}`;
const CHECKED_SCRIPT = 'function () { return this.checked === true; }';

const listenerFaults = [];

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function withDeadline(promise, ms, label) {
  let timer = null;
  const expired = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} did not finish within ${ms}ms`)), ms);
  });
  return Promise.race([promise, expired]).finally(() => clearTimeout(timer));
}

function createToolContext({ session, host, cwd, timeoutMs, textLimit }) {
  return {
    session,
    host,
    cwd,
    timeoutMs,
    textLimit,
    uidByBackend: new Map(),
    backendByUid: new Map(),
    nextUid: 0,
    consoleMessages: [],
    networkEntries: [],
    networkByRequest: new Map(),
    captureStartedAt: 0,
    captureFaults: [],
    dialogPolicy: { accept: true, promptText: '' },
    loadWaiters: [],
    trace: null,
    eventEntries: new Map(),
    lastSnapshot: null,
    consoleCursor: 0,
  };
}

function eventField(params, field) {
  let value = params;
  for (const part of field.split('.')) {
    if (value === null || typeof value !== 'object') return undefined;
    value = value[part];
  }
  return value;
}

function eventMatches(filter, params) {
  if (!filter) return true;
  const value = eventField(params, filter.field);
  if (value === undefined || value === null) return false;
  const text = typeof value === 'string' ? value : JSON.stringify(value);
  if (filter.op === 'contains') return text.includes(filter.value);
  if (filter.op === 'not_equals') return text !== filter.value;
  return text === filter.value;
}

function armEventCapture(s, step, entry) {
  s.on(step.event, (params) => {
    if (!eventMatches(step.where || null, params)) return;
    if (entry.events.length >= step.limit) {
      entry.dropped += 1;
      return;
    }
    entry.events.push({ method: step.event, params });
    entry.count = entry.events.length;
  });
  const domain = step.event.split('.')[0];
  return domain ? s.send(`${domain}.enable`).catch(() => {}) : Promise.resolve();
}

function restoreUidState(ctx, state) {
  ctx.nextUid = state.nextUid || 0;
  for (const [backend, uid] of Object.entries(state.uids || {})) {
    ctx.uidByBackend.set(backend, uid);
    ctx.backendByUid.set(uid, Number(backend));
  }
  ctx.lastSnapshot = typeof state.lastSnapshot === 'string' ? state.lastSnapshot : null;
  ctx.consoleCursor = Number.isInteger(state.consoleCursor) ? state.consoleCursor : 0;
}

// lastSnapshot (for diffs) and consoleCursor (for sinceLastCall) persist across calls in the session file.
function snapshotUidState(ctx) {
  return {
    version: SESSION_VERSION,
    nextUid: ctx.nextUid,
    uids: Object.fromEntries(ctx.uidByBackend),
    lastSnapshot: ctx.lastSnapshot,
    consoleCursor: ctx.consoleCursor,
  };
}

// A search is a regex when written /pattern/flags, else a case-insensitive substring.
function searchMatcher(search) {
  const text = String(search);
  const regex = /^\/(.+)\/([gimsuy]*)$/.exec(text);
  if (regex) {
    let compiled;
    try {
      compiled = new RegExp(regex[1], regex[2]);
    } catch (error) {
      throw new Error(`search regex is invalid: ${error.message}`);
    }
    return (line) => compiled.test(line);
  }
  const needle = text.toLowerCase();
  return (line) => line.toLowerCase().includes(needle);
}

// Line diff of two snapshot texts: lines only in the new text are "+", lines only in the old are "-".
function snapshotDiff(previous, current) {
  const before = new Set(previous.split('\n'));
  const after = new Set(current.split('\n'));
  const removed = previous.split('\n').filter((line) => !after.has(line));
  const added = current.split('\n').filter((line) => !before.has(line));
  return [...removed.map((line) => `- ${line}`), ...added.map((line) => `+ ${line}`)].join('\n');
}

// Resolves a CSS locator string to the backend node id of its first match in the page.
async function locatorBackend(ctx, selector) {
  const s = ctx.session;
  const { result, exceptionDetails } = await s.send('Runtime.evaluate', {
    expression: `document.querySelector(${JSON.stringify(selector)})`,
    returnByValue: false,
  });
  if (exceptionDetails) throw new Error(`locator '${selector}' is not a valid CSS selector`);
  if (!result || result.subtype === 'null' || !result.objectId) throw new Error(`locator '${selector}' matched no element`);
  const { node } = await s.send('DOM.describeNode', { objectId: result.objectId });
  return node.backendNodeId;
}

function ensureUid(ctx, backendNodeId) {
  const key = String(backendNodeId);
  if (!ctx.uidByBackend.has(key)) {
    ctx.nextUid += 1;
    const uid = String(ctx.nextUid);
    ctx.uidByBackend.set(key, uid);
    ctx.backendByUid.set(uid, backendNodeId);
  }
  return ctx.uidByBackend.get(key);
}

function backendOf(ctx, uid) {
  const backend = ctx.backendByUid.get(String(uid));
  if (backend === undefined) {
    throw new Error(`uid ${uid} is not in this session's snapshots: take a snapshot and use a uid from its lines`);
  }
  return backend;
}

function axField(field) {
  return field && field.value !== undefined && field.value !== null ? field.value : '';
}

async function takeSnapshot(ctx, options = {}) {
  const s = ctx.session;
  await s.send('DOM.enable');
  await s.send('Accessibility.enable');
  const scopeSelector = options.locator || options.frame;
  let nodes;
  if (scopeSelector) {
    const backendNodeId = await locatorBackend(ctx, scopeSelector);
    ({ nodes } = await s.send('Accessibility.getPartialAXTree', { backendNodeId, fetchRelatives: false }));
  } else {
    ({ nodes } = await s.send('Accessibility.getFullAXTree'));
  }
  if (!nodes || nodes.length === 0) throw new Error('the page has no accessibility tree to snapshot');
  const byId = new Map(nodes.map((node) => [node.nodeId, node]));
  const lines = [];
  const refs = [];
  const visit = (node, depth) => {
    if (!node) return;
    const role = String(axField(node.role) || 'none');
    const name = String(axField(node.name));
    if (node.ignored || ((role === 'none' || role === 'generic') && name === '')) {
      for (const childId of node.childIds || []) visit(byId.get(childId), depth);
      return;
    }
    const value = axField(node.value);
    const uid = node.backendDOMNodeId ? ensureUid(ctx, node.backendDOMNodeId) : null;
    if (uid !== null) refs.push({ uid, role, name, backend: node.backendDOMNodeId, interactive: INTERACTIVE_ROLES.has(role) });
    const props = (node.properties || [])
      .filter((p) => SNAPSHOT_PROPERTIES.has(p.name) && p.value && p.value.value !== false && p.value.value !== '' && p.value.value !== undefined)
      .map((p) => ` ${p.name}=${JSON.stringify(p.value.value)}`)
      .join('');
    const head = [uid === null ? null : `uid=${uid}`, role, name === '' ? null : JSON.stringify(name)]
      .filter((part) => part !== null)
      .join(' ');
    const valuePart = value === '' ? '' : ` value=${JSON.stringify(String(value))}`;
    lines.push(`${'  '.repeat(depth)}${head}${valuePart}${props}`);
    for (const childId of node.childIds || []) visit(byId.get(childId), depth + 1);
  };
  visit(nodes[0], 0);
  const full = lines.join('\n');
  const previous = ctx.lastSnapshot;
  if (options.record !== false) ctx.lastSnapshot = full;
  const report = {
    op: 'snapshot',
    uidCount: ctx.backendByUid.size,
    nodeCount: nodes.length,
    scope: scopeSelector || null,
    refs: refs.filter((ref) => ref.interactive).map(({ uid, role, name }) => ({ uid, role, name })),
  };
  if (options.refs === true) report.allRefs = refs;
  if (options.showDiffSinceLastCall === true) {
    report.diff = previous === null ? null : snapshotDiff(previous, full);
    report.text = full;
    return report;
  }
  if (options.search !== undefined && options.search !== '') {
    const matches = searchMatcher(options.search);
    const kept = lines.filter((line) => matches(line));
    report.matches = kept.length;
    report.text = kept.join('\n');
    return report;
  }
  report.text = full;
  return report;
}

async function describeBackend(s, backendNodeId) {
  const { node } = await s.send('DOM.describeNode', { backendNodeId });
  const attributes = {};
  const flat = node.attributes || [];
  for (let i = 0; i + 1 < flat.length; i += 2) attributes[flat[i]] = flat[i + 1];
  return { tag: String(node.nodeName || '').toLowerCase(), attributes };
}

async function boxOf(s, backendNodeId) {
  await s.send('DOM.scrollIntoViewIfNeeded', { backendNodeId }).catch(() => {});
  let box;
  try {
    box = await s.send('DOM.getBoxModel', { backendNodeId });
  } catch (error) {
    throw new Error(`element is not rendered: ${error.message}`);
  }
  const q = box.model.content;
  const xs = [q[0], q[2], q[4], q[6]];
  const ys = [q[1], q[3], q[5], q[7]];
  const x = Math.min(...xs);
  const y = Math.min(...ys);
  return { x, y, width: Math.max(...xs) - x, height: Math.max(...ys) - y };
}

function centerOf(box) {
  return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
}

async function mouseClick(s, point, clicks) {
  await s.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: point.x, y: point.y });
  for (let count = 1; count <= clicks; count += 1) {
    await s.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: point.x, y: point.y, button: 'left', buttons: 1, clickCount: count });
    await s.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: point.x, y: point.y, button: 'left', buttons: 0, clickCount: count });
  }
}

async function objectOf(s, backendNodeId) {
  const { object } = await s.send('DOM.resolveNode', { backendNodeId });
  return object.objectId;
}

async function callOn(s, objectId, declaration, args = []) {
  const result = await s.send('Runtime.callFunctionOn', {
    objectId,
    functionDeclaration: declaration,
    arguments: args.map((value) => ({ value })),
    returnByValue: true,
    awaitPromise: true,
    userGesture: true,
  });
  if (result.exceptionDetails) {
    const ex = result.exceptionDetails.exception;
    throw new Error(`page script threw: ${(ex && ex.description) || result.exceptionDetails.text}`);
  }
  return result.result ? result.result.value : undefined;
}

async function clickUid(ctx, uid, clicks) {
  const s = ctx.session;
  const backend = backendOf(ctx, uid);
  const point = centerOf(await boxOf(s, backend));
  await mouseClick(s, point, clicks);
  const { tag, attributes } = await describeBackend(s, backend);
  return { op: clicks === 2 ? 'dblclick' : 'click', uid: String(uid), tag, id: attributes.id || null };
}

async function hoverUid(ctx, uid) {
  const s = ctx.session;
  const point = centerOf(await boxOf(s, backendOf(ctx, uid)));
  await s.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: point.x, y: point.y });
  return { op: 'hover', uid: String(uid) };
}

async function clickAt(ctx, x, y) {
  await mouseClick(ctx.session, { x, y }, 1);
  return { op: 'click_at', x, y };
}

function parseWantedBoolean(value) {
  if (value === 'true') return true;
  if (value === 'false') return false;
  throw new Error(`checkbox and radio fills take true or false, got '${value}'`);
}

async function focusBackend(ctx, backend) {
  try {
    await ctx.session.send('DOM.focus', { backendNodeId: backend });
  } catch (_) {
    const point = centerOf(await boxOf(ctx.session, backend));
    await mouseClick(ctx.session, point, 1);
  }
}

async function fillUid(ctx, uid, value) {
  const s = ctx.session;
  const backend = backendOf(ctx, uid);
  const { tag, attributes } = await describeBackend(s, backend);
  const inputType = String(attributes.type || '').toLowerCase();
  if (tag === 'select') {
    const chosen = await callOn(s, await objectOf(s, backend), SELECT_OPTION_SCRIPT, [value]);
    if (chosen === null || chosen === undefined) throw new Error(`no option of the select at uid ${uid} matches '${value}'`);
    return { op: 'fill', uid: String(uid), tag, value: chosen };
  }
  if (tag === 'input' && (inputType === 'checkbox' || inputType === 'radio')) {
    const wanted = parseWantedBoolean(value);
    const checked = await callOn(s, await objectOf(s, backend), CHECKED_SCRIPT);
    if (checked !== wanted) {
      const point = centerOf(await boxOf(s, backend));
      await mouseClick(s, point, 1);
    }
    return { op: 'fill', uid: String(uid), tag, value: wanted };
  }
  await focusBackend(ctx, backend);
  await callOn(s, await objectOf(s, backend), SELECT_ALL_SCRIPT);
  if (value === '') {
    await pressKeyCombo(ctx, 'Backspace');
  } else {
    await s.send('Input.insertText', { text: value });
  }
  return { op: 'fill', uid: String(uid), tag, value };
}

function keyDefinition(name, shift) {
  if (NAMED_KEYS[name]) return NAMED_KEYS[name];
  if (/^F([1-9]|1[0-2])$/.test(name)) {
    return { key: name, code: name, vk: 111 + Number(name.slice(1)) };
  }
  if (name.length === 1 && /[a-z]/i.test(name)) {
    const upper = name.toUpperCase();
    const key = shift ? upper : name.toLowerCase();
    return { key, code: `Key${upper}`, vk: upper.charCodeAt(0), text: key };
  }
  if (name.length === 1 && /[0-9]/.test(name)) {
    return { key: name, code: `Digit${name}`, vk: 48 + Number(name), text: name };
  }
  if (name.length === 1 && PUNCTUATION_KEYS[name]) {
    const [code, vk] = PUNCTUATION_KEYS[name];
    return { key: name, code, vk, text: name };
  }
  throw new Error(`unknown key '${name}': use Enter, Tab, Escape, Backspace, Delete, Space, Home, End, PageUp, PageDown, arrows, F1-F12, a letter, a digit or punctuation, with modifiers Control, Shift, Alt, Meta`);
}

function typeDefinition(ch) {
  if (ch === '\n' || ch === '\r') return NAMED_KEYS.Enter;
  if (ch === ' ') return NAMED_KEYS.Space;
  if (/[A-Za-z0-9]/.test(ch) || PUNCTUATION_KEYS[ch]) return keyDefinition(ch, /[A-Z]/.test(ch));
  return { key: ch, code: '', vk: 0, text: ch };
}

function parseKeySpec(spec) {
  const text = String(spec).trim();
  const endsWithPlus = text.length > 1 && text.endsWith('+');
  const parts = endsWithPlus
    ? text.slice(0, -1).split('+').filter((part) => part.length > 0).concat('+')
    : text.split('+');
  const name = parts.pop();
  const held = [];
  let mask = 0;
  for (const raw of parts) {
    const modifier = raw === 'Ctrl' ? 'Control' : raw === 'Cmd' || raw === 'Command' ? 'Meta' : raw;
    if (!(modifier in MODIFIER_BITS)) throw new Error(`'${raw}' is not a modifier: use Control, Shift, Alt or Meta`);
    held.push(modifier);
    mask |= MODIFIER_BITS[modifier];
  }
  return { held, def: keyDefinition(name, (mask & MODIFIER_BITS.Shift) !== 0) };
}

function dispatchKey(s, type, def, modifiers, text) {
  const params = {
    type,
    modifiers,
    key: def.key,
    code: def.code,
    windowsVirtualKeyCode: def.vk,
    nativeVirtualKeyCode: def.vk,
  };
  if (text !== undefined) params.text = text;
  return s.send('Input.dispatchKeyEvent', params);
}

async function pressKeyCombo(ctx, spec) {
  const s = ctx.session;
  const { held, def } = parseKeySpec(spec);
  let active = 0;
  for (const name of held) {
    active |= MODIFIER_BITS[name];
    await dispatchKey(s, 'rawKeyDown', MODIFIER_DEFS[name], active);
  }
  const printable = def.text !== undefined && (active & (MODIFIER_BITS.Control | MODIFIER_BITS.Meta)) === 0;
  await dispatchKey(s, printable ? 'keyDown' : 'rawKeyDown', def, active, printable ? def.text : undefined);
  await dispatchKey(s, 'keyUp', def, active);
  for (const name of [...held].reverse()) {
    await dispatchKey(s, 'keyUp', MODIFIER_DEFS[name], active);
    active &= ~MODIFIER_BITS[name];
  }
  return { op: 'press', key: String(spec) };
}

async function typeText(ctx, text) {
  const s = ctx.session;
  for (const ch of text) {
    const def = typeDefinition(ch);
    await dispatchKey(s, def.text !== undefined ? 'keyDown' : 'rawKeyDown', def, 0, def.text);
    await dispatchKey(s, 'keyUp', def, 0);
  }
  const focused = await probeSafely(ctx, '(document.activeElement && document.activeElement.tagName) || null');
  return { op: 'type', text, focused };
}

async function uploadUid(ctx, uid, filePaths) {
  const s = ctx.session;
  const backend = backendOf(ctx, uid);
  const { tag, attributes } = await describeBackend(s, backend);
  if (tag !== 'input' || String(attributes.type || '').toLowerCase() !== 'file') {
    throw new Error(`uid ${uid} is a <${tag}>, not a file input`);
  }
  const files = filePaths.map((p) => resolveLocalFile(ctx, p));
  await s.send('DOM.setFileInputFiles', { files, backendNodeId: backend });
  return { op: 'upload', uid: String(uid), files };
}

function resolveLocalFile(ctx, p) {
  const absolute = path.resolve(ctx.cwd, p);
  if (!fs.existsSync(absolute)) throw new Error(`no file at ${absolute}`);
  return absolute;
}

async function probeSafely(ctx, expression) {
  try {
    return await ctx.host.evaluate(ctx.session, expression);
  } catch (_) {
    return null;
  }
}

async function waitForText(ctx, text) {
  const deadline = Date.now() + ctx.timeoutMs;
  const probe = `(document.body ? document.body.innerText : '').includes(${JSON.stringify(text)})`;
  for (;;) {
    if ((await probeSafely(ctx, probe)) === true) return { op: 'wait_for', text, found: true };
    if (Date.now() >= deadline) throw new Error(`text '${text}' did not appear within ${ctx.timeoutMs}ms`);
    await delay(100);
  }
}

async function waitForUrl(ctx, url) {
  const deadline = Date.now() + ctx.timeoutMs;
  const probe = '({ href: location.href, state: document.readyState })';
  for (;;) {
    const page = await probeSafely(ctx, probe);
    if (page && page.href === url && page.state === 'complete') return;
    if (Date.now() >= deadline) throw new Error(`navigation to ${url} did not complete within ${ctx.timeoutMs}ms`);
    await delay(100);
  }
}

function armLoadEvent(ctx) {
  return new Promise((resolve) => ctx.loadWaiters.push(resolve));
}

async function pageState(ctx) {
  return ctx.host.evaluate(ctx.session, '({ url: location.href, title: document.title })');
}

async function reloadPage(ctx) {
  const loaded = armLoadEvent(ctx);
  await ctx.session.send('Page.reload', {});
  await withDeadline(loaded, ctx.timeoutMs, 'page reload');
  return { op: 'reload', ...(await pageState(ctx)) };
}

async function navigateHistory(ctx, direction) {
  const s = ctx.session;
  const { currentIndex, entries } = await s.send('Page.getNavigationHistory');
  const target = entries[currentIndex + direction];
  if (!target) throw new Error(`no history entry ${direction < 0 ? 'before' : 'after'} the current page`);
  await s.send('Page.navigateToHistoryEntry', { entryId: target.id });
  await waitForUrl(ctx, target.url);
  return { op: direction < 0 ? 'back' : 'forward', url: target.url, title: target.title };
}

// count keeps the last N lines, search filters by message text, sinceLastCall starts at the cursor
// left by the previous console report in this session. The cursor always moves to the buffer end.
function consoleReport(ctx, options = {}) {
  let selected = options.sinceLastCall === true ? ctx.consoleMessages.slice(ctx.consoleCursor) : ctx.consoleMessages.slice();
  if (options.search !== undefined && options.search !== '') {
    const matches = searchMatcher(options.search);
    selected = selected.filter((message) => matches(String(message.text)));
  }
  if (options.count !== undefined) {
    const wanted = Number(options.count);
    if (!Number.isInteger(wanted) || wanted < 0) throw new Error(`console count must be a whole number, got '${options.count}'`);
    selected = wanted === 0 ? [] : selected.slice(-wanted);
  }
  ctx.consoleCursor = ctx.consoleMessages.length;
  return { op: 'console', count: ctx.consoleMessages.length, returned: selected.length, messages: selected };
}

function consoleClear(ctx) {
  const cleared = ctx.consoleMessages.length;
  ctx.consoleMessages.length = 0;
  ctx.consoleCursor = 0;
  return { op: 'console_clear', cleared };
}

// An HTTP error is a response with status 400 or more, whatever loadingFailed said after it. A transport
// failure is a loadingFailed on a request with no HTTP error status (DNS, abort, CORS, no response).
function httpErrorStatus(entry) {
  return typeof entry.status === 'number' && entry.status >= 400;
}

function transportFailed(entry) {
  return !!entry.failed && !httpErrorStatus(entry);
}

function networkSummary(entry) {
  return {
    reqid: entry.reqid,
    method: entry.method,
    url: entry.url,
    type: entry.type,
    status: entry.status,
    failed: transportFailed(entry) ? entry.failed : null,
    bytes: entry.bytes,
  };
}

async function networkReport(ctx, reqid) {
  if (reqid === undefined) {
    return { op: 'network', count: ctx.networkEntries.length, requests: ctx.networkEntries.map(networkSummary) };
  }
  const entry = ctx.networkEntries.find((e) => e.reqid === reqid);
  if (!entry) throw new Error(`no network request with reqid ${reqid}`);
  let body = null;
  if (entry.finished) {
    try {
      const got = await ctx.session.send('Network.getResponseBody', { requestId: entry.requestId });
      body = got.base64Encoded
        ? { base64: true, bytes: got.body.length }
        : { base64: false, text: got.body.slice(0, ctx.textLimit), truncated: got.body.length > ctx.textLimit };
    } catch (error) {
      body = { error: error.message };
    }
  }
  return {
    op: 'network',
    request: {
      ...networkSummary(entry),
      mimeType: entry.mimeType,
      requestHeaders: entry.requestHeaders,
      responseHeaders: entry.responseHeaders,
      body,
    },
  };
}

function describeRemote(arg) {
  if (arg.value !== undefined) return typeof arg.value === 'string' ? arg.value : JSON.stringify(arg.value);
  if (arg.unserializableValue !== undefined) return String(arg.unserializableValue);
  return arg.description || arg.type || 'undefined';
}

// Events stamped before the call started are history the page logged earlier, not this call's events.
function capturedDuring(ctx, timestamp) {
  return typeof timestamp !== 'number' || timestamp >= ctx.captureStartedAt;
}

function stackLines(stackTrace) {
  const frames = (stackTrace && stackTrace.callFrames) || [];
  return frames
    .map((f) => `    at ${f.functionName || '<anonymous>'} (${f.url || '<anonymous>'}:${f.lineNumber + 1}:${f.columnNumber + 1})`)
    .join('\n');
}

function attachConsole(ctx, s) {
  const push = (message) => ctx.consoleMessages.push({ msgid: ctx.consoleMessages.length + 1, ...message });
  s.on('Runtime.consoleAPICalled', (p) => {
    if (!capturedDuring(ctx, p.timestamp)) return;
    const frame = p.stackTrace && p.stackTrace.callFrames && p.stackTrace.callFrames[0];
    push({
      kind: 'console',
      type: p.type === 'assert' ? 'error' : p.type,
      text: (p.args || []).map(describeRemote).join(' '),
      url: frame ? frame.url || null : null,
      line: frame ? frame.lineNumber + 1 : null,
      column: frame ? frame.columnNumber + 1 : null,
      timestamp: p.timestamp ?? null,
    });
  });
  s.on('Runtime.exceptionThrown', (p) => {
    if (!capturedDuring(ctx, p.timestamp)) return;
    const details = p.exceptionDetails || {};
    const exception = details.exception || {};
    push({
      kind: details.text === 'Uncaught (in promise)' ? 'unhandled_rejection' : 'exception',
      type: 'error',
      text: exception.description || exception.value || details.text || 'uncaught exception',
      url: details.url || null,
      line: details.lineNumber != null ? details.lineNumber + 1 : null,
      column: details.columnNumber != null ? details.columnNumber + 1 : null,
      timestamp: p.timestamp ?? null,
      stack: stackLines(details.stackTrace) || null,
    });
  });
  s.on('Log.entryAdded', (p) => {
    const e = p.entry;
    if (!capturedDuring(ctx, e.timestamp)) return;
    push({
      kind: 'log',
      type: e.level === 'verbose' ? 'debug' : e.level,
      text: e.text,
      url: e.url || null,
      line: e.lineNumber != null ? e.lineNumber + 1 : null,
      column: null,
      timestamp: e.timestamp ?? null,
      source: e.source,
    });
  });
}

function attachNetwork(ctx, s) {
  s.on('Network.requestWillBeSent', (p) => {
    if (p.redirectResponse) {
      const prior = ctx.networkByRequest.get(p.requestId);
      if (prior) {
        prior.status = p.redirectResponse.status;
        prior.mimeType = p.redirectResponse.mimeType;
      }
    }
    const entry = {
      reqid: ctx.networkEntries.length + 1,
      requestId: p.requestId,
      method: p.request.method,
      url: p.request.url,
      type: p.type || 'Other',
      status: null,
      mimeType: null,
      failed: false,
      bytes: null,
      finished: false,
      requestHeaders: p.request.headers || {},
      responseHeaders: null,
    };
    ctx.networkEntries.push(entry);
    ctx.networkByRequest.set(p.requestId, entry);
  });
  s.on('Network.responseReceived', (p) => {
    const entry = ctx.networkByRequest.get(p.requestId);
    if (!entry) return;
    entry.status = p.response.status;
    entry.mimeType = p.response.mimeType;
    entry.responseHeaders = p.response.headers || {};
  });
  s.on('Network.responseReceivedExtraInfo', (p) => {
    const entry = ctx.networkByRequest.get(p.requestId);
    if (!entry || entry.status !== null || typeof p.statusCode !== 'number') return;
    entry.status = p.statusCode;
  });
  s.on('Network.loadingFinished', (p) => {
    const entry = ctx.networkByRequest.get(p.requestId);
    if (!entry) return;
    entry.bytes = p.encodedDataLength;
    entry.finished = true;
  });
  s.on('Network.loadingFailed', (p) => {
    const entry = ctx.networkByRequest.get(p.requestId);
    if (!entry) return;
    entry.failed = p.errorText || true;
  });
}

async function attachCapture(ctx, steps) {
  const s = ctx.session;
  s.on('Page.javascriptDialogOpening', (p) => {
    ctx.consoleMessages.push({
      msgid: ctx.consoleMessages.length + 1,
      kind: 'dialog',
      type: 'dialog',
      text: `${p.type}: ${p.message}`,
      url: p.url || null,
      line: null,
      handled: ctx.dialogPolicy.accept ? 'accept' : 'dismiss',
    });
    s.send('Page.handleJavaScriptDialog', { accept: ctx.dialogPolicy.accept, promptText: ctx.dialogPolicy.promptText }).catch(() => {});
  });
  s.on('Page.loadEventFired', () => {
    for (const resolve of ctx.loadWaiters.splice(0)) resolve();
  });
  s.on('Tracing.dataCollected', (p) => {
    if (!ctx.trace) return;
    for (const event of p.value) ctx.trace.events.push(event);
  });
  s.on('Tracing.tracingComplete', () => {
    if (ctx.trace && ctx.trace.finish) ctx.trace.finish();
  });
  await s.send('Page.enable').catch(() => {});
  if (steps.some((step) => TOOL_OPS.has(step.op))) await s.send('DOM.enable').catch(() => {});
  attachConsole(ctx, s);
  attachNetwork(ctx, s);
  ctx.captureStartedAt = Date.now();
  await Promise.all(['Runtime.enable', 'Log.enable', 'Network.enable'].map((method) => s.send(method).catch((error) => {
    ctx.captureFaults.push(`${method}: ${error.message}`);
  })));
  const eventSteps = steps.filter((step) => step.op === 'events');
  for (const step of eventSteps) {
    ctx.eventEntries.set(step, {
      op: 'events',
      event: step.event,
      where: step.where || null,
      count: 0,
      dropped: 0,
      events: [],
    });
  }
  for (const step of eventSteps) {
    await armEventCapture(s, step, ctx.eventEntries.get(step));
  }
}

async function disableCapture(ctx) {
  const s = ctx.session;
  await Promise.all(['Runtime.disable', 'Log.disable', 'Network.disable'].map((method) =>
    withDeadline(s.send(method), 500, method).catch(() => {})));
}

const CAPTURE_DIR_PARTS = ['.gm', 'browser-cdp-capture'];
const CAPTURE_KEEP_FILES = 100;

function isConsoleError(message) {
  return message.type === 'error';
}

// by_status key: the HTTP status of an HTTP error, else 'failed' for a transport failure.
function failureKey(entry) {
  return httpErrorStatus(entry) ? String(entry.status) : 'failed';
}

function isFailedRequest(entry) {
  return httpErrorStatus(entry) || transportFailed(entry);
}

function hostOfUrl(url) {
  try {
    const parsed = new URL(url);
    return parsed.host || parsed.protocol;
  } catch (_) {
    return 'unparsed';
  }
}

function tally(items, keyOf) {
  const counts = {};
  for (const item of items) {
    const key = String(keyOf(item));
    counts[key] = (counts[key] || 0) + 1;
  }
  return counts;
}

function consoleView(message) {
  return {
    msgid: message.msgid,
    level: message.type,
    kind: message.kind,
    text: message.text,
    url: message.url ?? null,
    line: message.line ?? null,
    column: message.column ?? null,
    timestamp: message.timestamp ?? null,
    ...(message.source !== undefined ? { source: message.source } : {}),
    ...(message.stack ? { stack: message.stack } : {}),
  };
}

function failureView(entry) {
  return {
    reqid: entry.reqid,
    method: entry.method,
    url: entry.url,
    type: entry.type,
    status: entry.status,
    failed: transportFailed(entry) ? entry.failed : null,
  };
}

function pruneCaptureDir(dir) {
  const files = fs.readdirSync(dir).filter((name) => name.endsWith('.json')).sort();
  for (const name of files.slice(0, Math.max(0, files.length - CAPTURE_KEEP_FILES))) {
    try { fs.unlinkSync(path.join(dir, name)); } catch (_) {}
  }
}

function notificationFor(error, failure) {
  if (error) {
    return { kind: error.kind, level: error.type, text: error.text, url: error.url ?? null, line: error.line ?? null };
  }
  if (failure) {
    const outcome = httpErrorStatus(failure) ? `HTTP ${failure.status}` : `failed: ${failure.failed}`;
    return { kind: 'network', level: 'error', text: `${failure.method} ${failure.url} ${outcome}`, url: failure.url, line: null, status: failure.status ?? null };
  }
  return null;
}

// Writes the call's capture into out. The reply stays small: console_events lists errors (every event with
// consoleAll), network counts and the first CAPTURE_LIST_LIMIT failing urls. The capture file holds every
// console event and every failed request (status 400+ or a failed load), and is written only when there are some.
function finishCapture(ctx, out, options) {
  const messages = ctx ? ctx.consoleMessages : [];
  const requests = ctx ? ctx.networkEntries : [];
  const faults = ctx ? ctx.captureFaults : [];
  const failures = requests.filter(isFailedRequest);
  const errors = messages.filter(isConsoleError);
  out.console_event_count = messages.length;
  out.console_levels = tally(messages, (message) => message.type);
  out.console_events = (options.consoleAll ? messages : errors).map(consoleView);
  out.network = {
    requests: requests.length,
    failed: failures.filter(transportFailed).length,
    by_status: tally(failures, failureKey),
    by_host: tally(failures, (entry) => hostOfUrl(entry.url)),
    urls: failures.slice(0, CAPTURE_LIST_LIMIT).map((entry) => entry.url),
  };
  out.notification_count = errors.length + failures.length;
  out.notification = notificationFor(errors[0], failures[0]);
  if (faults.length) out.capture_faults = faults;
  out.capture_file = null;
  if (messages.length === 0 && failures.length === 0 && faults.length === 0) return;
  const dir = path.join(options.cwd, ...CAPTURE_DIR_PARTS);
  const file = path.join(dir, `${Date.now()}-${process.pid}.json`);
  try {
    fs.mkdirSync(dir, { recursive: true });
    fs.writeFileSync(file, JSON.stringify({
      console_events: messages.map(consoleView),
      network_requests: requests.length,
      network_failures: failures.map(failureView),
      capture_faults: faults,
    }, null, 2));
    out.capture_file = file;
    pruneCaptureDir(dir);
  } catch (e) {
    out.capture_error = `capture file was not written: ${e.message}`;
  }
}

function screenshotFormat(filePath) {
  const ext = path.extname(filePath).toLowerCase();
  if (ext === '.jpg' || ext === '.jpeg') return 'jpeg';
  if (ext === '.webp') return 'webp';
  return 'png';
}

const LABEL_OVERLAY_SCRIPT = `function (items) {
  const root = document.createElement('div');
  root.setAttribute('data-gm-labels', '');
  root.style.cssText = 'position:fixed;left:0;top:0;width:0;height:0;z-index:2147483647;pointer-events:none';
  for (const item of items) {
    const label = document.createElement('div');
    label.textContent = String(item.n);
    label.style.cssText = 'position:fixed;left:' + item.x + 'px;top:' + item.y + 'px;background:#d00;color:#fff;font:bold 12px sans-serif;padding:0 3px;border-radius:2px';
    root.appendChild(label);
  }
  document.documentElement.appendChild(root);
  return items.length;
}`;
const LABEL_REMOVE_SCRIPT = `(() => { const el = document.querySelector('[data-gm-labels]'); if (el) el.remove(); return true; })()`;

// Labels each rendered snapshot ref with its uid, so the numbers match the snapshot lines. interactiveOnly
// (default true) limits the labels to interactive roles. Labels are removed in finally, even on capture error.
async function labelledCapture(ctx, captureFn, interactiveOnly) {
  const snapshot = await takeSnapshot(ctx, { record: false, refs: true });
  const wanted = snapshot.allRefs.filter((ref) => (interactiveOnly ? ref.interactive : true));
  const items = [];
  for (const ref of wanted) {
    try {
      const box = await boxOf(ctx.session, ref.backend);
      items.push({ n: ref.uid, x: Math.round(box.x), y: Math.round(box.y) });
    } catch (_) {
      // Not rendered (display:none or off-tree): no label.
    }
  }
  await ctx.host.evaluate(ctx.session, `(${LABEL_OVERLAY_SCRIPT})(${JSON.stringify(items)})`);
  try {
    return { result: await captureFn(), labels: items.length };
  } finally {
    await ctx.host.evaluate(ctx.session, LABEL_REMOVE_SCRIPT).catch(() => {});
  }
}

async function screenshotStep(ctx, step) {
  const s = ctx.session;
  const format = screenshotFormat(step.path);
  const params = { format, captureBeyondViewport: step.full === true };
  if (step.uid !== undefined) {
    const box = await boxOf(s, backendOf(ctx, step.uid));
    params.clip = { x: box.x, y: box.y, width: box.width, height: box.height, scale: 1 };
  } else if (step.full === true) {
    const metrics = await s.send('Page.getLayoutMetrics');
    const size = metrics.cssContentSize || metrics.contentSize;
    params.clip = { x: 0, y: 0, width: size.width, height: size.height, scale: 1 };
  }
  if (step.max !== undefined) {
    const max = Number(step.max);
    if (!Number.isInteger(max) || max < 1) throw new Error(`screenshot max must be a whole number of pixels, got '${step.max}'`);
    const size = params.clip || await viewportSize(s);
    const scale = Math.min(1, max / Math.max(size.width, size.height));
    params.clip = { x: size.x || 0, y: size.y || 0, width: size.width, height: size.height, scale };
  }
  const capture = () => s.send('Page.captureScreenshot', params);
  let shot;
  let labels = null;
  if (step.labels === true) {
    const labelled = await labelledCapture(ctx, capture, step.interactiveOnly !== false);
    shot = labelled.result;
    labels = labelled.labels;
  } else {
    shot = await capture();
  }
  const target = path.resolve(ctx.cwd, step.path);
  const bytes = Buffer.from(shot.data, 'base64');
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, bytes);
  return {
    op: 'screenshot',
    path: target,
    format,
    bytes: bytes.length,
    full: step.full === true,
    uid: step.uid ?? null,
    labels,
    max: step.max === undefined ? null : Number(step.max),
  };
}

async function viewportSize(s) {
  const metrics = await s.send('Page.getLayoutMetrics');
  const view = metrics.cssVisualViewport || metrics.visualViewport || metrics.cssLayoutViewport;
  return { x: view.pageX || 0, y: view.pageY || 0, width: view.clientWidth, height: view.clientHeight };
}

const VIEWPORT_STEP_MS = 10000;

// Sets the device metrics for the rest of this call and reads back the size the page reports. Each command has
// its own VIEWPORT_STEP_MS cap (and the call budget still bounds it), so a stalled emulation fails this step.
async function viewportStep(ctx, step) {
  const s = ctx.session;
  const width = Number(step.width);
  const height = Number(step.height);
  if (!Number.isInteger(width) || !Number.isInteger(height) || width < 1 || height < 1) {
    throw new Error(`viewport needs whole-number width and height, got ${step.width}x${step.height}`);
  }
  await withDeadline(
    s.send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: false }),
    VIEWPORT_STEP_MS,
    `viewport ${width}x${height}`,
  );
  const reported = await withDeadline(
    ctx.host.evaluate(s, '({ width: innerWidth, height: innerHeight })'),
    VIEWPORT_STEP_MS,
    'viewport readback',
  );
  return { op: 'viewport', width, height, reported };
}

function summarizeTrace(events) {
  const timed = events.filter((e) => typeof e.ts === 'number' && e.ph !== 'M');
  if (timed.length === 0) {
    return { durationMs: 0, longTasks: 0, totalBlockingMs: 0, firstContentfulPaintMs: null, largestContentfulPaintMs: null, topEvents: [] };
  }
  let start = Infinity;
  let end = -Infinity;
  for (const e of timed) {
    start = Math.min(start, e.ts);
    end = Math.max(end, e.ts + (typeof e.dur === 'number' ? e.dur : 0));
  }
  const tasks = timed.filter((e) => e.name === 'RunTask' && typeof e.dur === 'number');
  const longTasks = tasks.filter((e) => e.dur > LONG_TASK_US);
  const blockingUs = longTasks.reduce((sum, e) => sum + (e.dur - LONG_TASK_US), 0);
  const fcp = timed.find((e) => e.name === 'firstContentfulPaint');
  const lcpCandidates = timed.filter((e) => e.name === 'largestContentfulPaint::Candidate');
  const lcp = lcpCandidates.length ? lcpCandidates[lcpCandidates.length - 1] : null;
  const topEvents = timed
    .filter((e) => typeof e.dur === 'number')
    .sort((a, b) => b.dur - a.dur)
    .slice(0, 5)
    .map((e) => ({ name: e.name, durationMs: Math.round(e.dur / 10) / 100 }));
  return {
    durationMs: Math.round((end - start) / 10) / 100,
    longTasks: longTasks.length,
    totalBlockingMs: Math.round(blockingUs / 10) / 100,
    firstContentfulPaintMs: fcp ? Math.round((fcp.ts - start) / 10) / 100 : null,
    largestContentfulPaintMs: lcp ? Math.round((lcp.ts - start) / 10) / 100 : null,
    topEvents,
  };
}

async function traceStart(ctx) {
  if (ctx.trace) throw new Error('a trace is already recording: run trace_stop first');
  ctx.trace = { events: [], finish: null };
  await ctx.session.send('Tracing.start', { categories: TRACE_CATEGORIES, transferMode: 'ReportEvents' });
  return { op: 'trace_start', categories: TRACE_CATEGORIES };
}

async function traceStop(ctx, tracePath) {
  if (!ctx.trace) throw new Error('no trace is recording: run trace_start first');
  const done = new Promise((resolve) => {
    ctx.trace.finish = resolve;
  });
  await ctx.session.send('Tracing.end');
  await withDeadline(done, ctx.timeoutMs, 'trace completion');
  const events = ctx.trace.events;
  ctx.trace = null;
  let savedTo = null;
  if (tracePath) {
    savedTo = path.resolve(ctx.cwd, tracePath);
    const json = JSON.stringify({ traceEvents: events });
    const payload = savedTo.endsWith('.gz') ? zlib.gzipSync(json) : Buffer.from(json, 'utf8');
    fs.mkdirSync(path.dirname(savedTo), { recursive: true });
    fs.writeFileSync(savedTo, payload);
  }
  return { op: 'trace_stop', path: savedTo, eventCount: events.length, summary: summarizeTrace(events) };
}

const CLEAN_HTML_SCRIPT = `function (selector, maxAttr, maxContent, strip) {
  const root = selector ? document.querySelector(selector) : document.body;
  if (!root) return null;
  const clone = root.cloneNode(true);
  clone.querySelectorAll(strip).forEach((node) => node.remove());
  const walker = document.createTreeWalker(clone, NodeFilter.SHOW_ELEMENT | NodeFilter.SHOW_TEXT);
  let node = walker.currentNode;
  while (node) {
    if (node.nodeType === Node.ELEMENT_NODE) {
      for (const attr of Array.from(node.attributes)) {
        if (attr.value.length > maxAttr) node.setAttribute(attr.name, attr.value.slice(0, maxAttr) + '...');
      }
    } else if (node.nodeValue.length > maxContent) {
      node.nodeValue = node.nodeValue.slice(0, maxContent) + '...';
    }
    node = walker.nextNode();
  }
  return clone.outerHTML.replace(/>\\s*</g, '>\\n<');
}`;

const PAGE_MARKDOWN_SCRIPT = `function (selector) {
  const root = selector ? document.querySelector(selector) : document.body;
  if (!root) return null;
  const out = [];
  const BLOCK = ['p', 'div', 'section', 'article', 'ul', 'ol', 'tr', 'header', 'footer', 'main', 'nav', 'blockquote', 'pre', 'table'];
  const visit = (node) => {
    if (node.nodeType === Node.TEXT_NODE) {
      const text = node.nodeValue.replace(/\\s+/g, ' ');
      if (text.trim()) out.push(text);
      return;
    }
    if (node.nodeType !== Node.ELEMENT_NODE) return;
    const tag = node.tagName.toLowerCase();
    if (['script', 'style', 'noscript', 'template', 'svg'].includes(tag) || node.hidden) return;
    const heading = /^h([1-6])$/.exec(tag);
    if (heading) {
      out.push('\\n\\n' + '#'.repeat(Number(heading[1])) + ' ');
      for (const child of node.childNodes) visit(child);
      out.push('\\n\\n');
      return;
    }
    if (tag === 'a') {
      out.push('[');
      for (const child of node.childNodes) visit(child);
      out.push('](' + node.href + ')');
      return;
    }
    if (tag === 'img') {
      out.push('![' + (node.alt || '') + '](' + node.src + ')');
      return;
    }
    if (tag === 'li') {
      out.push('\\n- ');
      for (const child of node.childNodes) visit(child);
      return;
    }
    if (tag === 'br') {
      out.push('\\n');
      return;
    }
    const block = BLOCK.includes(tag);
    if (block) out.push('\\n');
    for (const child of node.childNodes) visit(child);
    if (block) out.push('\\n');
  };
  visit(root);
  return out.join('').replace(/\\n{3,}/g, '\\n\\n').trim();
}`;

// Returns a CSS path for `this`: the first element with a unique id, else tag:nth-of-type segments from the root.
const LOCATOR_SCRIPT = `function () {
  const segments = [];
  let el = this;
  while (el && el.nodeType === 1) {
    if (el.id && document.querySelectorAll('#' + CSS.escape(el.id)).length === 1) {
      segments.unshift('#' + CSS.escape(el.id));
      break;
    }
    const tag = el.tagName.toLowerCase();
    const siblings = Array.from(el.parentElement ? el.parentElement.children : [el]).filter((c) => c.tagName === el.tagName);
    segments.unshift(siblings.length > 1 ? tag + ':nth-of-type(' + (siblings.indexOf(el) + 1) + ')' : tag);
    el = el.parentElement;
  }
  return segments.join(' > ');
}`;

const INSPECT_SCRIPT = `function (names) {
  const rect = this.getBoundingClientRect();
  const aria = {};
  for (const attr of Array.from(this.attributes)) {
    if (attr.name.startsWith('aria-')) aria[attr.name] = attr.value;
  }
  const properties = {};
  for (const name of names || []) {
    const value = this[name];
    const kind = typeof value;
    properties[name] = kind === 'function' || value === undefined ? null : kind === 'object' && value !== null ? String(value).slice(0, 200) : value;
  }
  return {
    tag: this.tagName.toLowerCase(),
    box: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
    aria,
    input: {
      value: 'value' in this ? this.value : null,
      checked: 'checked' in this ? this.checked : null,
      disabled: this.disabled === true,
      required: this.required === true,
      focused: document.activeElement === this,
    },
    properties,
  };
}`;

const RESIZE_SCRIPT = `async (src, max, mime) => {
  const img = new Image();
  img.src = src;
  await img.decode();
  const scale = Math.min(1, max / Math.max(img.naturalWidth, img.naturalHeight));
  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, Math.round(img.naturalWidth * scale));
  canvas.height = Math.max(1, Math.round(img.naturalHeight * scale));
  canvas.getContext('2d').drawImage(img, 0, 0, canvas.width, canvas.height);
  return {
    data: canvas.toDataURL(mime).split(',')[1],
    width: canvas.width,
    height: canvas.height,
    sourceWidth: img.naturalWidth,
    sourceHeight: img.naturalHeight,
  };
}`;

const IMAGE_MIME = { '.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg', '.webp': 'image/webp' };

function wholeNumber(value, what, fallback) {
  if (value === undefined) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 1) throw new Error(`${what} must be a whole number of at least 1, got '${value}'`);
  return parsed;
}

function capText(ctx, text) {
  if (text.length <= ctx.textLimit) return { text, truncated: false };
  return { text: text.slice(0, ctx.textLimit), truncated: true };
}

async function cleanHtml(ctx, step) {
  const maxAttr = wholeNumber(step.maxAttrLen, 'maxAttrLen', 100);
  const maxContent = wholeNumber(step.maxContentLen, 'maxContentLen', 500);
  const html = await ctx.host.evaluate(
    ctx.session,
    `(${CLEAN_HTML_SCRIPT})(${JSON.stringify(step.locator || null)}, ${maxAttr}, ${maxContent}, ${JSON.stringify(CLEAN_HTML_STRIP)})`,
  );
  if (html === null) throw new Error(`locator '${step.locator}' matched no element`);
  let body = html;
  if (step.search !== undefined && step.search !== '') {
    const matches = searchMatcher(step.search);
    body = html.split('\n').filter((line) => matches(line)).join('\n');
  }
  return { op: 'clean_html', locator: step.locator || null, ...capText(ctx, body) };
}

async function pageMarkdown(ctx, step) {
  const markdown = await ctx.host.evaluate(ctx.session, `(${PAGE_MARKDOWN_SCRIPT})(${JSON.stringify(step.locator || null)})`);
  if (markdown === null) throw new Error(`locator '${step.locator}' matched no element`);
  return { op: 'page_markdown', locator: step.locator || null, ...capText(ctx, markdown) };
}

async function locatorOfUid(ctx, uid) {
  const s = ctx.session;
  const locator = await callOn(s, await objectOf(s, backendOf(ctx, uid)), LOCATOR_SCRIPT);
  return { op: 'locator', uid: String(uid), locator };
}

async function refToLocator(ctx, ref) {
  const match = /^(?:uid=)?(\d+)$/.exec(String(ref).trim());
  if (!match) throw new Error(`ref must be a uid as printed in a snapshot line (uid=N or N), got '${ref}'`);
  const located = await locatorOfUid(ctx, match[1]);
  return { op: 'ref_to_locator', ref: String(ref), uid: located.uid, locator: located.locator };
}

async function inspectElement(ctx, step) {
  const s = ctx.session;
  let backend;
  if (step.uid !== undefined) backend = backendOf(ctx, step.uid);
  else if (step.locator) backend = await locatorBackend(ctx, step.locator);
  else throw new Error('inspect needs a uid or a locator');
  const names = Array.isArray(step.properties) ? step.properties.map(String) : [];
  const report = await callOn(s, await objectOf(s, backend), INSPECT_SCRIPT, [names]);
  return { op: 'inspect', uid: step.uid === undefined ? null : String(step.uid), locator: step.locator || null, ...report };
}

async function resizeImage(ctx, step) {
  if (typeof step.image !== 'string' || step.image === '') throw new Error('resize needs an image path');
  const max = wholeNumber(step.max, 'resize max', AGENT_MAX_DIMENSION);
  const file = resolveLocalFile(ctx, step.image);
  const ext = path.extname(file).toLowerCase();
  const mime = IMAGE_MIME[ext];
  if (!mime) throw new Error(`resize takes a png, jpg or webp image, got '${ext || 'no extension'}'`);
  const src = `data:${mime};base64,${fs.readFileSync(file).toString('base64')}`;
  const resized = await ctx.host.evaluate(ctx.session, `(${RESIZE_SCRIPT})(${JSON.stringify(src)}, ${max}, ${JSON.stringify(mime)})`);
  if (!resized || !resized.data) throw new Error(`the browser could not decode ${file}`);
  const target = path.join(path.dirname(file), `${path.basename(file, ext)}.max${max}${ext}`);
  const bytes = Buffer.from(resized.data, 'base64');
  fs.writeFileSync(target, bytes);
  return {
    op: 'resize',
    image: file,
    path: target,
    max,
    width: resized.width,
    height: resized.height,
    sourceWidth: resized.sourceWidth,
    sourceHeight: resized.sourceHeight,
    bytes: bytes.length,
  };
}

function setDialogPolicy(ctx, action, promptText) {
  if (promptText !== undefined && typeof promptText !== 'string') throw new Error('dialog promptText must be a string');
  ctx.dialogPolicy = { accept: action === 'accept', promptText: promptText || '' };
  return { op: 'dialog', action, promptText: ctx.dialogPolicy.promptText };
}

async function runToolStep(ctx, step) {
  switch (step.op) {
    case 'snapshot': return takeSnapshot(ctx, step);
    case 'clean_html': return cleanHtml(ctx, step);
    case 'page_markdown': return pageMarkdown(ctx, step);
    case 'locator': return locatorOfUid(ctx, step.uid);
    case 'ref_to_locator': return refToLocator(ctx, step.ref);
    case 'inspect': return inspectElement(ctx, step);
    case 'resize': return resizeImage(ctx, step);
    case 'console_clear': return consoleClear(ctx);
    case 'click': return clickUid(ctx, step.uid, 1);
    case 'dblclick': return clickUid(ctx, step.uid, 2);
    case 'hover': return hoverUid(ctx, step.uid);
    case 'click_at': return clickAt(ctx, step.x, step.y);
    case 'fill': return fillUid(ctx, step.uid, step.value);
    case 'type': return typeText(ctx, step.text);
    case 'press': {
      if (step.uid !== undefined) await focusBackend(ctx, backendOf(ctx, step.uid));
      return { ...(await pressKeyCombo(ctx, step.key)), uid: step.uid === undefined ? null : String(step.uid) };
    }
    case 'upload': return uploadUid(ctx, step.uid, step.paths);
    case 'wait_for': return waitForText(ctx, step.text);
    case 'reload': return reloadPage(ctx);
    case 'back': return navigateHistory(ctx, -1);
    case 'forward': return navigateHistory(ctx, 1);
    case 'console': return consoleReport(ctx, step);
    case 'network': return networkReport(ctx, step.reqid);
    case 'dialog': return setDialogPolicy(ctx, step.action, step.promptText);
    case 'screenshot': return screenshotStep(ctx, step);
    case 'viewport': return viewportStep(ctx, step);
    case 'trace_start': return traceStart(ctx);
    case 'trace_stop': return traceStop(ctx, step.path || '');
    case 'cursor': return cursorStep(ctx, step);
    case 'debugger': return debuggerStep(ctx, step);
    case 'editor': return editorStep(ctx, step);
    default: throw new Error(`unknown tool op '${step.op}'`);
  }
}

function readSessionFile(file) {
  if (!fs.existsSync(file)) return null;
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch (error) {
    throw new Error(`session file ${file} is not valid JSON (${error.message}): delete it to start that session again`);
  }
}

function writeSessionFile(file, state) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const temp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(temp, JSON.stringify(state, null, 2));
  fs.renameSync(temp, file);
}

async function lockSession(file, waitMs) {
  const lock = `${file}.lock`;
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const deadline = Date.now() + waitMs;
  for (;;) {
    try {
      fs.writeFileSync(lock, String(process.pid), { flag: 'wx' });
      return () => {
        try { fs.unlinkSync(lock); } catch (_) {}
      };
    } catch (error) {
      if (error.code !== 'EEXIST') throw error;
      let ageMs = 0;
      try {
        ageMs = Date.now() - fs.statSync(lock).mtimeMs;
      } catch (_) {
        continue;
      }
      if (ageMs > SESSION_LOCK_STALE_MS) {
        try { fs.unlinkSync(lock); } catch (_) {}
        continue;
      }
      if (Date.now() >= deadline) throw new Error(`session is busy: another browser holds ${lock}`);
      await delay(200);
    }
  }
}
