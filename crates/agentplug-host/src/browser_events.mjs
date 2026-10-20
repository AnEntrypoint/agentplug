import fs from 'node:fs';
import path from 'node:path';

// Live feed: one long-running capture on the project's shared browser (the browser lease is held by the host
// hook that spawns this helper). Console, exception, log, network and page-load events are appended to the
// spool as JSON lines. Every line carries a sequence number that only grows, so a reader resumes from the
// last seq it saw. Config comes from GM_BROWSER_EVENTS_CONFIG: {port, session, spool, url?, maxMs?}.
const cfg = JSON.parse(process.env.GM_BROWSER_EVENTS_CONFIG || '{}');
const endpoint = `http://127.0.0.1:${cfg.port}`;
const spool = cfg.spool;
const stopFile = `${spool}.stop`;
const statusFile = `${spool}.status.json`;
const lockFile = `${spool}.lock`;
const maxMs = Number(cfg.maxMs) > 0 ? Number(cfg.maxMs) : 900000;
const TEXT_CAP = 4000;
const startedAt = Date.now();

function cap(text) {
  const s = String(text ?? '');
  return s.length > TEXT_CAP ? `${s.slice(0, TEXT_CAP)}...` : s;
}

// The last sequence number in the spool, read from the final parseable line; a torn trailing line is skipped.
function lastSeq() {
  let lines;
  try {
    lines = fs.readFileSync(spool, 'utf8').split('\n');
  } catch (_) {
    return 0;
  }
  for (let i = lines.length - 1; i >= 0; i--) {
    if (!lines[i].trim()) continue;
    try {
      const seq = JSON.parse(lines[i]).seq;
      if (Number.isInteger(seq)) return seq;
    } catch (_) {
      // torn line from an earlier crash: keep scanning backwards
    }
  }
  return 0;
}

let seq = 0;
function emit(kind, data) {
  seq += 1;
  fs.appendFileSync(spool, `${JSON.stringify({ seq, ts: Date.now(), session: cfg.session, kind, ...data })}\n`);
}

