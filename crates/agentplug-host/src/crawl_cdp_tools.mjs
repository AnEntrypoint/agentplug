import fs from 'node:fs';
import path from 'node:path';
import zlib from 'node:zlib';

const TOOL_OPS = new Set([
  'snapshot', 'click', 'dblclick', 'hover', 'click_at', 'fill', 'type', 'press', 'upload',
  'wait_for', 'reload', 'back', 'forward', 'console', 'network', 'dialog',
  'screenshot', 'trace_start', 'trace_stop',
]);
const CONSOLE_OPS = new Set(['console']);
const NETWORK_OPS = new Set(['network']);
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
    dialogPolicy: { accept: true, promptText: '' },
    loadWaiters: [],
    trace: null,
  };
}

function restoreUidState(ctx, state) {
  ctx.nextUid = state.nextUid || 0;
  for (const [backend, uid] of Object.entries(state.uids || {})) {
    ctx.uidByBackend.set(backend, uid);
    ctx.backendByUid.set(uid, Number(backend));
  }
}

function snapshotUidState(ctx) {
  return { version: SESSION_VERSION, nextUid: ctx.nextUid, uids: Object.fromEntries(ctx.uidByBackend) };
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

async function takeSnapshot(ctx) {
  const s = ctx.session;
  await s.send('DOM.enable');
  await s.send('Accessibility.enable');
  const { nodes } = await s.send('Accessibility.getFullAXTree');
  const byId = new Map(nodes.map((node) => [node.nodeId, node]));
  const lines = [];
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
  return { op: 'snapshot', uidCount: ctx.backendByUid.size, nodeCount: nodes.length, text: lines.join('\n') };
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

function consoleReport(ctx) {
  return { op: 'console', count: ctx.consoleMessages.length, messages: ctx.consoleMessages };
}

function networkSummary(entry) {
  return {
    reqid: entry.reqid,
    method: entry.method,
    url: entry.url,
    type: entry.type,
    status: entry.status,
    failed: entry.failed,
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

function attachConsole(ctx, s) {
  const push = (message) => ctx.consoleMessages.push({ msgid: ctx.consoleMessages.length + 1, ...message });
  s.on('Runtime.consoleAPICalled', (p) => {
    const frame = p.stackTrace && p.stackTrace.callFrames && p.stackTrace.callFrames[0];
    push({
      type: p.type,
      text: p.args.map(describeRemote).join(' '),
      url: frame ? frame.url || null : null,
      line: frame ? frame.lineNumber + 1 : null,
    });
  });
  s.on('Runtime.exceptionThrown', (p) => {
    const details = p.exceptionDetails || {};
    push({
      type: 'error',
      text: (details.exception && details.exception.description) || details.text || 'uncaught exception',
      url: details.url || null,
      line: details.lineNumber != null ? details.lineNumber + 1 : null,
    });
  });
  s.on('Log.entryAdded', (p) => {
    const e = p.entry;
    push({ type: e.level, text: e.text, url: e.url || null, line: e.lineNumber != null ? e.lineNumber + 1 : null, source: e.source });
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
  if (steps.some((step) => CONSOLE_OPS.has(step.op))) {
    attachConsole(ctx, s);
    await s.send('Runtime.enable');
    await s.send('Log.enable').catch(() => {});
  }
  if (steps.some((step) => NETWORK_OPS.has(step.op))) {
    attachNetwork(ctx, s);
    await s.send('Network.enable');
  }
}

function screenshotFormat(filePath) {
  const ext = path.extname(filePath).toLowerCase();
  if (ext === '.jpg' || ext === '.jpeg') return 'jpeg';
  if (ext === '.webp') return 'webp';
  return 'png';
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
  const shot = await s.send('Page.captureScreenshot', params);
  const target = path.resolve(ctx.cwd, step.path);
  const bytes = Buffer.from(shot.data, 'base64');
  fs.mkdirSync(path.dirname(target), { recursive: true });
  fs.writeFileSync(target, bytes);
  return { op: 'screenshot', path: target, format, bytes: bytes.length, full: step.full === true, uid: step.uid ?? null };
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

function setDialogPolicy(ctx, action) {
  ctx.dialogPolicy = { accept: action === 'accept', promptText: '' };
  return { op: 'dialog', action };
}

async function runToolStep(ctx, step) {
  switch (step.op) {
    case 'snapshot': return takeSnapshot(ctx);
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
    case 'console': return consoleReport(ctx);
    case 'network': return networkReport(ctx, step.reqid);
    case 'dialog': return setDialogPolicy(ctx, step.action);
    case 'screenshot': return screenshotStep(ctx, step);
    case 'trace_start': return traceStart(ctx);
    case 'trace_stop': return traceStop(ctx, step.path || '');
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
      if (Date.now() >= deadline) throw new Error(`session is busy: another crawl holds ${lock}`);
      await delay(200);
    }
  }
}
