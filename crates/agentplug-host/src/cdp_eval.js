import fs from 'fs';
import http from 'http';

function httpJson(url, timeoutMs) {
  return new Promise((resolve) => {
    const req = http.get(url, { timeout: timeoutMs }, (res) => {
      let body = '';
      res.on('data', (c) => { body += c; });
      res.on('end', () => { try { resolve(JSON.parse(body)); } catch (_) { resolve(null); } });
    });
    req.on('error', () => resolve(null));
    req.on('timeout', () => { req.destroy(); resolve(null); });
  });
}

function httpPutJson(url, timeoutMs) {
  return new Promise((resolve) => {
    const req = http.request(url, { method: 'PUT', timeout: timeoutMs }, (res) => {
      let body = '';
      res.on('data', (c) => { body += c; });
      res.on('end', () => { try { resolve(JSON.parse(body)); } catch (_) { resolve(null); } });
    });
    req.on('error', () => resolve(null));
    req.on('timeout', () => { req.destroy(); resolve(null); });
    req.end();
  });
}

function isInternalChromeUrl(url) {
  if (!url) return false;
  return url.startsWith('chrome://') || url.startsWith('chrome-untrusted://') || url.startsWith('devtools://');
}

function cdpUrl(endpoint, path) {
  return `${endpoint.replace(/\/$/, '')}${path}`;
}

async function createTargetViaFlattenedSession(endpoint, startUrl) {
  const version = await httpJson(cdpUrl(endpoint, '/json/version'), 2000);
  const rootWsUrl = version && version.webSocketDebuggerUrl;
  if (!rootWsUrl) return null;
  let sess;
  try {
    sess = await cdpSession(rootWsUrl, 5000);
  } catch (_) {
    return null;
  }
  try {
    const created = await sess.send('Target.createTarget', { url: startUrl || 'about:blank' });
    const targetId = created && created.targetId;
    if (!targetId) return null;
    const attached = await sess.send('Target.attachToTarget', { targetId, flatten: true });
    const sessionId = attached && attached.sessionId;
    if (!sessionId) { sess.close(); return null; }
    sess.bindSession(sessionId);
    return { id: targetId, webSocketDebuggerUrl: rootWsUrl, sessionId, url: startUrl || 'about:blank', __liveSession: sess };
  } catch (_) {
    sess.close();
    return null;
  }
}

async function pickPageTarget(endpoint, startUrl, targetId, timeoutMs, claimFreshTarget) {
  const deadline = Date.now() + timeoutMs;
  let sawWorkingJsonList = false;
  while (Date.now() < deadline) {
    const list = await httpJson(cdpUrl(endpoint, '/json/list'), 2000);
    if (Array.isArray(list)) {
      sawWorkingJsonList = true;
      if (targetId) {
        const remembered = list.find((t) => t.id === targetId && t.webSocketDebuggerUrl);
        if (remembered) return remembered;
      }
      if (!claimFreshTarget) {
        const pages = list.filter((t) => t.type === 'page' && t.webSocketDebuggerUrl);
        const realPage = pages.find((t) => !isInternalChromeUrl(t.url));
        if (realPage) return realPage;
        if (pages.length) return pages[0];
      }
    }
    if (startUrl || claimFreshTarget) {
      const created = await httpPutJson(cdpUrl(endpoint, `/json/new?${encodeURIComponent(startUrl || 'about:blank')}`), 3000);
      if (created && created.webSocketDebuggerUrl) return created;
      if (sawWorkingJsonList) {
        const flattened = await createTargetViaFlattenedSession(endpoint, startUrl);
        if (flattened) return flattened;
      }
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  return null;
}

function cdpSession(wsUrl, timeoutMs) {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(wsUrl);
    let opened = false;
    let nextId = 1;
    let boundSessionId = null;
    const pending = new Map();
    const timer = setTimeout(() => { try { ws.close(); } catch (_) {} reject(new Error('cdp timeout')); }, timeoutMs);
    const rejectAllPendingSendsOnSocketDrop = (reason) => {
      for (const { rej } of pending.values()) rej(new Error(reason));
      pending.clear();
    };
    const sessObj = {
      send(method, params, sessionId) {
        const id = nextId++;
        return new Promise((res, rej) => {
          pending.set(id, { res, rej });
          const body = { id, method, params: params || {} };
          if (sessionId || boundSessionId) body.sessionId = sessionId || boundSessionId;
          ws.send(JSON.stringify(body));
        });
      },
      bindSession(sessionId) { boundSessionId = sessionId; },
      close() { clearTimeout(timer); try { ws.close(); } catch (_) {} },
      onIdLessNotification: null,
    };
    ws.addEventListener('open', () => { opened = true; clearTimeout(timer); resolve(sessObj); });
    ws.addEventListener('message', (ev) => {
      let msg;
      try { msg = JSON.parse(ev.data); } catch (_) { return; }
      if (msg.id && pending.has(msg.id)) {
        const { res, rej } = pending.get(msg.id);
        pending.delete(msg.id);
        if (msg.error) rej(new Error(msg.error.message || 'cdp error'));
        else res(msg.result);
      } else if (msg.method && sessObj.onIdLessNotification) {
        sessObj.onIdLessNotification(msg);
      }
    });
    ws.addEventListener('error', () => {
      clearTimeout(timer);
      if (!opened) { reject(new Error('cdp websocket error')); return; }
      rejectAllPendingSendsOnSocketDrop('cdp websocket error (connection dropped mid-session)');
    });
    ws.addEventListener('close', () => {
      clearTimeout(timer);
      if (!opened) { reject(new Error('cdp websocket closed before opening')); return; }
      rejectAllPendingSendsOnSocketDrop('cdp websocket closed (connection dropped mid-session)');
    });
  });
}

const CONNECTION_DROP_MARKER = 'connection dropped mid-session';
const RECONNECT_CONSECUTIVE_FAILURE_LIMIT = 4;

function isConnectionDrop(error) {
  return String(error && error.message || error).includes(CONNECTION_DROP_MARKER);
}

async function openTargetConnection(endpoint, target) {
  if (target.sessionId) {
    const version = await httpJson(cdpUrl(endpoint, '/json/version'), 2000);
    if (!version || !version.webSocketDebuggerUrl) return { failure: `CDP endpoint ${endpoint} stopped answering /json/version -- the browser process exited or was killed` };
    const root = await cdpSession(version.webSocketDebuggerUrl, 5000);
    const attached = await root.send('Target.attachToTarget', { targetId: target.id, flatten: true });
    root.bindSession(attached.sessionId);
    return { connection: root };
  }
  const list = await httpJson(cdpUrl(endpoint, '/json/list'), 2000);
  if (!Array.isArray(list)) return { failure: `CDP endpoint ${endpoint} stopped answering /json/list -- the browser process exited or was killed` };
  const same = list.find((t) => t.id === target.id && t.webSocketDebuggerUrl);
  if (!same) return { failure: `target ${target.id} is no longer listed by the browser -- the tab was closed or crashed` };
  return { connection: await cdpSession(same.webSocketDebuggerUrl, 5000) };
}

async function reopenTargetConnection(endpoint, target, deadline) {
  let failure = 'the deadline passed before any reconnect attempt';
  for (let consecutiveFailures = 0; Date.now() < deadline && consecutiveFailures < RECONNECT_CONSECUTIVE_FAILURE_LIMIT; consecutiveFailures++) {
    try {
      const opened = await openTargetConnection(endpoint, target);
      if (opened.connection) return opened.connection;
      failure = opened.failure;
    } catch (e) {
      failure = `reconnect to target ${target.id} failed: ${e && e.message || e}`;
    }
    await new Promise((r) => setTimeout(r, 500));
  }
  throw new Error(`cdp websocket dropped mid-session and the same target could not be re-attached: ${failure}`);
}

function resumableSession(endpoint, target, first) {
  let live = first;
  const replayedSetup = new Map();
  const wrapper = {
    onIdLessNotification: null,
    send(method, params) {
      if (method.endsWith('.enable') || method.startsWith('Emulation.set') || method === 'Input.setIgnoreInputEvents') replayedSetup.set(method, params);
      if (method.endsWith('.disable')) replayedSetup.delete(method.replace(/\.disable$/, '.enable'));
      return live.send(method, params);
    },
    async reattach(deadline) {
      try { live.close(); } catch (_) {}
      live = await reopenTargetConnection(endpoint, target, deadline);
      live.onIdLessNotification = forwardNotification;
      for (const [method, params] of replayedSetup) await live.send(method, params).catch(() => {});
    },
    close() { live.close(); },
  };
  const forwardNotification = (msg) => { if (wrapper.onIdLessNotification) wrapper.onIdLessNotification(msg); };
  first.onIdLessNotification = forwardNotification;
  return wrapper;
}