// Atomic replace: write a temp name, then rename over the target.
function writeAtomic(target, text) {
  const tmp = `${target}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, text);
  fs.renameSync(tmp, target);
}

let finishing = false;
let finishReason = '';
function finish(reason) {
  if (finishing) return;
  finishing = true;
  finishReason = reason;
}

function writeStatus(extra = {}) {
  writeAtomic(
    statusFile,
    `${JSON.stringify({
      session: cfg.session,
      pid: process.pid,
      startedAt,
      heartbeatAt: Date.now(),
      lastSeq: seq,
      maxMs,
      ...extra,
    })}\n`,
  );
}

async function getJson(p, method = 'GET') {
  const res = await fetch(endpoint + p, { method, signal: AbortSignal.timeout(3000) });
  if (!res.ok) throw new Error(`${method} ${p} answered HTTP ${res.status}`);
  return res.json();
}

// The first open page, else a new one. The browser verb shares the same tab through the same lease.
async function pageTarget() {
  const list = await getJson('/json/list');
  const page = Array.isArray(list) && list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl);
  if (page) return page;
  const created = await getJson('/json/new?about:blank', 'PUT');
  if (!created.webSocketDebuggerUrl) throw new Error('the endpoint created a target without webSocketDebuggerUrl');
  return created;
}

function connect(wsUrl) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(wsUrl);
    let nextId = 0;
    const pending = new Map();
    const listeners = [];
    const session = {
      send(method, params = {}) {
        const id = ++nextId;
        return new Promise((res, rej) => {
          pending.set(id, { res, rej });
          ws.send(JSON.stringify({ id, method, params }));
        });
      },
      on(fn) {
        listeners.push(fn);
      },
      close() {
        try {
          ws.close();
        } catch (_) {
          // already closed
        }
      },
    };
    ws.onopen = () => resolve(session);
    ws.onerror = () => reject(new Error('websocket to the page failed'));
    ws.onmessage = (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id && pending.has(msg.id)) {
        const p = pending.get(msg.id);
        pending.delete(msg.id);
        if (msg.error) p.rej(new Error(msg.error.message));
        else p.res(msg.result);
        return;
      }
      for (const fn of listeners) fn(msg.method, msg.params || {});
    };
    ws.onclose = () => {
      for (const p of pending.values()) p.rej(new Error('page websocket closed'));
      pending.clear();
      finish('page websocket closed');
    };
  });
}

function describeArgs(args = []) {
  return args
    .map((a) => {
      if (a.value !== undefined) return typeof a.value === 'string' ? a.value : JSON.stringify(a.value);
      return a.description || a.unserializableValue || a.type || '';
    })
    .join(' ');
}

function topFrame(stackTrace) {
  const f = stackTrace && stackTrace.callFrames && stackTrace.callFrames[0];
  return f ? { url: f.url || '', line: f.lineNumber ?? null } : { url: '', line: null };
}

function onEvent(method, p) {
  switch (method) {
    case 'Runtime.consoleAPICalled': {
      const frame = topFrame(p.stackTrace);
      return emit('console', { type: p.type, text: cap(describeArgs(p.args)), url: frame.url, line: frame.line });
    }
    case 'Runtime.exceptionThrown': {
      const d = p.exceptionDetails || {};
      return emit('exception', {
        type: 'error',
        text: cap((d.exception && d.exception.description) || d.text),
        url: d.url || '',
        line: d.lineNumber ?? null,
      });
    }
    case 'Log.entryAdded': {
      const e = p.entry || {};
      return emit('log', { type: e.level, text: cap(e.text), url: e.url || '', line: e.lineNumber ?? null, source: e.source });
    }
    case 'Network.requestWillBeSent':
      return emit('network', {
        phase: 'request',
        reqid: p.requestId,
        method: p.request && p.request.method,
        url: p.request && p.request.url,
        type: p.type || '',
      });
    case 'Network.responseReceived':
      return emit('network', {
        phase: 'response',
        reqid: p.requestId,
        status: p.response && p.response.status,
        url: p.response && p.response.url,
        type: p.type || '',
      });
    case 'Network.loadingFinished':
      return emit('network', { phase: 'finished', reqid: p.requestId, bytes: p.encodedDataLength });
    case 'Network.loadingFailed':
      return emit('network', { phase: 'failed', reqid: p.requestId, error: p.errorText, type: p.type || '' });
    case 'Page.loadEventFired':
      return emit('page', { phase: 'load' });
    case 'Page.frameNavigated':
      if (p.frame && !p.frame.parentId) return emit('page', { phase: 'navigated', url: p.frame.url });
      return undefined;
    default:
      return undefined;
  }
}

async function main() {
  fs.mkdirSync(path.dirname(spool), { recursive: true });
  try {
    fs.writeFileSync(lockFile, `${JSON.stringify({ pid: process.pid, session: cfg.session, startedAt })}\n`, { flag: 'wx' });
  } catch (e) {
    if (e.code === 'EEXIST') {
      console.error('a live feed already holds this spool');
      process.exit(3);
    }
    throw e;
  }
  seq = lastSeq();
  try {
    fs.unlinkSync(stopFile);
  } catch (_) {
    // no stale stop marker
  }
  const target = await pageTarget();
  const session = await connect(target.webSocketDebuggerUrl);
  session.on(onEvent);
  for (const domain of ['Page', 'Runtime', 'Log', 'Network']) await session.send(`${domain}.enable`);
  emit('feed', { phase: 'attached', targetId: target.id });
  if (cfg.url) await session.send('Page.navigate', { url: cfg.url });
  writeStatus({ state: 'running', targetId: target.id });

  const deadline = startedAt + maxMs;
  while (!finishing) {
    await new Promise((resolve) => setTimeout(resolve, 500));
    if (fs.existsSync(stopFile)) finish('stop requested');
    else if (Date.now() >= deadline) finish('max time reached');
    else writeStatus({ state: 'running', targetId: target.id });
  }
  session.close();
  emit('feed', { phase: 'stopped', reason: finishReason });
  writeStatus({ state: 'stopped', reason: finishReason });
  try {
    fs.unlinkSync(lockFile);
  } catch (_) {
    // already removed by the host
  }
}

main().catch((e) => {
  try {
    fs.appendFileSync(spool, `${JSON.stringify({ seq: seq + 1, ts: Date.now(), session: cfg.session, kind: 'feed', phase: 'error', text: String(e && e.message) })}\n`);
  } catch (_) {
    // spool unwritable: nothing else to record
  }
  try {
    fs.unlinkSync(lockFile);
  } catch (_) {
    // lock not ours or already gone
  }
  process.exit(1);
});
