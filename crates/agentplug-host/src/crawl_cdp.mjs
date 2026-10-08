// CDP crawl helper. Runs as `node --input-type=module` with this file on stdin.
// Config arrives as JSON in the GM_CRAWL_CONFIG environment variable:
//   { port, targetId?, browserSession?, steps: [{op:"goto",url}|{op:"wait",ms}|{op:"eval",code}],
//     pageTimeoutMs, textLimit }
// browserSession selects the lightpanda protocol: targets are created and attached over
// the browser websocket, because lightpanda serves no HTTP target endpoints.
// It prints exactly one JSON object on stdout and exits 0 when every step ran.

const cfg = JSON.parse(process.env.GM_CRAWL_CONFIG || '{}');
const endpoint = `http://127.0.0.1:${cfg.port}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function getJson(path, method = 'GET') {
  const res = await fetch(endpoint + path, { method, signal: AbortSignal.timeout(3000) });
  if (!res.ok) throw new Error(`${method} ${path} answered HTTP ${res.status}`);
  return res.json();
}

async function acquireTarget() {
  if (cfg.targetId) {
    const list = await getJson('/json/list');
    const same = Array.isArray(list) && list.find((t) => t.id === cfg.targetId && t.webSocketDebuggerUrl);
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
      close() { try { ws.close(); } catch (_) {} },
    };
    ws.addEventListener('open', () => { clearTimeout(timer); resolve(session); });
    ws.addEventListener('message', (ev) => {
      let msg;
      try { msg = JSON.parse(ev.data); } catch (_) { return; }
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
  const nav = await session.send('Page.navigate', { url });
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
  let session = null;
  let close = () => {};
  try {
    if (cfg.browserSession) {
      const info = await getJson('/json/version');
      const browser = await openSession(info.webSocketDebuggerUrl, 5000);
      close = () => browser.close();
      const attached = await attachBrowserTarget(browser);
      out.targetId = attached.targetId;
      session = { send: (method, params) => browser.send(method, params, attached.sessionId) };
    } else {
      const target = await acquireTarget();
      out.targetId = target.id;
      const page = await openSession(target.webSocketDebuggerUrl, 5000);
      close = () => page.close();
      session = page;
    }
    for (const step of cfg.steps || []) {
      if (step.op === 'goto') {
        out.pages.push(await gotoStep(session, step.url));
      } else if (step.op === 'wait') {
        await sleep(step.ms);
        out.pages.push({ op: 'wait', ms: step.ms });
      } else if (step.op === 'eval') {
        const value = await evaluate(session, step.code);
        out.pages.push({ op: 'eval', code: step.code, value });
      } else {
        throw new Error(`unknown crawl step op '${step.op}'`);
      }
    }
    out.ok = true;
  } catch (e) {
    out.error = String((e && e.message) || e);
  } finally {
    close();
  }
  out.duration_ms = Date.now() - started;
  process.stdout.write(JSON.stringify(out) + '\n');
  process.exitCode = out.ok ? 0 : 1;
}

main();
