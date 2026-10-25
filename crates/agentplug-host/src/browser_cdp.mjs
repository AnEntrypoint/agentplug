const cfg =JSON.parse(process.env.GM_BROWSER_CONFIG || '{}');
const endpoint = `http://127.0.0.1:${cfg.port}`;
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
// One deadline for the whole helper run: every CDP command, HTTP probe and lock wait is capped by what is left of it.
const deadlineAt = Number.isFinite(cfg.budgetMs) ? Date.now() + cfg.budgetMs : Infinity;
const remainingMs = () => Math.max(0, deadlineAt - Date.now());
const budgetCap = (ms) => Math.max(1, Math.min(ms, remainingMs()));

// process.cwd() throws ENOENT when the spawning project directory was removed
// while the helper ran; fall back to the home directory so the helper still starts.
function safeCwd() {
  try {
    return process.cwd();
  } catch (_) {
    return process.env.HOME || process.env.USERPROFILE || '/';
  }
}

async function getJson(path, method = 'GET') {
  const res = await fetch(endpoint + path, { method, signal: AbortSignal.timeout(budgetCap(3000)) });
  if (!res.ok) throw new Error(`${method} ${path} answered HTTP ${res.status}`);
  return res.json();
}

// One browser per project shares one tab: the persisted session tab if it is still open, else any open page
// that is not an isolated session's page. A new tab is created only when the browser has no shared page at all.
async function acquireTarget(targetId, isolatedIds) {
  const list = await getJson('/json/list');
  if (Array.isArray(list)) {
    const same = targetId && list.find((t) => t.id === targetId && t.webSocketDebuggerUrl);
    if (same) return same;
    const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl && !isolatedIds.has(t.id));
    if (page) return page;
  }
  const created = await getJson('/json/new?about:blank', 'PUT');
  if (!created.webSocketDebuggerUrl) throw new Error('the endpoint created a target without webSocketDebuggerUrl');
  return { ...created, created: true };
}

function isolatedTargetIds(dir) {
  const ids = new Set();
  let names = [];
  try { names = fs.readdirSync(dir); } catch (_) { return ids; }
  for (const name of names) {
    if (!name.endsWith('.json')) continue;
    try {
      const state = JSON.parse(fs.readFileSync(path.join(dir, name), 'utf8'));
      if (state.isolated && state.targetId) ids.add(state.targetId);
    } catch (_) {}
  }
  return ids;
}

async function createIsolatedPage(browser, contextId) {
  if (contextId) {
    try {
      const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank', browserContextId: contextId });
      return { browserContextId: contextId, targetId };
    } catch (_) {}
  }
  const { browserContextId } = await browser.send('Target.createBrowserContext', { disposeOnDetach: false });
  const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank', browserContextId });
  return { browserContextId, targetId };
}

async function awaitPageTarget(targetId) {
  for (let attempt = 0; attempt < 50; attempt++) {
    if (remainingMs() <= 0) throw new Error(`call budget exhausted while waiting for target ${targetId}`);
    const list = await getJson('/json/list');
    const found = Array.isArray(list) && list.find((t) => t.id === targetId && t.webSocketDebuggerUrl);
    if (found) return found;
    await sleep(100);
  }
  throw new Error(`target ${targetId} did not appear in the endpoint target list`);
}

// An isolated session owns a page in its own browser context (separate cookies and localStorage). It reuses
// that page while it is open, otherwise opens a new page in the same context, or in a new context.
async function acquireIsolatedTarget(persisted) {
  if (persisted && persisted.isolated && persisted.targetId) {
    const list = await getJson('/json/list');
    const same = Array.isArray(list) && list.find((t) => t.id === persisted.targetId && t.webSocketDebuggerUrl);
    if (same) return { ...same, created: false, isolated: true, browserContextId: persisted.browserContextId || null };
  }
  const version = await getJson('/json/version');
  if (!version.webSocketDebuggerUrl) throw new Error('the endpoint has no browser websocket for isolated pages');
  const browser = await openSession(version.webSocketDebuggerUrl, budgetCap(5000));
  try {
    const page = await createIsolatedPage(browser, persisted && persisted.isolated ? persisted.browserContextId : null);
    const target = await awaitPageTarget(page.targetId);
    return { ...target, created: true, isolated: true, browserContextId: page.browserContextId };
  } finally {
    browser.close();
  }
}

async function closeHttpTarget(id) {
  await fetch(`${endpoint}/json/close/${id}`, { signal: AbortSignal.timeout(3000) }).then((r) => r.text()).catch(() => {});
}