async function evaluateParkedSurvivingReconnect(sess, wrapped, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  const token = JSON.stringify(`${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2)}`);
  const slot = `globalThis.__gmEvalRuns[${token}]`;
  const park = `((globalThis.__gmEvalRuns ||= {}), ${slot} = ${wrapped})`;
  const resume = `(globalThis.__gmEvalRuns && ${token} in globalThis.__gmEvalRuns) ? ${slot} : Promise.reject(new Error('the parked evaluation is gone from the page after the cdp reconnect -- the page navigated or reloaded while the connection was down'))`;
  let expression = park;
  for (;;) {
    try {
      const result = await sess.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true, userGesture: true, timeout: timeoutMs });
      await sess.send('Runtime.evaluate', { expression: `globalThis.__gmEvalRuns && delete ${slot}` }).catch(() => {});
      return result;
    } catch (e) {
      if (!isConnectionDrop(e) || Date.now() >= deadline) throw e;
      process.stderr.write(`cdp-eval: ${e.message} -- re-attaching to target and resuming the parked evaluation\n`);
      await sess.reattach(deadline);
      expression = resume;
    }
  }
}

const NAV_SETTLE_POLL_MS = 100;
const HASH_SETTLE_CAP_MS = 2000;
const NAV_BUDGET_CAP_MS = 30000;

const navSleep = (ms) => new Promise((r) => setTimeout(r, ms));

function navBaseOf(url) {
  const raw = String(url == null ? '' : url);
  try {
    const u = new URL(raw);
    return `${u.origin}${u.pathname}${u.search}`;
  } catch (_) {
    return raw.split('#')[0];
  }
}

function navHashOf(url) {
  const raw = String(url == null ? '' : url);
  try {
    return new URL(raw).hash || '';
  } catch (_) {
    const i = raw.indexOf('#');
    return i < 0 ? '' : raw.slice(i);
  }
}

function sessionSetupApplier(sess, viewport, inputIsolation) {
  const applied = { viewport: null, input_isolation: null };
  const wantsViewport = Boolean(viewport && viewport.width && viewport.height);
  if (wantsViewport) {
    applied.viewport = {
      width: viewport.width,
      height: viewport.height,
      deviceScaleFactor: viewport.deviceScaleFactor || 1,
      mobile: viewport.mobile !== false,
    };
  }
  if (inputIsolation === true || inputIsolation === false) applied.input_isolation = inputIsolation;
  const apply = async () => {
    if (applied.viewport) {
      await sess.send('Emulation.setDeviceMetricsOverride', {
        width: applied.viewport.width,
        height: applied.viewport.height,
        deviceScaleFactor: applied.viewport.deviceScaleFactor,
        mobile: applied.viewport.mobile,
        screenWidth: applied.viewport.width,
        screenHeight: applied.viewport.height,
      });
      if (applied.viewport.mobile) {
        await sess.send('Emulation.setTouchEmulationEnabled', { enabled: true, maxTouchPoints: 5 }).catch(() => {});
      }
    }
    if (applied.input_isolation !== null) {
      await sess.send('Input.setIgnoreInputEvents', { ignore: applied.input_isolation });
    }
  };
  return { applied, apply };
}

const NAVIGATING_RAW_METHODS = new Set(['Page.navigate', 'Page.reload']);

async function documentStateOf(sess) {
  const res = await sess.send('Runtime.evaluate', {
    expression: 'JSON.stringify({ url: String(location.href), readyState: String(document.readyState), blank: (function () { var b = document.body; if (!b) return true; return b.childElementCount === 0 && !String(b.textContent || "").trim(); })(), title: String(document.title || "") })',
    returnByValue: true,
  }).catch(() => null);
  const raw = res && res.result && res.result.value;
  if (typeof raw !== 'string') return null;
  try {
    const parsed = JSON.parse(raw);
    return parsed && typeof parsed === 'object' ? parsed : null;
  } catch (_) {
    return null;
  }
}

async function ensureDocumentForUrl(sess, startUrl, timeoutMs, reapply) {
  const target = String(startUrl);
  const budget = Math.min(Math.max(5000, Math.round(timeoutMs / 3)), NAV_BUDGET_CAP_MS);
  const before = await documentStateOf(sess);
  const kind = !before || !before.url || before.url === 'about:blank'
    || navBaseOf(before.url) !== navBaseOf(target)
    ? 'navigate'
    : (navHashOf(before.url) !== navHashOf(target) ? 'hash' : 'reload');

  let sawLoad = false;
  let sawSameDocument = false;
  let navigationFailure = null;
  const failedDocumentLoadsByRequestId = new Map();
  const prevOnIdLessNotification = sess.onIdLessNotification;
  sess.onIdLessNotification = (msg) => {
    if (prevOnIdLessNotification) prevOnIdLessNotification(msg);
    const method = msg && msg.method;
    if (method === 'Page.loadEventFired') sawLoad = true;
    else if (method === 'Page.navigatedWithinDocument') sawSameDocument = true;
    else if (method === 'Network.loadingFailed' && msg.params && msg.params.type === 'Document') {
      failedDocumentLoadsByRequestId.set(msg.params.requestId, msg.params.errorText || 'loading failed');
    }
  };

  let after = before;
  try {
    await sess.send('Network.enable', {});
    await sess.send('Page.enable', {});
    let navResult = null;
    if (kind === 'navigate') {
      navResult = await sess.send('Page.navigate', { url: target });
    } else if (kind === 'reload') {
      navResult = await sess.send('Page.reload', { ignoreCache: false });
    } else {
      await sess.send('Runtime.evaluate', { expression: `location.href = ${JSON.stringify(target)}` }).catch(() => {});
    }
    if (navResult && navResult.errorText) navigationFailure = navResult.errorText;
    const ownLoaderId = navResult && navResult.loaderId;
    if (!navigationFailure && ownLoaderId && failedDocumentLoadsByRequestId.has(ownLoaderId)) {
      navigationFailure = failedDocumentLoadsByRequestId.get(ownLoaderId);
    }

    const settleDeadline = Date.now() + (kind === 'hash' ? Math.min(HASH_SETTLE_CAP_MS, budget) : budget);
    for (;;) {
      after = await documentStateOf(sess);
      if (after) {
        if (kind === 'hash') {
          if (navHashOf(after.url) === navHashOf(target)) break;
        } else if (sawSameDocument) {
          break;
        } else if (sawLoad && after.readyState === 'complete') {
          break;
        }
      }
      if (Date.now() >= settleDeadline) break;
      await navSleep(NAV_SETTLE_POLL_MS);
    }
    if (reapply) await reapply();

    if (kind !== 'hash') {
      if (!navigationFailure && sawSameDocument && !sawLoad) {
        navigationFailure = `the browser treated ${target} as a same-document (fragment) navigation, so the previous document was reused instead of loading a new one`;
      }
      if (!navigationFailure && (!after || !after.readyState)) {
        navigationFailure = `the page stopped answering document state after navigating to ${target} (the tab may have crashed)`;
      }
      if (!navigationFailure && after && after.readyState !== 'complete') {
        navigationFailure = `the document at ${target} never reached readyState 'complete' within ${budget}ms (readyState='${after.readyState}')`;
      }
      if (!navigationFailure && /^(https?:|file:)/i.test(target) && after && after.blank) {
        navigationFailure = `the document at ${target} loaded to readyState 'complete' but its body has no elements and no text -- the page did not render, so evaluating against it would report zero rows as if that were the rendered state`;
      }
    }
  } finally {
    sess.onIdLessNotification = prevOnIdLessNotification;
  }

  return {
    navigation: kind,
    requested_url: target,
    url: after ? after.url : null,
    ready_state: after ? after.readyState : null,
    blank_document: after ? !!after.blank : null,
    title: after ? after.title : null,
    loaded_new_document: kind === 'hash' ? false : (sawLoad && !sawSameDocument),
    navigation_failure: navigationFailure,
  };
}

