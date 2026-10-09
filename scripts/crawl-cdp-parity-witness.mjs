import { execFileSync, spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import zlib from 'node:zlib';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const options = Object.fromEntries(
  process.argv.slice(2).map((arg) => {
    const match = /^--([^=]+)(?:=(.*))?$/.exec(arg);
    return match ? [match[1], match[2] ?? 'true'] : [arg, 'true'];
  }),
);
const moduleDir = path.resolve(options['module-dir'] || path.join(here, '..', 'crates', 'agentplug-host', 'src'));
const leaseRoot = path.resolve(options.root || 'C:/dev/spoint');
const workDir = path.resolve(options['work-dir'] || fs.mkdtempSync(path.join(os.tmpdir(), 'cdp-parity-')));
const gmCli = path.join(os.homedir(), '.gm-tools', 'gm-mcp-server.mjs');
const agent = `cdp-parity-witness-${process.pid}`;
const sessionDir = path.join(workDir, 'sessions');
const EXPECTED_CHECKS = 38;
const outcomes = [];
const targetIds = new Set();

function check(name, ok, detail = '') {
  outcomes.push({ name, ok: ok === true });
  console.log(`CHECK ${name}: ${ok === true ? 'PASS' : 'FAIL'}${detail ? ` (${detail})` : ''}`);
}

function coerce(value) {
  if (value === 'true') return true;
  if (value === 'false') return false;
  if (/^-?\d+(\.\d+)?$/.test(value)) return Number(value);
  return value;
}

function parseDispatch(text) {
  const trimmed = text.trim();
  try {
    return JSON.parse(trimmed);
  } catch (_) {
    const reply = {};
    for (const line of trimmed.split('\n')) {
      const match = /^([a-z_]+): (.*)$/.exec(line.trim());
      if (match) reply[match[1]] = coerce(match[2]);
    }
    return reply;
  }
}

function gmDispatch(verb, body) {
  try {
    const stdout = execFileSync(
      process.execPath,
      [gmCli, 'dispatch', verb, '--body', JSON.stringify(body), '--cwd', leaseRoot],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], maxBuffer: 16 * 1024 * 1024, windowsHide: true },
    );
    return parseDispatch(stdout);
  } catch (error) {
    return { ok: false, error: String(error.message).split('\n')[0] };
  }
}

const helperSource = [
  fs.readFileSync(path.join(moduleDir, 'crawl_cdp_tools.mjs'), 'utf8'),
  fs.readFileSync(path.join(moduleDir, 'crawl_cdp.mjs'), 'utf8'),
].join('\n');

function runHelper(port, steps, session, extra = {}) {
  const config = {
    port,
    targetId: null,
    browserSession: false,
    cwd: workDir,
    pageTimeoutMs: 30000,
    textLimit: 20000,
    steps,
    ...extra,
  };
  if (session) config.session = { name: session, file: path.join(sessionDir, `${session}.json`) };
  return new Promise((resolve) => {
    const child = spawn(process.execPath, ['--input-type=module'], {
      env: { ...process.env, GM_CRAWL_CONFIG: JSON.stringify(config) },
      stdio: ['pipe', 'pipe', 'pipe'],
      windowsHide: true,
    });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    const killer = setTimeout(() => child.kill(), 100000);
    child.on('close', () => {
      clearTimeout(killer);
      const lastLine = stdout.trim().split('\n').pop() || '';
      let reply;
      try {
        reply = JSON.parse(lastLine);
      } catch (_) {
        reply = { ok: false, error: `no JSON reply from the helper: ${stderr.slice(-400)}` };
      }
      if (reply.targetId) targetIds.add(reply.targetId);
      resolve(reply);
    });
    child.stdin.end(helperSource);
  });
}

let heldLease = null;

function releaseHeld() {
  if (!heldLease) return;
  const { pid, startedAlive } = heldLease;
  heldLease = null;
  const released = gmDispatch('browser_lease_release', { root: leaseRoot, agent });
  if (!startedAlive && released.alive === true && Number(released.lease_count) === 0 && options['keep-browser'] !== 'true') {
    if (process.platform === 'win32') {
      spawnSync('taskkill', ['/PID', String(pid), '/T', '/F'], { windowsHide: true });
    } else {
      try { process.kill(pid); } catch (_) {}
    }
    console.log(`browser: closed the Chrome this witness started (pid ${pid})`);
  }
}