function openSession(wsUrl, timeoutMs) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(wsUrl);
    const pending = new Map();
    const listeners = new Map();
    let nextId = 1;
    let closed = false;
    const timer = setTimeout(() => {
      try { ws.close(); } catch (_) {}
      reject(new Error('cdp websocket did not open in time'));
    }, timeoutMs);
    const failAll = (reason) => {
      for (const { rej } of pending.values()) rej(new Error(reason));
      pending.clear();
    };
    const session = {
      // A command with no answer fails when the call's budget runs out, so no step can outlive the call.
      send(method, params = {}, sessionId) {
        const id = nextId++;
        return new Promise((res, rej) => {
          if (closed) return rej(new Error(`cdp websocket closed before ${method} was sent`));
          const ms = remainingMs();
          if (ms <= 0) return rej(new Error(`call budget exhausted before ${method} was sent`));
          const commandTimer = Number.isFinite(ms) ? setTimeout(() => {
            pending.delete(id);
            rej(new Error(`${method} got no response within the call budget (${cfg.budgetMs}ms)`));
          }, ms) : null;
          pending.set(id, {
            res: (value) => { clearTimeout(commandTimer); res(value); },
            rej: (error) => { clearTimeout(commandTimer); rej(error); },
          });
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
      closed = true;
      clearTimeout(timer);
      failAll('cdp websocket error');
      reject(new Error('cdp websocket error'));
    });
    ws.addEventListener('close', () => {
      closed = true;
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

async function forgetSessionFile(file) {
  try {
    const { rmSync } = await import('node:fs');
    rmSync(file, { force: true });
    return null;
  } catch (e) {
    return String((e && e.message) || e);
  }
}

async function currentUrl(session) {
  try {
    return await evaluate(session, 'location.href');
  } catch (e) {
    return `url unreadable: ${e.message}`;
  }
}

// The watchdog must not block on a page stuck in a busy loop: the cleanup URL read gets this bound, then null.
const CLEANUP_URL_DEADLINE_MS = 2000;

// Written after every completed step so a helper killed by the Rust budget still leaves its steps behind.
// Atomic (temp then rename); best effort, the final stdout line stays the authoritative reply.
function writePartial(out) {
  if (!cfg.partialFile) return;
  try {
    fs.mkdirSync(path.dirname(cfg.partialFile), { recursive: true });
    const tmp = `${cfg.partialFile}.tmp`;
    fs.writeFileSync(tmp, JSON.stringify(out));
    fs.renameSync(tmp, cfg.partialFile);
  } catch (e) {
    out.partial_error = e.message;
  }
}

// Every failure carries text: a thrown value with no message is named by what it is, never left empty.
function errorText(e) {
  const text = e && e.message ? String(e.message) : String(e);
  return text.trim() || 'browser helper failed without an error message';
}

function sameUrl(a, b) {
  try {
    return new URL(a).href === new URL(b).href;
  } catch (_) {
    return a === b;
  }
}

// A url= step navigates only when the tab is not already on that URL. Reloading is an explicit
// reload step, so an unchanged page is never refreshed by a call that did not ask for it.
async function gotoStep(session, url) {
  await session.send('Page.enable');
  const before = await currentUrl(session);
  const navigated = !before || !sameUrl(before, url);
  if (navigated) {
    const nav = await withDeadline(session.send('Page.navigate', { url }), cfg.pageTimeoutMs || 30000, `navigation to ${url}`);
    if (nav.errorText) throw new Error(`navigation to ${url} failed: ${nav.errorText}`);
    await waitForComplete(session, cfg.pageTimeoutMs || 30000);
  }
  const limit = cfg.textLimit || 20000;
  const page = await evaluate(session, `({
    url: location.href,
    title: document.title,
    text: (document.body ? document.body.innerText : '').slice(0, ${limit})
  })`);
  return { op: 'goto', url: page.url, title: page.title, text: page.text, navigated };
}

async function main() {
  const started = Date.now();
  const out = { ok: false, targetId: null, tab: null, steps: [], pages: [], error: null, failed_step: null, failed_action: null };
  const steps = cfg.steps || [];
  let session = null;
  let close = () => {};
  let ctx = null;
  let unlock = () => {};
  let persisted = null;
  let closedByStep = false;
  let cleaned = false;
  let current = null;
  const cleanup = async () => {
    if (cleaned) return;
    cleaned = true;
    if (session && out.tab && !closedByStep) {
      out.tab.url = await withDeadline(currentUrl(session), CLEANUP_URL_DEADLINE_MS, 'cleanup url read').catch((e) => `url unreadable: ${e.message}`);
    }
    if (ctx && session && !closedByStep) await disableCapture(ctx);
    finishCapture(ctx, out, { consoleAll: cfg.consoleAll === true, cwd: cfg.cwd || safeCwd() });
    if (cfg.session && ctx && out.targetId && !closedByStep) {
      try {
        writeSessionFile(cfg.session.file, {
          version: SESSION_VERSION,
          targetId: out.targetId,
          isolated: out.tab.isolated,
          browserContextId: out.tab.browserContextId,
          ...snapshotUidState(ctx),
        });
      } catch (e) {
        out.ok = false;
        out.error = out.error || `session state was not saved: ${e.message}`;
      }
    }
    close();
    unlock();
  };
  const watchdog = cfg.watchdogMs
    ? setTimeout(async () => {
        out.ok = false;
        out.error = out.error || `browser helper ran past its ${cfg.watchdogMs}ms budget`;
        out.failed_step = out.failed_step ?? (current ? current.index : null);
        out.failed_action = out.failed_action ?? (current ? current.action : null);
        await cleanup();
        process.stdout.write(JSON.stringify(out) + '\n');
        process.exit(1);
      }, cfg.watchdogMs)
    : null;
  try {
    if (cfg.session) {
      unlock = await lockSession(cfg.session.file, Math.min(cfg.lockWaitMs || SESSION_LOCK_WAIT_MS, remainingMs()));
      persisted = readSessionFile(cfg.session.file);
    }
    const wantsIsolation = !!(cfg.session && cfg.session.isolated);
    const isolatedIds = isolatedTargetIds(path.join(cfg.cwd || safeCwd(), '.gm', 'browser-cdp-sessions'));
    const target = wantsIsolation
      ? await acquireIsolatedTarget(persisted)
      : await acquireTarget((persisted && persisted.targetId) || cfg.targetId, isolatedIds);
    out.targetId = target.id;
    const isolation = target.isolated
      ? { isolated: true, browserContextId: target.browserContextId }
      : persisted && persisted.isolated && persisted.targetId === target.id
        ? { isolated: true, browserContextId: persisted.browserContextId || null }
        : { isolated: false, browserContextId: null };
    out.tab = { targetId: target.id, state: target.created ? 'opened' : 'reused', url: null, ...isolation };
    const page = await openSession(target.webSocketDebuggerUrl, budgetCap(5000));
    close = () => page.close();
    session = page;
    ctx = createToolContext({
      session,
      host: { evaluate, waitForComplete, sleep },
      cwd: cfg.cwd || safeCwd(),
      timeoutMs: cfg.pageTimeoutMs || 30000,
      textLimit: cfg.textLimit || 20000,
    });
    if (persisted) {
      if (persisted.targetId === out.targetId) restoreUidState(ctx, persisted);
      else ctx.nextUid = persisted.nextUid || 0;
    }
    await attachCapture(ctx, steps);
    for (let index = 0; index < steps.length; index++) {
      const step = steps[index];
      current = { index: index + 1, action: step.op };
      if (remainingMs() <= 0) throw new Error(`call budget exhausted before step ${index + 1} (${step.op})`);
      const record = {
        index: index + 1,
        action: step.op,
        tab: out.targetId,
        tab_state: out.tab.state,
        url_before: await currentUrl(session),
        url_after: null,
        closed: false,
      };
      if (step.op === 'goto') {
        const pageOut = await gotoStep(session, step.url);
        out.pages.push(pageOut);
        record.navigated = pageOut.navigated;
        if (!pageOut.navigated) record.skipped = 'already-at-url';
      } else if (step.op === 'wait') {
        if (step.ms > remainingMs()) throw new Error(`wait ${step.ms}ms is longer than the call's remaining budget`);
        await sleep(step.ms);
        out.pages.push({ op: 'wait', ms: step.ms });
      } else if (step.op === 'eval') {
        const value = await evaluate(session, step.code);
        out.pages.push({ op: 'eval', code: step.code, value });
      } else if (step.op === 'cdp') {
        const sent = await session.send(step.method, step.params || {});
        const pageRecord = sent && typeof sent === 'object' ? { ...sent } : { result: sent === undefined ? null : sent };
        pageRecord.op = 'cdp';
        pageRecord.method = step.method;
        out.pages.push(pageRecord);
      } else if (step.op === 'events') {
        const captured = ctx.eventEntries.get(step);
        out.pages.push(captured || { op: 'events', event: step.event, count: 0, dropped: 0, events: [] });
      } else if (step.op === 'close') {
        await closeHttpTarget(out.targetId);
        closedByStep = true;
        record.closed = true;
        if (cfg.session) {
          const forgetError = await forgetSessionFile(cfg.session.file);
          if (forgetError) out.session_forget_error = forgetError;
        }
      } else if (TOOL_OPS.has(step.op)) {
        out.pages.push(await runToolStep(ctx, step));
      } else {
        throw new Error(`unknown browser step op '${step.op}'`);
      }
      record.url_after = closedByStep ? null : await currentUrl(session);
      out.steps.push(record);
      writePartial(out);
      current = null;
    }
    out.ok = true;
  } catch (e) {
    out.error = errorText(e);
    out.failed_step = current ? current.index : null;
    out.failed_action = current ? current.action : null;
  } finally {
    await cleanup();
  }
  if (watchdog) clearTimeout(watchdog);
  if (listenerFaults.length) out.listenerFaults = listenerFaults;
  out.duration_ms = Date.now() - started;
  process.stdout.write(JSON.stringify(out) + '\n');
  process.exitCode = out.ok ? 0 : 1;
}

main();