async function navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs, reapply = null, keepNetworkEvents = false) {
  let navigationFailure = null;
  let documentTelemetry = null;
  if (startUrl) {
    documentTelemetry = await ensureDocumentForUrl(sess, startUrl, timeoutMs, reapply);
    navigationFailure = documentTelemetry.navigation_failure;
    if (!keepNetworkEvents) await sess.send('Network.disable', {}).catch(() => {});
  }
  const trimmedScript = script.trim();
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
  const exprAttempt = `(async () => { return (\n${trimmedScript}\n); })()`;
  const stmtAttempt = `(async () => { ${script} })()`;
  let wrapped = stmtAttempt;
  if (trimmedScript && !/^\s*(return|const|let|var|if|for|while|switch|try|function|async|throw)\b/.test(trimmedScript)) {
    try {
      new AsyncFunction(`"use strict"; return (\n${trimmedScript}\n);`);
      wrapped = exprAttempt;
    } catch (_) {}
  }
  const result = await evaluateParkedSurvivingReconnect(sess, wrapped, timeoutMs);
  result.statementBodyWithoutReturn = wrapped === stmtAttempt && !/\breturn\b/.test(trimmedScript);
  if (!documentTelemetry) {
    const state = await documentStateOf(sess);
    documentTelemetry = {
      navigation: 'none',
      requested_url: null,
      url: state ? state.url : null,
      ready_state: state ? state.readyState : null,
      blank_document: state ? !!state.blank : null,
      title: state ? state.title : null,
      loaded_new_document: false,
      navigation_failure: null,
    };
  }
  result.__document = documentTelemetry;
  if (navigationFailure) {
    const prior = result.exceptionDetails
      ? `; the evaluation then reported: ${result.exceptionDetails.exception && result.exceptionDetails.exception.description ? result.exceptionDetails.exception.description : (result.exceptionDetails.text || 'evaluate exception')}`
      : '';
    result.exceptionDetails = { text: `page navigation failed: ${navigationFailure} (url=${startUrl})${prior}` };
  }
  return result;
}

const GL_ERROR_TRACKING_INIT_SCRIPT = `
(() => {
  const MAX_SIGNATURES = 40;
  window.__gmGlErrors = window.__gmGlErrors || {};
  const drawCounts = window.__gmGlDrawCalls = window.__gmGlDrawCalls || { drawArrays: 0, drawElements: 0, drawArraysInstanced: 0, drawElementsInstanced: 0 };
  window.__gmGlErrorTotalCount = window.__gmGlErrorTotalCount || 0;
  window.__gmGlLastDrainedError = null;
  const drawFns = ['drawArrays', 'drawElements', 'drawArraysInstanced', 'drawElementsInstanced'];
  const FRAME_DRAIN_ATTRIBUTION = 'drained once per animation frame; attributed to the last draw call issued before the drain';
  const contextsDrawnThisFrame = [];
  const trackedSet = new WeakSet();
  let drainScheduled = false;
  const recordFrameError = (ctx, err) => {
    window.__gmGlErrorTotalCount += 1;
    const fnName = ctx.lastFn;
    const isArrays = fnName === 'drawArrays' || fnName === 'drawArraysInstanced';
    const count = isArrays ? ctx.p2 : ctx.p1;
    const first = isArrays ? ctx.p1 : undefined;
    const instanceCount = isArrays ? ctx.p3 : ctx.p4;
    const sig = fnName + '|' + err + '|' + ctx.p0 + '|' + count + '|' + (instanceCount || 0);
    const existing = window.__gmGlErrors[sig];
    if (existing) {
      existing.occurrenceCount += 1;
      existing.lastDrawCallIndex = drawCounts[fnName];
    } else if (Object.keys(window.__gmGlErrors).length < MAX_SIGNATURES) {
      window.__gmGlErrors[sig] = {
        fn: fnName, error: err, mode: ctx.p0, count, first, instanceCount: instanceCount || 0,
        occurrenceCount: 1, lastDrawCallIndex: drawCounts[fnName],
        stack: FRAME_DRAIN_ATTRIBUTION,
      };
    }
  };
  const drainOncePerFrame = () => {
    drainScheduled = false;
    for (const ctx of contextsDrawnThisFrame) {
      ctx.drewSinceDrain = false;
      const err = ctx.origGetError();
      window.__gmGlLastDrainedError = err;
      if (err === ctx.gl.NO_ERROR) continue;
      if (ctx.withheldError === ctx.gl.NO_ERROR) ctx.withheldError = err;
      recordFrameError(ctx, err);
    }
    contextsDrawnThisFrame.length = 0;
  };
  const scheduleDrain = () => {
    if (drainScheduled) return;
    drainScheduled = true;
    requestAnimationFrame(drainOncePerFrame);
  };
  const track = (gl) => {
    const ctx = { gl, origGetError: gl.getError.bind(gl), withheldError: gl.NO_ERROR, drewSinceDrain: false, lastFn: null, p0: 0, p1: 0, p2: 0, p3: 0, p4: 0 };
    gl.getError = function () {
      if (ctx.withheldError !== gl.NO_ERROR) {
        const err = ctx.withheldError;
        ctx.withheldError = gl.NO_ERROR;
        return err;
      }
      return ctx.origGetError();
    };
    for (const fnName of drawFns) {
      const orig = gl[fnName];
      if (typeof orig !== 'function') continue;
      const wrapped = function (p0, p1, p2, p3, p4) {
        orig.call(this, p0, p1, p2, p3, p4);
        drawCounts[fnName]++;
        ctx.lastFn = fnName; ctx.p0 = p0; ctx.p1 = p1; ctx.p2 = p2; ctx.p3 = p3; ctx.p4 = p4;
        if (!ctx.drewSinceDrain) { ctx.drewSinceDrain = true; contextsDrawnThisFrame.push(ctx); scheduleDrain(); }
      };
      wrapped.__gmWrapped = true;
      gl[fnName] = wrapped;
    }
  };
  const origGetContext = HTMLCanvasElement.prototype.getContext;
  HTMLCanvasElement.prototype.getContext = function (type, ...rest) {
    const gl = origGetContext.call(this, type, ...rest);
    if (!gl || !/^webgl/.test(type) && type !== 'experimental-webgl') return gl;
    if (!trackedSet.has(gl)) { trackedSet.add(gl); track(gl); }
    return gl;
  };
})();
`;

async function attachDebugCapture(sess, glCapture) {
  const consoleLines = [];
  const networkEvents = [];
  const pageErrors = [];
  sess.onIdLessNotification = (msg) => {
    if (msg.method === 'Runtime.consoleAPICalled') {
      const args = (msg.params.args || []).map((a) => (a.value !== undefined ? a.value : a.description || a.type));
      consoleLines.push({ type: msg.params.type, args, ts: msg.params.timestamp });
    } else if (msg.method === 'Runtime.exceptionThrown') {
      const ex = msg.params.exceptionDetails;
      pageErrors.push({
        text: (ex.exception && ex.exception.description) || ex.text || 'uncaught exception',
        url: ex.url || null,
        line: ex.lineNumber != null ? ex.lineNumber + 1 : null,
        column: ex.columnNumber != null ? ex.columnNumber + 1 : null,
        ts: msg.params.timestamp,
      });
    } else if (msg.method === 'Network.requestWillBeSent') {
      networkEvents.push({ phase: 'request', url: msg.params.request.url, method: msg.params.request.method, ts: msg.params.timestamp });
    } else if (msg.method === 'Network.responseReceived') {
      networkEvents.push({ phase: 'response', url: msg.params.response.url, status: msg.params.response.status, ts: msg.params.timestamp });
    }
  };
  await sess.send('Network.enable', {});
  if (glCapture) await sess.send('Page.addScriptToEvaluateOnNewDocument', { source: GL_ERROR_TRACKING_INIT_SCRIPT });
  const NETWORK_CAP = 30;
  const CONSOLE_CAP = 50;
  const boundedNetwork = () => {
    if (networkEvents.length <= NETWORK_CAP) return { network: networkEvents, network_dropped: 0 };
    const failed = networkEvents.filter((e) => e.phase === 'response' && !(e.status >= 200 && e.status < 400));
    const kept = failed.slice(0, NETWORK_CAP);
    for (let i = networkEvents.length - 1; i >= 0 && kept.length < NETWORK_CAP; i--) {
      if (!kept.includes(networkEvents[i])) kept.push(networkEvents[i]);
    }
    return { network: kept, network_dropped: networkEvents.length - kept.length };
  };
  const boundedConsole = () => ({
    console: consoleLines.slice(-CONSOLE_CAP),
    console_dropped: Math.max(0, consoleLines.length - CONSOLE_CAP),
  });
  return async () => {
    const perf = await sess.send('Runtime.evaluate', { expression: 'JSON.stringify(performance.timing || {})', returnByValue: true }).catch(() => null);
    let performanceSnapshot = null;
    try { performanceSnapshot = perf && perf.result && perf.result.value ? JSON.parse(perf.result.value) : null; } catch (_) {}
    const glRes = await sess.send('Runtime.evaluate', {
      expression: 'JSON.stringify({errors: Object.values(window.__gmGlErrors||{}), drawCalls: window.__gmGlDrawCalls||{}, errorTotalCount: window.__gmGlErrorTotalCount||0})',
      returnByValue: true,
    }).catch(() => null);
    let gl = { errors: [], drawCalls: {}, errorTotalCount: 0 };
    try { if (glRes && glRes.result && glRes.result.value) gl = JSON.parse(glRes.result.value); } catch (_) {}
    return { instrumented: true, ...boundedConsole(), pageErrors, ...boundedNetwork(), performance: performanceSnapshot, gl };
  };
}