function onSignal() {
  releaseHeld();
  process.exit(130);
}

process.once('SIGINT', onSignal);
process.once('SIGTERM', onSignal);

function pageOf(reply, op) {
  return (reply.pages || []).filter((page) => page.op === op);
}

function evalValue(reply, index) {
  const evals = pageOf(reply, 'eval');
  return evals[index] ? evals[index].value : undefined;
}

function uidOf(text, roleAndName) {
  const pattern = roleAndName.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const match = new RegExp(`uid=(\\d+) ${pattern}`).exec(text || '');
  return match ? match[1] : null;
}

const PAGE = `<!doctype html>
<html><head><meta charset="utf-8"><title>Parity page</title></head>
<body>
<h1>Parity page</h1>
<button id="submit" onclick="window.clicks = (window.clicks || 0) + 1; console.log('submit clicked', window.clicks);">Submit</button>
<button id="alert" onclick="alert('parity alert'); window.alerted = true;">Alert</button>
<label>Name <input id="name" type="text" aria-label="Name" value=""></label>
<label><input id="agree" type="checkbox"> Agree</label>
<label>Colour <select id="colour" aria-label="Colour"><option value="red">Red</option><option value="blue">Blue</option></select></label>
<input id="upload" type="file" aria-label="Upload file">
<a href="/second">Second link</a>
<p id="status">loading</p>
<script>
console.log('parity page loaded');
console.warn('parity warning', 42);
fetch('/api/hello.json').then((r) => r.json()).then((j) => {
  document.getElementById('status').textContent = 'hello ' + j.msg;
  console.info('hello fetched');
});
setTimeout(() => {
  const p = document.createElement('p');
  p.textContent = 'late element';
  document.body.appendChild(p);
}, 200);
</script>
</body></html>`;

const server = http.createServer((req, res) => {
  if (req.url === '/') {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    res.end(PAGE);
  } else if (req.url === '/second') {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    res.end('<!doctype html><html><head><title>Second</title></head><body><h1>Second page</h1></body></html>');
  } else if (req.url === '/hang') {
    return;
  } else if (req.url === '/api/hello.json') {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end('{"msg":"world"}');
  } else {
    res.writeHead(404, { 'content-type': 'text/plain' });
    res.end('not found');
  }
});

