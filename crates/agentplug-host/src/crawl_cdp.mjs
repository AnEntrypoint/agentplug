// CDP crawl helper. The host concatenates crawl_cdp_tools.mjs ahead of this file and
// runs the result as `node --input-type=module` on stdin. Config arrives as JSON in the
// GM_CRAWL_CONFIG environment variable:
//   { port, targetId?, browserSession?, cwd?, session?: {name, file}, steps, pageTimeoutMs, textLimit }
// browserSession selects the lightpanda protocol: targets are created and attached over
// the browser websocket, because lightpanda serves no HTTP target endpoints.
// A session keeps its tab and its uid map across calls; its file is written atomically.
// It prints exactly one JSON object on stdout and exits 0 when every step ran.

const cfg = JSON.parse(process.env.GM_CRAWL_CONFIG || '{}');
const endpoint = `http://127.0.0.1:${cfg.port}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function getJson(path, method = 'GET') {
  const res = await fetch(endpoint + path, { method, signal: AbortSignal.timeout(3000) });
  if (!res.ok) throw new Error(`${method} ${path} answered HTTP ${res.status}`);
  return res.json();
}

async function acquireTarget(targetId) {
  if (targetId) {
    const list = await getJson('/json/list');
    const same = Array.isArray(list) && list.find((t) => t.id === targetId && t.webSocketDebuggerUrl);
    if (same) return same;
  }
  const created = await getJson('/json/new?about:blank', 'PUT');
  if (!created.webSocketDebuggerUrl) throw new Error('the endpoint created a target without webSocketDebuggerUrl');
  return created;
}

async function attachBrowserTarget(browser) {
  if (cfg.targetId) {
    try {
      const attached = await browser.send('Target.attachToTarget', { targetId: cfg.targetId, flatten: true });
      return { targetId: cfg.targetId, sessionId: attached.sessionId };
    } catch (_) {}
  }
  const created = await browser.send('Target.createTarget', { url: 'about:blank' });
  const attached = await browser.send('Target.attachToTarget', { targetId: created.targetId, flatten: true });
  return { targetId: created.targetId, sessionId: attached.sessionId };
}

function openSession(wsUrl, timeoutMs) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(wsUrl);
    const pending = new Map();
    const listeners = new Map();
    let nextId = 1;
    const timer = setTimeout(() => {
      try { ws.close(); } catch (_) {}
      reject(new Error('cdp websocket did not open in time'));
    }, timeoutMs);
    const failAll = (reason) => {
      for (const { rej } of pending.values()) rej(new Error(reason));
      pending.clear();
    };
    const session = {
      send(method, params = {}, sessionId) {
        const id = nextId++;
        return new Promise((res, rej) => {
          pending.set(id, { res, rej });
          const message = { id, method, params };
          if (sessionId) message.sessionId = sessionId;
          ws.send(JSON.stringify(message));
        });
      },
      on(method, handler) {
        listeners.set(method, [...(listeners.get(method) || []), handler]);
      },
      close() { try { ws.close(); } catch (_) {} },
    };
    ws.addEventListener('open', () => { clearTimeout(timer); resolve(session); });
    ws.addEventListener('message', (ev) => {
      let msg;
      try { msg = JSON.parse(ev.data); } catch (_) { return; }
      if (msg.method) {
        for (const handler of listeners.get(msg.method) || []) {
          try {
            handler(msg.params || {}, msg.sessionId);
          } catch (error) {
            listenerFaults.push(`${msg.method}: ${error.message}`);
          }
        }
        return;
      }
      if (!msg.id || !pending.has(msg.id)) return;
      const { res, rej } = pending.get(msg.id);
      pending.delete(msg.id);
      if (msg.error) rej(new Error(msg.error.message || 'cdp error'));
      else res(msg.result);
    });
    ws.addEventListener('error', () => {
      clearTimeout(timer);
      failAll('cdp websocket error');
      reject(new Error('cdp websocket error'));
    });
    ws.addEventListener('close', () => {
      clearTimeout(timer);
      failAll('cdp websocket closed');
      reject(new Error('cdp websocket closed before it opened'));
    });
  });
}

async function evaluate(session, expression) {
  const r = await session.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true, userGesture: true });
  if (r.exceptionDetails) {
    const ex = r.exceptionDetails.exception;
    throw new Error(`evaluation threw: ${(ex && ex.description) || r.exceptionDetails.text}`);
  }
  return r.result && r.result.value !== undefined ? r.result.value : null;
}

async function waitForComplete(session, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const state = await evaluate(session, 'document.readyState');
    if (state === 'complete') return;
    if (Date.now() >= deadline) throw new Error(`page did not reach readyState complete within ${timeoutMs}ms (last state: ${state})`);
    await sleep(100);
  }
}

async function gotoStep(session, url) {
  await session.send('Page.enable');
  const nav = await withDeadline(session.send('Page.navigate', { url }), cfg.pageTimeoutMs || 30000, `navigation to ${url}`);
  if (nav.errorText) throw new Error(`navigation to ${url} failed: ${nav.errorText}`);
  await waitForComplete(session, cfg.pageTimeoutMs || 30000);
  const limit = cfg.textLimit || 20000;
  const page = await evaluate(session, `({
    url: location.href,
    title: document.title,
    text: (document.body ? document.body.innerText : '').slice(0, ${limit})
  })`);
  return { op: 'goto', url: page.url, title: page.title, text: page.text };
}

async function main() {
  const started = Date.now();
  const out = { ok: false, targetId: null, pages: [], error: null };
  const watchdog = cfg.watchdogMs
    ? setTimeout(() => {
      out.ok = false;
      out.error = out.error || `crawl helper ran past its ${cfg.watchdogMs}ms budget`;
      process.stdout.write(JSON.stringify(out) + '\n');
      process.exit(1);
    }, cfg.watchdogMs)
    : null;
  const steps = cfg.steps || [];
  let session = null;
  let close = () => {};
  let ctx = null;
  let unlock = () => {};
  let persisted = null;
  try {
    if (cfg.session) {
      unlock = await lockSession(cfg.session.file, cfg.lockWaitMs || SESSION_LOCK_WAIT_MS);
      persisted = readSessionFile(cfg.session.file);
    }
    if (cfg.browserSession) {
      const info = await getJson('/json/version');
      const browser = await openSession(info.webSocketDebuggerUrl, 5000);
      close = () => browser.close();
      const attached = await attachBrowserTarget(browser);
      out.targetId = attached.targetId;
      session = {
        send: (method, params) => browser.send(method, params, attached.sessionId),
        on: (method, handler) => browser.on(method, (params, sessionId) => {
          if (sessionId === attached.sessionId) handler(params);
        }),
      };
    } else {
      const target = await acquireTarget((persisted && persisted.targetId) || cfg.targetId);
      out.targetId = target.id;
      const page = await openSession(target.webSocketDebuggerUrl, 5000);
      close = () => page.close();
      session = page;
    }
    ctx = createToolContext({
      session,
      host: { evaluate, waitForComplete, sleep },
      cwd: cfg.cwd || process.cwd(),
      timeoutMs: cfg.pageTimeoutMs || 30000,
      textLimit: cfg.textLimit || 20000,
    });
    if (persisted) {
      if (persisted.targetId === out.targetId) restoreUidState(ctx, persisted);
      else ctx.nextUid = persisted.nextUid || 0;
    }
    await attachCapture(ctx, steps);
    for (const step of steps) {
      if (step.op === 'goto') {
        out.pages.push(await gotoStep(session, step.url));
      } else if (step.op === 'wait') {
        await sleep(step.ms);
        out.pages.push({ op: 'wait', ms: step.ms });
      } else if (step.op === 'eval') {
        const value = await evaluate(session, step.code);
        out.pages.push({ op: 'eval', code: step.code, value });
      } else if (TOOL_OPS.has(step.op)) {
        out.pages.push(await runToolStep(ctx, step));
      } else {
        throw new Error(`unknown crawl step op '${step.op}'`);
      }
    }
    out.ok = true;
  } catch (e) {
    out.error = String((e && e.message) || e);
  } finally {
    if (cfg.session && ctx && out.targetId) {
      try {
        writeSessionFile(cfg.session.file, { version: SESSION_VERSION, targetId: out.targetId, ...snapshotUidState(ctx) });
      } catch (e) {
        out.ok = false;
        out.error = out.error || `session state was not saved: ${e.message}`;
      }
    }
    close();
    unlock();
    if (!cfg.browserSession && !cfg.session && out.targetId) {
      await fetch(`${endpoint}/json/close/${out.targetId}`, { signal: AbortSignal.timeout(3000) }).catch(() => {});
    }
  }
  if (watchdog) clearTimeout(watchdog);
  if (listenerFaults.length) out.listenerFaults = listenerFaults;
  out.duration_ms = Date.now() - started;
  process.stdout.write(JSON.stringify(out) + '\n');
  process.exitCode = out.ok ? 0 : 1;
}

main();