const NO_RETURN_NOTE = 'the script ran as a statement body and returned no value: a single expression (top-level await included) returns its value automatically, a multi-statement script must end with an explicit `return <value>`';

function resultNoteFor(res, value) {
  return res.statementBodyWithoutReturn && (value === undefined || value === null) ? NO_RETURN_NOTE : undefined;
}

function aggregateCpuProfile(profile, topN) {
  if (!profile || !Array.isArray(profile.nodes) || !Array.isArray(profile.samples)) {
    return { timeframe: null, culprits: [] };
  }
  const byId = new Map();
  for (const node of profile.nodes) byId.set(node.id, node);
  const deltas = Array.isArray(profile.timeDeltas) ? profile.timeDeltas : [];
  const selfUs = new Map();
  for (let i = 0; i < profile.samples.length; i++) {
    const node = byId.get(profile.samples[i]);
    if (!node) continue;
    const delta = deltas[i + 1] || deltas[i] || 0;
    selfUs.set(node.id, (selfUs.get(node.id) || 0) + Math.abs(delta));
  }
  const totalUs = Array.from(selfUs.values()).reduce((a, b) => a + b, 0);
  const acc = new Map();
  for (const [id, us] of selfUs.entries()) {
    const node = byId.get(id);
    if (!node || !node.callFrame) continue;
    const cf = node.callFrame;
    const fn = cf.functionName || '(anonymous)';
    const loc = `${cf.url || ''}:${cf.lineNumber != null ? cf.lineNumber + 1 : 0}:${cf.columnNumber != null ? cf.columnNumber + 1 : 0}`;
    const key = `${fn}@${loc}`;
    const prior = acc.get(key) || { location: loc, function: fn, self_us: 0, hits: 0 };
    prior.self_us += us;
    prior.hits += 1;
    acc.set(key, prior);
  }
  const culprits = Array.from(acc.values())
    .map((c) => ({ ...c, self_pct: totalUs > 0 ? Math.round((c.self_us / totalUs) * 10000) / 100 : 0 }))
    .sort((a, b) => b.self_us - a.self_us)
    .slice(0, topN);
  return {
    timeframe: {
      start_us: typeof profile.startTime === 'number' ? profile.startTime : 0,
      end_us: typeof profile.endTime === 'number' ? profile.endTime : 0,
      total_us: totalUs,
      sample_count: profile.samples.length,
    },
    culprits,
  };
}

const SOFTWARE_RENDERER_PATTERN = /swiftshader|llvmpipe|softpipe|software|basic render|microsoft basic|warp/i;
const VENDOR_PATTERNS = { nvidia: /nvidia/i, amd: /amd|radeon/i, intel: /intel/i };