async function main() {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  fs.mkdirSync(workDir, { recursive: true });
  fs.mkdirSync(sessionDir, { recursive: true });
  const base = `http://127.0.0.1:${server.address().port}`;
  const uploadFile = path.join(workDir, 'parity-upload.txt');
  fs.writeFileSync(uploadFile, 'parity upload body');

  const before = gmDispatch('browser_lease_status', { root: leaseRoot });
  const startedAlive = before.alive === true;
  const lease = gmDispatch('browser_lease_acquire', { root: leaseRoot, agent });
  if (lease.ok !== true) {
    console.log(`RESULT: FAIL shared browser lease could not be acquired: ${JSON.stringify(lease)}`);
    return 1;
  }
  console.log(`lease: port=${lease.port} chrome_pid=${lease.chrome_pid} started_by_witness=${!startedAlive}`);
  const port = Number(lease.port);
  heldLease = { pid: Number(lease.chrome_pid), startedAlive };
  try {
    const capture = await runHelper(port, [
      { op: 'goto', url: `${base}/` },
      { op: 'snapshot' },
      { op: 'wait_for', text: 'hello world' },
      { op: 'console' },
      { op: 'network' },
    ]);
    const snap = pageOf(capture, 'snapshot')[0] || {};
    const text = snap.text || '';
    check('capture.run_ok', capture.ok === true, capture.error || '');
    check('goto.title', (pageOf(capture, 'goto')[0] || {}).title === 'Parity page');
    check('snapshot.roles_and_names', ['button "Submit"', 'textbox "Name"', 'checkbox "Agree"', 'combobox "Colour"', 'button "Alert"'].every((p) => uidOf(text, p) !== null), text.slice(0, 200));
    check('snapshot.file_input_named', uidOf(text, 'button "Upload file"') !== null);
    const messages = pageOf(capture, 'console')[0]?.messages || [];
    const hasMessage = (type, needle) => messages.some((m) => m.type === type && m.text === needle);
    check('console.page_log', hasMessage('log', 'parity page loaded'), JSON.stringify(messages.map((m) => `${m.type}:${m.text}`)));
    check('console.page_warning', hasMessage('warning', 'parity warning 42'));
    check('console.page_info_after_fetch', hasMessage('info', 'hello fetched'));
    const requests = pageOf(capture, 'network')[0]?.requests || [];
    check('network.page_request', requests.some((r) => r.url.endsWith('/api/hello.json') && r.status === 200), JSON.stringify(requests.map((r) => `${r.status} ${r.url}`)));

    const reSnap = await runHelper(port, [
      { op: 'goto', url: `${base}/` },
      { op: 'snapshot' },
      { op: 'snapshot' },
    ]);
    const firstSubmit = uidOf(pageOf(reSnap, 'snapshot')[0]?.text, 'button "Submit"');
    const secondSubmit = uidOf(pageOf(reSnap, 'snapshot')[1]?.text, 'button "Submit"');
    check('uid.stable_across_resnapshot', firstSubmit !== null && firstSubmit === secondSubmit, `${firstSubmit} vs ${secondSubmit}`);
    const reClick = await runHelper(port, [
      { op: 'goto', url: `${base}/` },
      { op: 'snapshot' },
      { op: 'snapshot' },
      { op: 'click', uid: firstSubmit },
      { op: 'eval', code: 'window.clicks' },
    ]);
    check('uid.resolves_after_resnapshot', reClick.ok === true && evalValue(reClick, 0) === 1, reClick.error || String(evalValue(reClick, 0)));

    const clickSession = `parity-click-${process.pid}`;
    const c1 = await runHelper(port, [{ op: 'goto', url: `${base}/` }, { op: 'snapshot' }], clickSession);
    const uidSubmit = uidOf(pageOf(c1, 'snapshot')[0]?.text, 'button "Submit"');
    const uidAlert = uidOf(pageOf(c1, 'snapshot')[0]?.text, 'button "Alert"');
    check('session.first_call', c1.ok === true && uidSubmit !== null, c1.error || '');
    const c2 = await runHelper(port, [{ op: 'click', uid: uidSubmit }, { op: 'eval', code: 'window.clicks' }, { op: 'snapshot' }], clickSession);
    check('session.uid_from_earlier_call_clicks', c2.ok === true && evalValue(c2, 0) === 1, c2.error || String(evalValue(c2, 0)));
    check('session.same_tab_across_calls', c2.targetId === c1.targetId, `${c1.targetId} vs ${c2.targetId}`);
    check('session.uid_stable_across_calls', uidOf(pageOf(c2, 'snapshot')[0]?.text, 'button "Submit"') === uidSubmit);
    const c3 = await runHelper(port, [{ op: 'dblclick', uid: uidSubmit }, { op: 'eval', code: 'window.clicks' }, { op: 'hover', uid: uidSubmit }, { op: 'eval', code: 'window.clicks' }], clickSession);
    check('input.dblclick_adds_two', c3.ok === true && evalValue(c3, 0) === 3, c3.error || String(evalValue(c3, 0)));
    check('input.hover_is_inert', c3.ok === true && evalValue(c3, 1) === 3);
    const c4 = await runHelper(port, [{ op: 'click', uid: uidAlert }, { op: 'eval', code: 'window.alerted === true' }, { op: 'console' }], clickSession);
    const alertMessage = (pageOf(c4, 'console')[0]?.messages || []).find((m) => m.type === 'dialog');
    check('dialog.auto_accepted', c4.ok === true && evalValue(c4, 0) === true, c4.error || '');
    check('dialog.recorded_in_console', alertMessage !== undefined && alertMessage.text === 'alert: parity alert', JSON.stringify(alertMessage));
    check('console.replays_page_messages', (pageOf(c4, 'console')[0]?.messages || []).some((m) => m.text === 'submit clicked 1' || m.text === 'parity page loaded'), 'page messages from the earlier call');
    const shots = await runHelper(port, [
      { op: 'screenshot', path: path.join(workDir, 'viewport.png') },
      { op: 'screenshot', path: path.join(workDir, 'full.png'), full: true },
      { op: 'screenshot', uid: uidSubmit, path: path.join(workDir, 'button.png') },
    ], clickSession);
    const pngOk = (file) => {
      if (!fs.existsSync(file)) return false;
      const bytes = fs.readFileSync(file);
      return bytes.length > 100 && bytes.subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]));
    };
    check('screenshot.viewport_png', shots.ok === true && pngOk(path.join(workDir, 'viewport.png')), shots.error || '');
    check('screenshot.full_page_png', pngOk(path.join(workDir, 'full.png')));
    check('screenshot.element_png', pngOk(path.join(workDir, 'button.png')));
    const traceRun = await runHelper(port, [
      { op: 'trace_start' },
      { op: 'click', uid: uidSubmit },
      { op: 'trace_stop', path: path.join(workDir, 'trace.json.gz') },
    ], clickSession);
    const traceStep = pageOf(traceRun, 'trace_stop')[0] || {};
    check('trace.summary', traceRun.ok === true && traceStep.eventCount > 0 && traceStep.summary && traceStep.summary.durationMs > 0 && traceStep.summary.durationMs < 60000, traceRun.error || JSON.stringify(traceStep.summary || {}));
    const traceFile = path.join(workDir, 'trace.json.gz');
    const traceJson = fs.existsSync(traceFile) ? JSON.parse(zlib.gunzipSync(fs.readFileSync(traceFile)).toString('utf8')) : {};
    check('trace.file_has_events', Array.isArray(traceJson.traceEvents) && traceJson.traceEvents.length > 0);

    const formSession = `parity-form-${process.pid}`;
    const f1 = await runHelper(port, [{ op: 'goto', url: `${base}/` }, { op: 'snapshot' }], formSession);
    const fText = pageOf(f1, 'snapshot')[0]?.text;
    const uidName = uidOf(fText, 'textbox "Name"');
    const uidColour = uidOf(fText, 'combobox "Colour"');
    const uidAgree = uidOf(fText, 'checkbox "Agree"');
    const uidFile = uidOf(fText, 'button "Upload file"');
    const f2 = await runHelper(port, [
      { op: 'fill', uid: uidName, value: 'Ada Lovelace' },
      { op: 'eval', code: "document.getElementById('name').value" },
      { op: 'press', key: 'Backspace' },
      { op: 'type', text: '!' },
      { op: 'eval', code: "document.getElementById('name').value" },
      { op: 'fill', uid: uidColour, value: 'blue' },
      { op: 'eval', code: "document.getElementById('colour').value" },
      { op: 'fill', uid: uidAgree, value: 'true' },
      { op: 'eval', code: "document.getElementById('agree').checked" },
    ], formSession);
    check('fill.textbox_replaces_value', f2.ok === true && evalValue(f2, 0) === 'Ada Lovelace', f2.error || String(evalValue(f2, 0)));
    check('press.backspace_then_type', evalValue(f2, 1) === 'Ada Lovelac!', String(evalValue(f2, 1)));
    check('fill.select_by_option_value', evalValue(f2, 2) === 'blue', String(evalValue(f2, 2)));
    check('fill.checkbox_true', evalValue(f2, 3) === true);
    const f3 = await runHelper(port, [
      { op: 'upload', uid: uidFile, paths: [uploadFile] },
      { op: 'eval', code: "document.getElementById('upload').files[0].name" },
    ], formSession);
    check('upload.file_input_receives_file', f3.ok === true && evalValue(f3, 0) === 'parity-upload.txt', f3.error || String(evalValue(f3, 0)));
    const f4 = await runHelper(port, [
      { op: 'press', uid: uidName, key: 'End' },
      { op: 'type', text: 'X' },
      { op: 'eval', code: "document.getElementById('name').value" },
    ], formSession);
    check('press.by_uid_focuses_the_element', f4.ok === true && evalValue(f4, 0) === 'Ada Lovelac!X', f4.error || String(evalValue(f4, 0)));

    const nav = await runHelper(port, [
      { op: 'goto', url: `${base}/` },
      { op: 'goto', url: `${base}/second` },
      { op: 'eval', code: 'location.pathname' },
      { op: 'back' },
      { op: 'eval', code: 'location.pathname' },
      { op: 'forward' },
      { op: 'eval', code: 'location.pathname' },
      { op: 'reload' },
      { op: 'wait_for', text: 'Second page' },
    ]);
    check('nav.url_back_forward', nav.ok === true && evalValue(nav, 0) === '/second' && evalValue(nav, 1) === '/' && evalValue(nav, 2) === '/second', nav.error || JSON.stringify(pageOf(nav, 'eval').map((p) => p.value)));
    check('nav.reload_and_wait_for', pageOf(nav, 'wait_for')[0]?.found === true && pageOf(nav, 'reload')[0]?.title === 'Second');

    const net = await runHelper(port, [
      { op: 'goto', url: `${base}/` },
      { op: 'wait_for', text: 'hello world' },
      { op: 'network' },
      { op: 'network', reqid: 2 },
    ]);
    const detail = pageOf(net, 'network').find((page) => page.request);
    check('network.request_details', net.ok === true && detail && detail.request.url.endsWith('/api/hello.json') && (detail.request.body || {}).text === '{"msg":"world"}', JSON.stringify(detail && detail.request ? detail.request.url : net.error));

    const deadlineRun = await runHelper(port, [{ op: 'goto', url: `${base}/hang` }], null, { pageTimeoutMs: 3000 });
    check('res.navigation_deadline', deadlineRun.ok === false && /did not finish within 3000ms/.test(deadlineRun.error || ''), deadlineRun.error || '');
    const lockName = `parity-lock-${process.pid}`;
    const lockFile = path.join(sessionDir, `${lockName}.json`);
    fs.writeFileSync(`${lockFile}.lock`, '999999');
    const busy = await runHelper(port, [{ op: 'eval', code: '1' }], lockName, { lockWaitMs: 1500 });
    check('conc.busy_session_refused', busy.ok === false && /session is busy/.test(busy.error || ''), busy.error || '');
    const tenMinutesAgo = new Date(Date.now() - 600000);
    fs.utimesSync(`${lockFile}.lock`, tenMinutesAgo, tenMinutesAgo);
    const stale = await runHelper(port, [{ op: 'eval', code: '1' }], lockName, { lockWaitMs: 1500 });
    check('conc.stale_lock_recovered', stale.ok === true && !fs.existsSync(`${lockFile}.lock`), stale.error || '');
    fs.writeFileSync(lockFile, '{not json');
    const corrupt = await runHelper(port, [{ op: 'eval', code: '1' }], lockName);
    check('state.corrupt_session_refused', corrupt.ok === false && /not valid JSON/.test(corrupt.error || '') && fs.readFileSync(lockFile, 'utf8') === '{not json', corrupt.error || '');
    fs.rmSync(lockFile, { force: true });

    const errorRun = await runHelper(port, [{ op: 'goto', url: `${base}/` }, { op: 'click', uid: '99999' }]);
    check('error.unknown_uid_reports', errorRun.ok === false && /not in this session/.test(errorRun.error || ''), errorRun.error || '');
  } finally {
    await Promise.all([...targetIds].map((id) => fetch(`http://127.0.0.1:${port}/json/close/${id}`, { signal: AbortSignal.timeout(3000) }).catch(() => {})));
    releaseHeld();
    server.closeAllConnections();
    server.close();
  }

  const passed = outcomes.filter((o) => o.ok).length;
  const failed = outcomes.filter((o) => !o.ok).map((o) => o.name);
  console.log(`checks: ${passed}/${outcomes.length} passed`);
  const ok = outcomes.length >= EXPECTED_CHECKS && failed.length === 0;
  console.log(ok ? 'RESULT: PASS' : `RESULT: FAIL ${failed.length ? `failed: ${failed.join(', ')}` : `only ${outcomes.length} checks ran`}`);
  return ok ? 0 : 1;
}

main().then((code) => process.exit(code), (error) => {
  console.log(`RESULT: FAIL witness error: ${error.message}`);
  process.exit(1);
});
