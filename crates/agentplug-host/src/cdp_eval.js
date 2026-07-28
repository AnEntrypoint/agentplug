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

async function pickPageTarget(port, startUrl, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const list = await httpJson(`http://127.0.0.1:${port}/json/list`, 2000);
    if (Array.isArray(list)) {
      const page = list.find((t) => t.type === 'page' && t.webSocketDebuggerUrl);
      if (page) return page;
    }
    if (startUrl) {
      const created = await httpJson(`http://127.0.0.1:${port}/json/new?${encodeURIComponent(startUrl)}`, 3000);
      if (created && created.webSocketDebuggerUrl) return created;
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
    const pending = new Map();
    const timer = setTimeout(() => { try { ws.close(); } catch (_) {} reject(new Error('cdp timeout')); }, timeoutMs);
    const rejectAllPendingSendsOnSocketDrop = (reason) => {
      for (const { rej } of pending.values()) rej(new Error(reason));
      pending.clear();
    };
    const sessObj = {
      send(method, params) {
        const id = nextId++;
        return new Promise((res, rej) => {
          pending.set(id, { res, rej });
          ws.send(JSON.stringify({ id, method, params: params || {} }));
        });
      },
      close() { clearTimeout(timer); try { ws.close(); } catch (_) {} },
      onIdLessNotification: null,
    };
    ws.addEventListener('open', () => { opened = true; resolve(sessObj); });
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

async function navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs) {
  if (startUrl) {
    await sess.send('Page.enable', {});
    await sess.send('Page.navigate', { url: startUrl });
    await new Promise((r) => setTimeout(r, 1200));
  }
  const wrapped = `(async () => { ${script} })()`;
  return sess.send('Runtime.evaluate', {
    expression: wrapped, awaitPromise: true, returnByValue: true, userGesture: true, timeout: timeoutMs,
  });
}

const GL_ERROR_TRACKING_INIT_SCRIPT = `
(() => {
  const MAX_SIGNATURES = 40;
  window.__gmGlErrors = window.__gmGlErrors || {};
  window.__gmGlDrawCalls = window.__gmGlDrawCalls || {};
  window.__gmGlErrorTotalCount = window.__gmGlErrorTotalCount || 0;
  window.__gmGlLastDrainedError = null;
  const drawFns = ['drawArrays', 'drawElements', 'drawArraysInstanced', 'drawElementsInstanced'];
  const origGetContext = HTMLCanvasElement.prototype.getContext;
  HTMLCanvasElement.prototype.getContext = function (type, ...rest) {
    const gl = origGetContext.call(this, type, ...rest);
    if (!gl || !/^webgl/.test(type) && type !== 'experimental-webgl') return gl;
    for (const fnName of drawFns) {
      const orig = gl[fnName];
      if (typeof orig !== 'function' || orig.__gmWrapped) continue;
      const wrapped = function (...args) {
        const result = orig.apply(this, args);
        window.__gmGlDrawCalls[fnName] = (window.__gmGlDrawCalls[fnName] || 0) + 1;
        const err = gl.getError();
        window.__gmGlLastDrainedError = err;
        if (err !== gl.NO_ERROR) {
          window.__gmGlErrorTotalCount += 1;
          const mode = args[0];
          const count = args[1];
          const instanceCount = args[4];
          const sig = fnName + '|' + err + '|' + mode + '|' + count + '|' + (instanceCount || 0);
          const existing = window.__gmGlErrors[sig];
          if (existing) {
            existing.occurrenceCount += 1;
            existing.lastDrawCallIndex = window.__gmGlDrawCalls[fnName];
          } else if (Object.keys(window.__gmGlErrors).length < MAX_SIGNATURES) {
            window.__gmGlErrors[sig] = {
              fn: fnName, error: err, mode, count, instanceCount: instanceCount || 0,
              occurrenceCount: 1, lastDrawCallIndex: window.__gmGlDrawCalls[fnName],
              stack: new Error().stack,
            };
          }
        }
        return result;
      };
      wrapped.__gmWrapped = true;
      gl[fnName] = wrapped;
    }
    return gl;
  };
})();
`;

async function attachDebugCapture(sess) {
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
  await sess.send('Page.addScriptToEvaluateOnNewDocument', { source: GL_ERROR_TRACKING_INIT_SCRIPT });
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
    return { console: consoleLines, pageErrors, network: networkEvents, performance: performanceSnapshot, gl };
  };
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

async function main() {
  const cfg = JSON.parse(process.argv[2]);
  const { port, startUrl, scriptFile, resultFile, timeoutMs, mode, artifactFile, viewport } = cfg;
  const script = fs.readFileSync(scriptFile, 'utf-8');
  const target = await pickPageTarget(port, startUrl, Math.min(timeoutMs, 30000));
  if (!target) {
    fs.writeFileSync(resultFile, JSON.stringify({ __cdpError: 'no page target on CDP endpoint' }));
    process.stderr.write('cdp-eval: no page target\n');
    process.exit(1);
  }
  const sess = await cdpSession(target.webSocketDebuggerUrl, timeoutMs);
  try {
    await sess.send('Runtime.enable', {});
    await sess.send('Page.enable', {});
    const collectDebug = await attachDebugCapture(sess);

    if (viewport && viewport.width && viewport.height) {
      await sess.send('Emulation.setDeviceMetricsOverride', {
        width: viewport.width,
        height: viewport.height,
        deviceScaleFactor: viewport.deviceScaleFactor || 1,
        mobile: viewport.mobile !== false,
        screenWidth: viewport.width,
        screenHeight: viewport.height,
      });
      if (viewport.mobile !== false) {
        await sess.send('Emulation.setTouchEmulationEnabled', { enabled: true, maxTouchPoints: 5 }).catch(() => {});
      }
    }

    if (mode === 'capture') {
      const res = await navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs);
      const debug = await collectDebug();
      if (res.exceptionDetails) {
        const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'evaluate exception';
        fs.writeFileSync(resultFile, JSON.stringify({ __cdpError: msg }));
        process.stderr.write(`cdp-eval: exception ${msg}\n`);
        sess.close();
        process.exit(1);
      }
      const value = res.result && ('value' in res.result) ? res.result.value : null;
      const envelope = { result: value === undefined ? null : value, debug };
      fs.writeFileSync(resultFile, JSON.stringify(envelope));
      sess.close();
      process.exit(0);
    }

    if (mode === 'profile') {
      await sess.send('Profiler.enable', {});
      await sess.send('Profiler.setSamplingInterval', { interval: 100 });
      await sess.send('Profiler.start', {});
      const res = await navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs);
      const stopRes = await sess.send('Profiler.stop', {});
      const agg = aggregateCpuProfile(stopRes && stopRes.profile, 20);
      const debug = await collectDebug();
      if (res.exceptionDetails) {
        const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'evaluate exception';
        fs.writeFileSync(resultFile, JSON.stringify({ __cdpError: msg }));
        process.stderr.write(`cdp-eval: exception ${msg}\n`);
        sess.close();
        process.exit(1);
      }
      const value = res.result && ('value' in res.result) ? res.result.value : null;
      const envelope = { result: value === undefined ? null : value, profile: agg, debug };
      fs.writeFileSync(resultFile, JSON.stringify(envelope));
      if (artifactFile) { try { fs.writeFileSync(artifactFile, JSON.stringify(stopRes && stopRes.profile || {})); } catch (_) {} }
      sess.close();
      process.exit(0);
    }

    if (mode === 'trace') {
      const traceEvents = [];
      sess.onIdLessNotification = (msg) => {
        if (msg.method === 'Tracing.dataCollected') {
          for (const e of (msg.params.value || [])) traceEvents.push(e);
        }
      };
      await sess.send('Tracing.start', { categories: 'disabled-by-default-devtools.timeline,devtools.timeline,disabled-by-default-devtools.timeline.frame', transferMode: 'ReportEvents' });
      const w0 = Date.now();
      const res = await navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs);
      const wallUs = (Date.now() - w0) * 1000;
      const tracingDone = new Promise((resolve) => {
        const prevOnIdLessNotification = sess.onIdLessNotification;
        sess.onIdLessNotification = (msg) => {
          prevOnIdLessNotification(msg);
          if (msg.method === 'Tracing.tracingComplete') resolve();
        };
      });
      await sess.send('Tracing.end', {});
      await Promise.race([tracingDone, new Promise((r) => setTimeout(r, 5000))]);
      const byCategory = {};
      let gpuUs = 0, vizUs = 0, ccUs = 0;
      for (const e of traceEvents) {
        const cat = e.cat || 'unknown';
        const dur = e.dur || 0;
        byCategory[cat] = (byCategory[cat] || 0) + dur;
        if (/gpu/i.test(e.name || '') || /GPU/.test(cat)) gpuUs += dur;
        if (/composit/i.test(e.name || '')) ccUs += dur;
        if (/raster|paint|layer/i.test(e.name || '')) vizUs += dur;
      }
      if (res.exceptionDetails) {
        const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'evaluate exception';
        fs.writeFileSync(resultFile, JSON.stringify({ __cdpError: msg }));
        process.stderr.write(`cdp-eval: exception ${msg}\n`);
        sess.close();
        process.exit(1);
      }
      const value = res.result && ('value' in res.result) ? res.result.value : null;
      const debug = await collectDebug();
      const envelope = { result: value === undefined ? null : value, trace: { wall_us: wallUs, gpu_us: gpuUs, viz_us: vizUs, cc_us: ccUs, by_category: byCategory }, debug };
      fs.writeFileSync(resultFile, JSON.stringify(envelope));
      if (artifactFile) { try { fs.writeFileSync(artifactFile, JSON.stringify(traceEvents)); } catch (_) {} }
      sess.close();
      process.exit(0);
    }

    if (mode === 'screenshot') {
      const res = await navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs);
      if (res.exceptionDetails) {
        const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'evaluate exception';
        fs.writeFileSync(resultFile, JSON.stringify({ __cdpError: msg }));
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
      const envelope = { result: value === undefined ? null : value, screenshot_error: screenshotError, debug };
      fs.writeFileSync(resultFile, JSON.stringify(envelope));
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
      const res = await sess.send('Runtime.evaluate', { expression: wrapped, awaitPromise: true, returnByValue: true, userGesture: true, timeout: timeoutMs });
      if (res.exceptionDetails) {
        const msg = res.exceptionDetails.exception?.description || res.exceptionDetails.text || 'evaluate exception';
        fs.writeFileSync(resultFile, JSON.stringify({ __cdpError: msg }));
        process.stderr.write(`cdp-eval: exception ${msg}\n`);
        sess.close();
        process.exit(1);
      }
      const value = res.result && ('value' in res.result) ? res.result.value : null;
      const debug = await collectDebug();
      let envelope;
      if (value && value.__domError) {
        envelope = { match_count: 0, elements: [], error: value.__domError, debug };
      } else {
        const elements = Array.isArray(value) ? value : [];
        envelope = { match_count: elements.length, elements, debug };
      }
      fs.writeFileSync(resultFile, JSON.stringify(envelope));
      sess.close();
      process.exit(0);
    }

    const res = await navigateIfNeededThenEvaluateOverCdp(sess, script, startUrl, timeoutMs);
    const debug = await collectDebug();
    if (res.exceptionDetails) {
      const msg = res.exceptionDetails.exception && res.exceptionDetails.exception.description
        ? res.exceptionDetails.exception.description
        : (res.exceptionDetails.text || 'evaluate exception');
      fs.writeFileSync(resultFile, JSON.stringify({ __cdpError: msg, debug }));
      process.stderr.write(`cdp-eval: exception ${msg}\n`);
      sess.close();
      process.exit(1);
    }
    const value = res.result && ('value' in res.result) ? res.result.value : null;
    fs.writeFileSync(resultFile, JSON.stringify({ result: value === undefined ? null : value, debug }));
    sess.close();
    process.exit(0);
  } catch (e) {
    fs.writeFileSync(resultFile, JSON.stringify({ __cdpError: String(e && e.message || e) }));
    process.stderr.write(`cdp-eval: ${e && e.message || e}\n`);
    try { sess.close(); } catch (_) {}
    process.exit(1);
  }
}

main();