async function gpuReport(endpoint, probeSource, wantGpu, uncapped) {
  const version = await httpJson(cdpUrl(endpoint, '/json/version'), 2000);
  if (!version || !version.webSocketDebuggerUrl) throw new Error(`CDP endpoint ${endpoint} did not answer /json/version`);
  const root = await cdpSession(version.webSocketDebuggerUrl, 5000);
  const page = await cdpSession(version.webSocketDebuggerUrl, 5000);
  const server = http.createServer((_, res) => { res.setHeader('content-type', 'text/html'); res.end('<!doctype html><title>gpu</title>'); });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  let targetId = null;
  try {
    const info = await root.send('SystemInfo.getInfo', {});
    const created = await page.send('Target.createTarget', { url: `http://127.0.0.1:${server.address().port}/` });
    targetId = created.targetId;
    const attached = await page.send('Target.attachToTarget', { targetId, flatten: true });
    page.bindSession(attached.sessionId);
    await page.send('Emulation.setFocusEmulationEnabled', { enabled: true }).catch(() => {});
    if (uncapped) await page.send('Page.bringToFront', {}).catch(() => {});
    for (let i = 0; i < 40; i++) {
      const ready = await page.send('Runtime.evaluate', { expression: 'document.readyState', returnByValue: true }).catch(() => null);
      if (ready && ready.result && ready.result.value === 'complete') break;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    let evaluated = null;
    for (let attempt = 0; attempt < 3 && !evaluated; attempt++) {
      try {
        evaluated = await page.send('Runtime.evaluate', { expression: probeSource, awaitPromise: true, returnByValue: true });
      } catch (e) {
        if (attempt === 2) throw e;
        await new Promise((resolve) => setTimeout(resolve, 500));
      }
    }
    const probe = (evaluated.result && evaluated.result.value) || {};
    const aux = (info.gpu && info.gpu.auxAttributes) || {};
    const features = (info.gpu && info.gpu.featureStatus) || {};
    const adapter = probe.gpu ? `${probe.gpu.vendor || '?'}/${probe.gpu.arch || '?'}` : null;
    const reasons = [];
    if (!probe.gl) reasons.push('no webgl2');
    else if (!probe.glDraw) reasons.push('webgl2 draw failed');
    if (SOFTWARE_RENDERER_PATTERN.test(`${probe.gl || ''} ${probe.gpu ? Object.values(probe.gpu).join(' ') : ''}`)) reasons.push('software renderer');
    if (probe.gpu && probe.gpu.fallback) reasons.push('webgpu fallback adapter');
    if (features.webgpu === 'enabled' && probe.gpu && probe.gpuCompute !== true) reasons.push('webgpu compute failed');
    if (features.gpu_compositing !== 'enabled') reasons.push(`gpu_compositing ${features.gpu_compositing}`);
    if (wantGpu && VENDOR_PATTERNS[wantGpu] && !VENDOR_PATTERNS[wantGpu].test(probe.gl || '')) reasons.push(`requested gpu=${wantGpu} but renderer is ${probe.gl}`);
    const report = {
      accelerated: reasons.length === 0,
      gl: probe.gl,
      angle: aux.displayType,
      skia: aux.skiaBackendType,
      webgl: features.webgl,
      webgpu: features.webgpu,
      adapter: adapter ? adapter + (probe.gpu.fallback ? '/FALLBACK' : '') : 'none',
      draw: probe.glDraw === true,
      compute: probe.gpuCompute === true,
      fps: probe.fps,
      focused: probe.focused,
      visibility: probe.visibility,
    };
    if (wantGpu) report.want = wantGpu;
    if (reasons.length) report.warn = `NOT ACCELERATED OR MISMATCHED: ${reasons.join('; ')} -- perf and visual witnesses from this session are untrustworthy`;
    else if (probe.fps < 30) report.warn = `rAF only ${probe.fps}fps on an idle page -- session is throttled or the display is slow; perf witnesses are untrustworthy`;
    return report;
  } finally {
    if (targetId) await root.send('Target.closeTarget', { targetId }).catch(() => {});
    root.close();
    page.close();
    server.close();
  }
}

const TRACE_CATEGORIES = [
  'devtools.timeline', 'disabled-by-default-devtools.timeline', 'disabled-by-default-devtools.timeline.frame',
  'toplevel', 'gpu', 'gpu.service', 'viz', 'cc', 'benchmark', 'blink.user_timing', 'v8.execute',
];
const TRACE_FLUSH_WATCHDOG_MARGIN_MS = 300;
const TRACE_STREAM_CHUNK_BYTES = 1 << 20;
const TRACE_THREADS_REPORTED = 12;
const TRACE_CATEGORIES_REPORTED = 15;

async function startTraceRecording(sess) {
  const recording = { bufferPercentFull: 0 };
  let markComplete;
  recording.complete = new Promise((resolve) => { markComplete = resolve; });
  const prevOnIdLessNotification = sess.onIdLessNotification;
  sess.onIdLessNotification = (msg) => {
    if (prevOnIdLessNotification) prevOnIdLessNotification(msg);
    if (msg.method === 'Tracing.bufferUsage') recording.bufferPercentFull = Math.max(recording.bufferPercentFull, (msg.params && msg.params.percentFull) || 0);
    if (msg.method === 'Tracing.tracingComplete') markComplete(msg.params || {});
  };
  await sess.send('Tracing.start', {
    traceConfig: { recordMode: 'recordAsMuchAsPossible', includedCategories: TRACE_CATEGORIES },
    transferMode: 'ReturnAsStream',
    streamFormat: 'json',
    streamCompression: 'none',
    bufferUsageReportingInterval: 500,
  });
  return recording;
}

async function readTraceStream(sess, handle, deadline, artifactFile) {
  const chunks = [];
  let bytes = 0;
  if (artifactFile) fs.writeFileSync(artifactFile, '');
  for (;;) {
    if (Date.now() > deadline) return { chunks, bytes, error: `trace stream read passed the dispatch deadline after ${bytes} bytes -- raise timeout= so the trace transfer fits inside the dispatch` };
    const chunk = await sess.send('IO.read', { handle, size: TRACE_STREAM_CHUNK_BYTES });
    const buf = chunk.base64Encoded ? Buffer.from(chunk.data || '', 'base64') : Buffer.from(chunk.data || '', 'utf8');
    chunks.push(buf);
    bytes += buf.length;
    if (artifactFile) fs.appendFileSync(artifactFile, buf);
    if (chunk.eof) return { chunks, bytes, error: null };
  }
}

async function stopTraceRecordingToFile(sess, recording, deadline, artifactFile) {
  const endedAt = Date.now();
  await sess.send('Tracing.end', {});
  const completion = await Promise.race([
    recording.complete,
    new Promise((resolve) => setTimeout(() => resolve(null), Math.max(0, deadline - Date.now()))),
  ]);
  const flushMs = Date.now() - endedAt;
  const base = { events: [], bytes: 0, flushMs, bufferPercentFull: recording.bufferPercentFull, dataLoss: !!(completion && completion.dataLossOccurred) };
  if (!completion) return { ...base, error: `Tracing.tracingComplete did not arrive within ${flushMs}ms of Tracing.end -- raise timeout= so the trace flush fits inside the dispatch` };
  if (!completion.stream) return { ...base, error: 'Tracing.tracingComplete carried no stream handle' };
  const read = await readTraceStream(sess, completion.stream, deadline, artifactFile);
  await sess.send('IO.close', { handle: completion.stream }).catch(() => {});
  const transfer = { ...base, bytes: read.bytes, flushMs: Date.now() - endedAt };
  if (read.error) return { ...transfer, error: read.error };
  try {
    const parsed = JSON.parse(Buffer.concat(read.chunks).toString('utf8'));
    return { ...transfer, events: Array.isArray(parsed) ? parsed : (parsed.traceEvents || []), error: null };
  } catch (e) {
    return { ...transfer, error: `trace stream of ${read.bytes} bytes did not parse as JSON: ${e && e.message || e}` };
  }
}

function mergedIntervalLength(intervals) {
  intervals.sort((a, b) => a[0] - b[0]);
  let total = 0, start = -Infinity, end = -Infinity;
  for (const [s, e] of intervals) {
    if (s > end) { if (end > start) total += end - start; start = s; end = e; }
    else if (e > end) end = e;
  }
  if (end > start) total += end - start;
  return total;
}

function summarizeTrace(captured) {
  const processNames = new Map();
  const threadNames = new Map();
  for (const e of captured.events) {
    if (e.ph !== 'M' || !e.args) continue;
    if (e.name === 'process_name') processNames.set(e.pid, e.args.name);
    else if (e.name === 'thread_name') threadNames.set(`${e.pid}:${e.tid}`, e.args.name);
  }
  const threads = new Map();
  const byCategory = {};
  let eventCount = 0;
  for (const e of captured.events) {
    if (e.ph === 'M') continue;
    eventCount++;
    const key = `${e.pid}:${e.tid}`;
    let t = threads.get(key);
    if (!t) { t = { pid: e.pid, tid: e.tid, events: 0, intervals: [], open: [] }; threads.set(key, t); }
    t.events++;
    if (e.ph === 'X' && e.dur > 0) {
      t.intervals.push([e.ts, e.ts + e.dur]);
      byCategory[e.cat || 'unknown'] = (byCategory[e.cat || 'unknown'] || 0) + e.dur;
    } else if (e.ph === 'B') t.open.push(e.ts);
    else if (e.ph === 'E' && t.open.length) t.intervals.push([t.open.pop(), e.ts]);
  }
  const rows = Array.from(threads.values()).map((t) => ({
    process: processNames.get(t.pid) || null,
    thread: threadNames.get(`${t.pid}:${t.tid}`) || null,
    pid: t.pid,
    tid: t.tid,
    busy_us: Math.round(mergedIntervalLength(t.intervals)),
    events: t.events,
  })).sort((a, b) => b.busy_us - a.busy_us);
  const busyOf = (processPattern, threadName) => rows
    .filter((r) => r.thread === threadName && processPattern.test(r.process || ''))
    .reduce((sum, r) => sum + r.busy_us, 0);
  const gpuProcessEvents = rows.filter((r) => /GPU Process/.test(r.process || '')).reduce((sum, r) => sum + r.events, 0);
  const error = captured.error || (eventCount === 0 ? 'the trace holds no events besides metadata' : null);
  const summary = {
    main_us: busyOf(/Renderer/, 'CrRendererMain'),
    cc_us: busyOf(/Renderer/, 'Compositor'),
    gpu_us: busyOf(/GPU Process/, 'CrGpuMain'),
    viz_us: busyOf(/GPU Process|Browser/, 'VizCompositorThread'),
    event_count: eventCount,
    gpu_process_events: gpuProcessEvents,
    trace_bytes: captured.bytes,
    flush_ms: captured.flushMs,
    data_loss: captured.dataLoss,
    buffer_percent_full: captured.bufferPercentFull,
    threads: rows.slice(0, TRACE_THREADS_REPORTED),
    by_category: Object.fromEntries(Object.entries(byCategory).sort((a, b) => b[1] - a[1]).slice(0, TRACE_CATEGORIES_REPORTED)),
  };
  if (error) summary.trace_error = error;
  return summary;
}

const RAW_DIRECTIVE = /^(cdp|events|wait)[ \t]+(.*)$/;
const RAW_EVENT_NAME = /^[A-Z]\w*\.\w+$/;
const RAW_BLOCK_NAME = /^[A-Za-z_]\w*$/;
const RAW_REFERENCE = /^\$([A-Za-z_]\w*|\d+)((?:\.[\w$-]+|\[\d+\])*)$/;
const RAW_AUTO_ENABLED_DOMAINS = new Set(['Debugger', 'Network', 'Log', 'Runtime', 'Page', 'Profiler']);
const RAW_BROWSER_LEVEL_DOMAINS = new Set(['Browser', 'SystemInfo']);
const RAW_BROWSER_LEVEL_TARGET_METHODS = new Set(['createBrowserContext', 'disposeBrowserContext', 'getBrowserContexts', 'createTarget', 'closeTarget', 'getTargets', 'attachToTarget', 'activateTarget']);
const RAW_EVENT_LIMIT_DEFAULT = 200;
const RAW_EVENT_LIMIT_MAX = 5000;
const RAW_EVENTS_BYTES_CAP = 400000;
const RAW_EVENT_PARAMS_BYTES_CAP = 6000;
const RAW_WAIT_TIMEOUT_DEFAULT_MS = 10000;
const RAW_DEADLINE_MARGIN_MS = 400;
const RAW_POLL_MS = 25;
const NOT_ENABLED_HINT = ' -- CDP domain state belongs to the connection of one dispatch: a domain enabled by an earlier dispatch is not enabled now. Put `cdp <Domain>.enable` (or an `events <Domain.event>` line, which enables it) in the same body as the command that needs it';

function parseRawWhere(clause, line) {
  const parsed = /^([^~=\s]+)([~=])(.+)$/.exec(clause);
  if (!parsed) throw new Error(`line ${line}: \`where\` needs <param.path>~<substring> or <param.path>=<value>, got '${clause}'`);
  return { path: parsed[1], op: parsed[2], value: parsed[3] };
}

function takeRawOption(tokens, name) {
  const last = tokens[tokens.length - 1] || '';
  if (!last.startsWith(`${name}=`)) return null;
  const value = Number(last.slice(name.length + 1));
  if (!Number.isInteger(value) || value < 0) return null;
  tokens.pop();
  return value;
}

function splitRawWhere(tokens, line) {
  const at = tokens.indexOf('where');
  if (at < 0) return { head: tokens, where: null };
  return { head: tokens.slice(0, at), where: parseRawWhere(tokens.slice(at + 1).join(' '), line) };
}

function startRawStep(kind, rest, line) {
  if (kind === 'cdp') {
    const parsed = /^(\S+)(?:[ \t]+on[ \t]+(\S+))?(?:[ \t]+as[ \t]+(\S+))?$/.exec(rest);
    if (!parsed || !RAW_EVENT_NAME.test(parsed[1])) throw new Error(`line ${line}: expected \`cdp <Domain.method> [on <targetId|$ref>] [as <name>]\`, got 'cdp ${rest.slice(0, 80)}'`);
    if (parsed[3] && !RAW_BLOCK_NAME.test(parsed[3])) throw new Error(`line ${line}: block name '${parsed[3]}' must be a letter or underscore followed by letters, digits or underscores (a bare number already means block position)`);
    return { kind, method: parsed[1], on: parsed[2] || null, name: parsed[3] || null, paramsText: '', paramsLines: 0, paramsStartLine: 0, line };
  }
  const tokens = rest.split(/\s+/).filter(Boolean);
  if (kind === 'events') {
    const limit = takeRawOption(tokens, 'limit');
    const { head, where } = splitRawWhere(tokens, line);
    const bad = head.find((n) => !RAW_EVENT_NAME.test(n));
    if (!head.length || bad) throw new Error(`line ${line}: \`events\` takes CDP event names like Debugger.scriptParsed, got '${bad || ''}'`);
    return { kind, names: head, where, limit, count: 0, line };
  }
  if (tokens.length === 1 && /^\d+$/.test(tokens[0])) return { kind: 'sleep', ms: Number(tokens[0]), line };
  const timeoutMs = takeRawOption(tokens, 'timeout');
  const { head, where } = splitRawWhere(tokens, line);
  if (head.length !== 1 || !RAW_EVENT_NAME.test(head[0])) throw new Error(`line ${line}: \`wait\` takes a millisecond count or one CDP event name, got '${head.join(' ')}'`);
  return { kind: 'waitEvent', name: head[0], where, timeoutMs, lastMatchSeq: 0, line };
}

function parseRawBody(script) {
  const steps = [];
  let current = null;
  script.split(/\r?\n/).forEach((text, index) => {
    const directive = RAW_DIRECTIVE.exec(text);
    if (directive) {
      current = startRawStep(directive[1], directive[2].trim(), index + 1);
      steps.push(current);
      return;
    }
    if (!text.trim()) return;
    if (!current || current.kind !== 'cdp') throw new Error(`line ${index + 1}: '${text.slice(0, 80)}' is not a cdp/events/wait directive and follows no \`cdp <Method>\` line`);
    if (current.paramsLines === 0 && !/^\s*[[{]/.test(text)) {
      throw new Error(`line ${index + 1}: '${text.slice(0, 80)}' follows \`cdp ${current.method}\` but is not the start of a JSON params object -- every line under a \`cdp <Method>\` up to the next directive is read as that command's one JSON params value, so a stray line here is a body-authoring fault, not a page fault`);
    }
    if (current.paramsLines === 0) current.paramsStartLine = index + 1;
    current.paramsLines += 1;
    current.paramsText += `${text}\n`;
  });
  const named = new Set();
  for (const step of steps) {
    if (step.kind !== 'cdp' || !step.name) continue;
    if (named.has(step.name)) throw new Error(`line ${step.line}: block name '${step.name}' is used twice`);
    named.add(step.name);
  }
  return steps;
}

function rawPathSegments(path) {
  return path.match(/[^.\[\]]+/g) || [];
}

function rawOwnField(cursor, segment) {
  return cursor !== null && typeof cursor === 'object' && Object.prototype.hasOwnProperty.call(cursor, segment);
}

function rawMatches(where, params) {
  if (!where) return true;
  let cursor = params;
  for (const segment of rawPathSegments(where.path)) {
    if (!rawOwnField(cursor, segment)) return false;
    cursor = cursor[segment];
  }
  if (cursor === undefined || cursor === null) return false;
  return where.op === '~' ? String(cursor).includes(where.value) : String(cursor) === where.value;
}

function rawBlocksSummary(context) {
  const earlier = context.byIndex.slice(1).map((b) => `${b.index} ${b.method}${b.name ? ` as ${b.name}` : ''}`);
  return earlier.length ? `earlier blocks: ${earlier.join(', ')}` : 'no earlier block ran';
}

function lookupRawReference(head, pathText, context, where, original) {
  const byPosition = /^\d+$/.test(head);
  const block = byPosition ? context.byIndex[Number(head)] : context.byName.get(head);
  if (!block) throw new Error(`${where} references ${original} but no earlier block is ${byPosition ? `number ${head}` : `named '${head}'`} (${rawBlocksSummary(context)}); write $$${original.slice(1)} for the literal text ${original}`);
  let cursor = block.result;
  for (const segment of rawPathSegments(pathText)) {
    if (!rawOwnField(cursor, segment)) {
      const have = cursor !== null && typeof cursor === 'object' ? Object.keys(cursor).join(', ') || 'no fields' : JSON.stringify(cursor);
      throw new Error(`${where} references ${original} but the result of block ${block.index} (${block.method}) has no field '${segment}' (it has: ${have})`);
    }
    cursor = cursor[segment];
  }
  return cursor;
}

function resolveRawReferences(value, context, where) {
  if (typeof value === 'string') {
    const reference = RAW_REFERENCE.exec(value);
    if (reference) return lookupRawReference(reference[1], reference[2], context, where, value);
    return value.startsWith('$$') && RAW_REFERENCE.test(value.slice(1)) ? value.slice(1) : value;
  }
  if (Array.isArray(value)) return value.map((item) => resolveRawReferences(item, context, where));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, resolveRawReferences(item, context, where)]));
  return value;
}

function rawEventCapture() {
  const capture = { events: [], dropped: 0, truncated: false, bytes: 0 };
  capture.record = (matchingSteps, msg, startedAt) => {
    const json = JSON.stringify(msg.params || {});
    const oversized = Buffer.byteLength(json) > RAW_EVENT_PARAMS_BYTES_CAP;
    const size = oversized ? RAW_EVENT_PARAMS_BYTES_CAP : Buffer.byteLength(json);
    const step = matchingSteps.find((s) => s.count < Math.min(s.limit === null ? RAW_EVENT_LIMIT_DEFAULT : s.limit, RAW_EVENT_LIMIT_MAX));
    if (!step || capture.bytes + size > RAW_EVENTS_BYTES_CAP) {
      capture.dropped++;
      capture.truncated = true;
      return;
    }
    step.count++;
    capture.bytes += size;
    const event = { event: msg.method, t_ms: Date.now() - startedAt, params: oversized ? { __truncated: true, bytes: Buffer.byteLength(json), keys: Object.keys(msg.params || {}) } : msg.params };
    if (msg.sessionId) event.session = msg.sessionId;
    capture.events.push(event);
  };
  return capture;
}

function isBrowserLevelRawMethod(method) {
  const [domain, name] = method.split('.');
  return RAW_BROWSER_LEVEL_DOMAINS.has(domain) || (domain === 'Target' && RAW_BROWSER_LEVEL_TARGET_METHODS.has(name));
}

function rawRouter(sess, endpoint, onNotification) {
  const router = { browserConnection: null, attached: new Map() };
  const browser = async () => {
    if (router.browserConnection) return router.browserConnection;
    const version = await httpJson(cdpUrl(endpoint, '/json/version'), 2000);
    if (!version || !version.webSocketDebuggerUrl) throw new Error(`CDP endpoint ${endpoint} did not answer /json/version, so browser-level commands (Browser.*, SystemInfo.*, Target.createTarget and friends) cannot be sent`);
    router.browserConnection = await cdpSession(version.webSocketDebuggerUrl, 5000);
    router.browserConnection.onIdLessNotification = onNotification;
    return router.browserConnection;
  };
  router.send = async (method, params, targetId) => {
    if (targetId) {
      const connection = await browser();
      if (!router.attached.has(targetId)) router.attached.set(targetId, (await connection.send('Target.attachToTarget', { targetId, flatten: true })).sessionId);
      const result = await connection.send(method, params, router.attached.get(targetId));
      if (method === 'Page.close') router.attached.delete(targetId);
      return result;
    }
    if (method === 'Target.closeTarget' && params && params.targetId) router.attached.delete(params.targetId);
    if (isBrowserLevelRawMethod(method)) return (await browser()).send(method, params);
    return sess.send(method, params);
  };
  router.close = () => { if (router.browserConnection) router.browserConnection.close(); };
  return router;
}

function raceRawDeadline(promise, deadline) {
  let timer;
  const expired = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error('no answer before the dispatch deadline')), Math.max(0, deadline - Date.now()));
  });
  return Promise.race([promise, expired]).finally(() => clearTimeout(timer));
}

async function runCdpRaw(sess, script, watchdogAt, endpoint, reapply) {
  const deadline = watchdogAt - RAW_DEADLINE_MARGIN_MS;
  const steps = parseRawBody(script);
  if (!steps.some((s) => s.kind === 'cdp')) throw new Error('the cdp raw body holds no `cdp <Domain.method>` block');
  const eventSteps = steps.filter((s) => s.kind === 'events');
  const waitSteps = steps.filter((s) => s.kind === 'waitEvent');
  const startedAt = Date.now();
  const capture = rawEventCapture();
  let arrivalSeq = 0;
  const onNotification = (msg) => {
    arrivalSeq++;
    for (const step of waitSteps) {
      if (step.name === msg.method && rawMatches(step.where, msg.params)) step.lastMatchSeq = arrivalSeq;
    }
    const matchingSteps = eventSteps.filter((step) => step.names.includes(msg.method) && rawMatches(step.where, msg.params));
    if (matchingSteps.length) capture.record(matchingSteps, msg, startedAt);
  };
  sess.onIdLessNotification = onNotification;
  const router = rawRouter(sess, endpoint, onNotification);
  const explicitDomains = new Set(steps.filter((s) => s.kind === 'cdp' && s.method.endsWith('.enable')).map((s) => s.method.split('.')[0]));
  const wantedDomains = new Set([...eventSteps.flatMap((s) => s.names), ...waitSteps.map((s) => s.name)].map((name) => name.split('.')[0]));
  const autoEnabled = [...wantedDomains].filter((d) => RAW_AUTO_ENABLED_DOMAINS.has(d) && !explicitDomains.has(d));
  const results = [];
  const context = { byIndex: [null], byName: new Map() };
  const bareResults = steps.every((s) => s.kind === 'cdp');
  const shaped = (forPartial) => (bareResults && !forPartial
    ? (results.length === 1 ? results[0] : results)
    : { results, events: capture.events, events_dropped: capture.dropped, events_truncated: capture.truncated, auto_enabled: autoEnabled, elapsed_ms: Date.now() - startedAt });
  let lastBlockStartSeq = 0;
  try {
    for (const domain of autoEnabled) await raceRawDeadline(sess.send(`${domain}.enable`, {}), deadline);
    for (const step of steps) {
      if (step.kind === 'cdp') {
        lastBlockStartSeq = arrivalSeq;
        const index = results.length + 1;
        const where = `block ${index} (${step.method})`;
        let params = {};
        if (step.paramsText.trim()) {
          try { params = JSON.parse(step.paramsText); } catch (e) { throw new Error(`${where}: params starting on line ${step.paramsStartLine} are not valid JSON: ${e.message}`); }
        }
        params = resolveRawReferences(params, context, where);
        const targetId = step.on ? resolveRawReferences(step.on, context, where) : null;
        let result;
        try { result = await raceRawDeadline(router.send(step.method, params, targetId), deadline); } catch (e) {
          const message = String(e && e.message || e);
          throw new Error(`${where} failed: ${message}${/not enabled/i.test(message) ? NOT_ENABLED_HINT : ''}`);
        }
        results.push(result);
        const block = { index, method: step.method, name: step.name, result };
        context.byIndex[index] = block;
        if (step.name) context.byName.set(step.name, block);
        if (reapply && NAVIGATING_RAW_METHODS.has(step.method)) await reapply();
      } else if (step.kind === 'sleep') {
        await new Promise((r) => setTimeout(r, Math.max(0, Math.min(step.ms, deadline - Date.now()))));
      } else if (step.kind === 'waitEvent') {
        const budget = step.timeoutMs === null ? RAW_WAIT_TIMEOUT_DEFAULT_MS : step.timeoutMs;
        const until = Date.now() + Math.max(0, Math.min(budget, deadline - Date.now()));
        while (step.lastMatchSeq <= lastBlockStartSeq) {
          if (Date.now() >= until) throw new Error(`line ${step.line}: \`wait ${step.name}\` saw no matching event since the previous cdp block started, within ${budget}ms or the dispatch deadline`);
          await new Promise((r) => setTimeout(r, RAW_POLL_MS));
        }
      }
    }
  } catch (e) {
    e.partial = shaped(true);
    throw e;
  } finally {
    router.close();
  }
  return shaped(false);
}


async function main() {
  const startedAt = Date.now();
  const cfg = JSON.parse(process.argv[2]);
  const { port, cdpEndpoint, startUrl, targetId, scriptFile, resultFile, timeoutMs, mode, artifactFile, viewport, claimFreshTarget, glCapture, gpuProbeFile, wantGpu, uncapped, inputIsolation } = cfg;
  const endpoint = cdpEndpoint || `http://127.0.0.1:${port}`;
  const script = fs.readFileSync(scriptFile, 'utf-8');
  const target = await pickPageTarget(endpoint, startUrl, targetId, Math.min(timeoutMs, 30000), claimFreshTarget === true);
  if (!target) {
    fs.writeFileSync(resultFile, JSON.stringify({ __cdpError: 'no page target on CDP endpoint' }));
    process.stderr.write('cdp-eval: no page target\n');
    process.exit(1);
  }
  fs.writeFileSync(resultFile, JSON.stringify({ __cdpError: 'cdp helper exited before the evaluation settled', __targetId: target.id }));
  let resultWritten = false;
  let sessionSetup = null;
  const writeResult = (envelope) => {
    resultWritten = true;
    fs.writeFileSync(resultFile, JSON.stringify({ ...envelope, __session_setup: sessionSetup, __targetId: target.id }));
  };
  const HOST_KILL_MARGIN_MS = Math.max(3000, Math.min(15000, Math.round(timeoutMs / 20)));
  const watchdogDeadline = Math.max(500, timeoutMs - HOST_KILL_MARGIN_MS);
  const watchdogTimer = setTimeout(() => {
    if (resultWritten) return;
    writeResult({
      __cdpError: `evaluate did not settle within ${timeoutMs}ms (awaitPromise still pending when the pre-kill watchdog fired) -- the awaited script is genuinely slower than timeoutMs, not a marshaling bug; raise timeoutMs to observe its real completion`,
    });
    process.stderr.write('cdp-eval: watchdog force-wrote pending-evaluate result before host kill\n');
    process.exit(1);
  }, watchdogDeadline);
  watchdogTimer.unref();
  const sess = resumableSession(endpoint, target, target.__liveSession || await cdpSession(target.webSocketDebuggerUrl, timeoutMs));
  try {
    const setup = sessionSetupApplier(sess, viewport, inputIsolation);
    sessionSetup = setup.applied;
    const reapplySetup = setup.apply;
    if (mode !== 'gpu') await setup.apply();
    if (mode === 'gpu') {
      const report = await gpuReport(endpoint, fs.readFileSync(gpuProbeFile, 'utf-8'), wantGpu, uncapped === true);
      if (uncapped === true) await sess.send('Page.bringToFront', {}).catch(() => {});
      writeResult({ result: report });
      sess.close();
      process.exit(0);
    }
    if (mode === 'cdpraw') {
      let rawDocument = null;
      if (startUrl) {
        rawDocument = await ensureDocumentForUrl(sess, startUrl, timeoutMs, reapplySetup);
        if (rawDocument.navigation_failure) {
          writeResult({ __cdpError: `page navigation failed: ${rawDocument.navigation_failure} (url=${startUrl})`, __document: rawDocument });
          process.stderr.write(`cdp-eval: ${rawDocument.navigation_failure}\n`);
          sess.close();
          process.exit(1);
        }
      }
      writeResult({ result: await runCdpRaw(sess, script, startedAt + watchdogDeadline, endpoint, reapplySetup), __document: rawDocument });
      sess.close();
      process.exit(0);
    }
    const instrumented = mode === 'capture' || mode === 'profile' || mode === 'trace';
    if (instrumented) await sess.send('Runtime.enable', {});
    await sess.send('Page.enable', {});
    await sess.send('Emulation.setFocusEmulationEnabled', { enabled: true }).catch(() => {});
    if (uncapped === true) await sess.send('Page.bringToFront', {}).catch(() => {});
    const collectDebug = instrumented ? await attachDebugCapture(sess, glCapture === true) : async () => undefined;

    if (mode === 'capture') {
      const res = await navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs, reapplySetup, true);
      const debug = await collectDebug();
      if (res.exceptionDetails) {
        const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'evaluate exception';
        writeResult({ __cdpError: msg, debug: await collectDebug(), __document: res.__document });
        process.stderr.write(`cdp-eval: exception ${msg}\n`);
        sess.close();
        process.exit(1);
      }
      const value = res.result && ('value' in res.result) ? res.result.value : null;
      const envelope = { result: value === undefined ? null : value, result_note: resultNoteFor(res, value), debug, __document: res.__document };
      writeResult(envelope);
      sess.close();
      process.exit(0);
    }

    if (mode === 'profile') {
      await sess.send('Profiler.enable', {});
      await sess.send('Profiler.setSamplingInterval', { interval: 100 });
      await sess.send('Profiler.start', {});
      const res = await navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs, reapplySetup, true);
      const stopRes = await sess.send('Profiler.stop', {});
      const agg = aggregateCpuProfile(stopRes && stopRes.profile, 20);
      const debug = await collectDebug();
      if (res.exceptionDetails) {
        const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'evaluate exception';
        writeResult({ __cdpError: msg, debug: await collectDebug(), __document: res.__document });
        process.stderr.write(`cdp-eval: exception ${msg}\n`);
        sess.close();
        process.exit(1);
      }
      const value = res.result && ('value' in res.result) ? res.result.value : null;
      const envelope = { result: value === undefined ? null : value, profile: agg, debug, __document: res.__document };
      writeResult(envelope);
      if (artifactFile) { try { fs.writeFileSync(artifactFile, JSON.stringify(stopRes && stopRes.profile || {})); } catch (_) {} }
      sess.close();
      process.exit(0);
    }

    if (mode === 'trace') {
      const recording = await startTraceRecording(sess);
      const w0 = Date.now();
      const res = await navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs, reapplySetup, true);
      const wallUs = (Date.now() - w0) * 1000;
      const flushDeadline = startedAt + watchdogDeadline - TRACE_FLUSH_WATCHDOG_MARGIN_MS;
      const captured = await stopTraceRecordingToFile(sess, recording, flushDeadline, artifactFile);
      if (res.exceptionDetails) {
        const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'evaluate exception';
        writeResult({ __cdpError: msg, debug: await collectDebug(), __document: res.__document });
        process.stderr.write(`cdp-eval: exception ${msg}\n`);
        sess.close();
        process.exit(1);
      }
      const value = res.result && ('value' in res.result) ? res.result.value : null;
      const debug = await collectDebug();
      const envelope = { result: value === undefined ? null : value, trace: { wall_us: wallUs, ...summarizeTrace(captured) }, debug, __document: res.__document };
      writeResult(envelope);
      sess.close();
      process.exit(0);
    }

    if (mode === 'screenshot') {
      const res = await navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs, reapplySetup);
      if (res.exceptionDetails) {
        const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'evaluate exception';
        writeResult({ __cdpError: msg, debug: await collectDebug(), __document: res.__document });
        process.stderr.write(`cdp-eval: exception ${msg}\n`);
        sess.close();
        process.exit(1);
      }
      const value = res.result && ('value' in res.result) ? res.result.value : null;
      let screenshotError = null;
      try {
        const shot = await sess.send('Page.captureScreenshot', { format: 'png' });
        if (shot && shot.data && artifactFile) {
          fs.writeFileSync(artifactFile, Buffer.from(shot.data, 'base64'));
        } else if (!shot || !shot.data) {
          screenshotError = 'Page.captureScreenshot returned no image data';
        }
      } catch (e) {
        screenshotError = String(e && e.message || e);
      }
      const debug = await collectDebug();
      const envelope = { result: value === undefined ? null : value, screenshot_error: screenshotError, debug, __document: res.__document };
      writeResult(envelope);
      sess.close();
      process.exit(0);
    }

    if (mode === 'dom') {
      const selector = cfg.domSelector || '';
      const domScript = `
        const __els = Array.from(document.querySelectorAll(${JSON.stringify(selector)})).slice(0, 20);
        return __els.map((el) => {
          const rect = el.getBoundingClientRect();
          const style = window.getComputedStyle(el);
          const attrs = {};
          for (const a of el.attributes) attrs[a.name] = a.value;
          return {
            tag: el.tagName.toLowerCase(),
            text: (el.textContent || '').trim().slice(0, 200),
            attrs,
            visible: style.display !== 'none' && style.visibility !== 'hidden' && rect.width > 0 && rect.height > 0,
            rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
          };
        });
      `;
      const wrapped = `(async () => { try { ${domScript} } catch (__e) { return { __domError: String(__e && __e.message || __e) }; } })()`;
      const res = await navigateIfNeededThenEvaluateOverCdp(sess, wrapped, startUrl, timeoutMs, reapplySetup);
      if (res.exceptionDetails) {
        const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'evaluate exception';
        writeResult({ __cdpError: msg, debug: await collectDebug(), __document: res.__document });
        process.stderr.write(`cdp-eval: exception ${msg}\n`);
        sess.close();
        process.exit(1);
      }
      const value = res.result && ('value' in res.result) ? res.result.value : null;
      const debug = await collectDebug();
      let envelope;
      if (value && value.__domError) {
        envelope = { match_count: 0, elements: [], error: value.__domError, debug, __document: res.__document };
      } else {
        const elements = Array.isArray(value) ? value : [];
        envelope = { match_count: elements.length, elements, debug, __document: res.__document };
      }
      writeResult(envelope);
      sess.close();
      process.exit(0);
    }

    const res = await navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs, reapplySetup);
    const debug = await collectDebug();
    if (res.exceptionDetails) {
      const msg = res.exceptionDetails.exception && res.exceptionDetails.exception.description
        ? res.exceptionDetails.exception.description
        : (res.exceptionDetails.text || 'evaluate exception');
      writeResult({ __cdpError: msg, debug, __document: res.__document });
      process.stderr.write(`cdp-eval: exception ${msg}\n`);
      sess.close();
      process.exit(1);
    }
    const value = res.result && ('value' in res.result) ? res.result.value : null;
    writeResult({ result: value === undefined ? null : value, result_note: resultNoteFor(res, value), debug, __document: res.__document });
    sess.close();
    process.exit(0);
  } catch (e) {
    writeResult({ __cdpError: String(e && e.message || e), partial: e && e.partial });
    process.stderr.write(`cdp-eval: ${e && e.message || e}\n`);
    try { sess.close(); } catch (_) {}
    process.exit(1);
  }
}

main();
