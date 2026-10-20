// Debugger (parity rows 55 to 60). Concatenated into the browser helper after browser_cdp_tools.mjs,
// so every top-level name carries the dbg or DBG_ prefix. Argument names follow the Debugger class
// in docs/browser-parity.md section 1.5. Step shape: { op: 'debugger', action, ...arguments }.
// Breakpoint lines are zero-based, the CDP convention. Breakpoint, pause and blackbox state lives
// on the call's CDP session; it outlives a call only once the session file keeps it.

const DBG_PAUSE_STATES = new Set(['none', 'uncaught', 'all']);
const DBG_STEP_METHODS = {
  step_over: 'Debugger.stepOver',
  step_into: 'Debugger.stepInto',
  step_out: 'Debugger.stepOut',
  resume: 'Debugger.resume',
};
const DBG_ACTIONS = new Set([
  'enable', 'set_breakpoint', 'delete_breakpoint', 'list_breakpoints',
  'step_over', 'step_into', 'step_out', 'resume', 'is_paused', 'get_location',
  'set_pause_on_exceptions', 'evaluate', 'inspect_local', 'inspect_global', 'list_scripts',
  'set_xhr_breakpoint', 'remove_xhr_breakpoint', 'list_xhr_breakpoints',
  'set_blackbox_patterns', 'add_blackbox_pattern', 'remove_blackbox_pattern', 'list_blackbox_patterns',
]);
const DBG_MAX_SCOPE_VARIABLES = 500;

// Debugger state for this call: the script list is filled by Debugger.scriptParsed, which
// Debugger.enable replays for scripts already loaded.
async function dbgState(ctx) {
  if (ctx.debugger) return ctx.debugger;
  const state = {
    scripts: new Map(),
    breakpoints: new Map(),
    paused: null,
    xhr: new Set(),
    blackbox: [],
  };
  ctx.debugger = state;
  ctx.session.on('Debugger.scriptParsed', (p) => {
    state.scripts.set(p.scriptId, { scriptId: p.scriptId, url: p.url || '', length: p.length || 0 });
  });
  ctx.session.on('Debugger.paused', (p) => { state.paused = p; });
  ctx.session.on('Debugger.resumed', () => { state.paused = null; });
  await ctx.session.send('Debugger.enable');
  return state;
}

function dbgRequirePaused(state) {
  if (!state.paused) throw new Error('the page is not paused: set a breakpoint and trigger it first');
  return state.paused;
}

function dbgWhole(value, what) {
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw new Error(`${what} must be a whole number of 0 or more, got '${value}'`);
  }
  return parsed;
}

function dbgText(step, field, what) {
  if (typeof step[field] !== 'string' || step[field] === '') throw new Error(`${what} needs ${field}`);
  return step[field];
}

function dbgCallValue(value) {
  if (!value) return null;
  if (value.value !== undefined) return value.value;
  return value.description !== undefined ? value.description : value.type;
}

async function dbgScopeVariables(ctx, frame, scopeType) {
  const scope = (frame.scopeChain || []).find((item) => item.type === scopeType);
  if (!scope) throw new Error(`the paused frame has no ${scopeType} scope`);
  const { result } = await ctx.session.send('Runtime.getProperties', {
    objectId: scope.object.objectId,
    ownProperties: true,
  });
  const variables = {};
  let count = 0;
  for (const prop of result || []) {
    if (!prop.value) continue;
    count += 1;
    if (count <= DBG_MAX_SCOPE_VARIABLES) variables[prop.name] = dbgCallValue(prop.value);
  }
  return { count, truncated: count > DBG_MAX_SCOPE_VARIABLES, variables };
}

async function dbgSyncBlackbox(session, state) {
  await session.send('Debugger.setBlackboxPatterns', { patterns: state.blackbox });
}

async function debuggerStep(ctx, step) {
  const action = String(step.action || '');
  if (!DBG_ACTIONS.has(action)) {
    throw new Error(`debugger action '${step.action}' is not one of: ${[...DBG_ACTIONS].join(', ')}`);
  }
  const state = await dbgState(ctx);
  const s = ctx.session;

  if (DBG_STEP_METHODS[action]) {
    dbgRequirePaused(state);
    state.paused = null;
    await s.send(DBG_STEP_METHODS[action]);
    return { op: 'debugger', action };
  }

  switch (action) {
    case 'enable':
      return { op: 'debugger', action, enabled: true };
    case 'set_breakpoint': {
      const file = dbgText(step, 'file', 'set_breakpoint');
      const line = dbgWhole(step.line, 'line');
      const params = { url: file, lineNumber: line, columnNumber: 0 };
      if (step.condition !== undefined) params.condition = String(step.condition);
      const reply = await s.send('Debugger.setBreakpointByUrl', params);
      state.breakpoints.set(reply.breakpointId, {
        breakpointId: reply.breakpointId,
        file,
        line,
        condition: params.condition || null,
      });
      return { op: 'debugger', action, breakpointId: reply.breakpointId, locations: reply.locations || [] };
    }
    case 'delete_breakpoint': {
      const breakpointId = dbgText(step, 'breakpointId', 'delete_breakpoint');
      await s.send('Debugger.removeBreakpoint', { breakpointId });
      state.breakpoints.delete(breakpointId);
      return { op: 'debugger', action, breakpointId };
    }
    case 'list_breakpoints':
      return { op: 'debugger', action, breakpoints: [...state.breakpoints.values()] };
    case 'is_paused':
      return { op: 'debugger', action, paused: state.paused !== null };
    case 'get_location': {
      const frame = dbgRequirePaused(state).callFrames[0];
      return { op: 'debugger', action, url: frame.url, functionName: frame.functionName || '', location: frame.location };
    }
    case 'set_pause_on_exceptions': {
      const pauseState = String(step.state || '');
      if (!DBG_PAUSE_STATES.has(pauseState)) {
        throw new Error(`pause on exceptions state must be none, uncaught or all, got '${step.state}'`);
      }
      await s.send('Debugger.setPauseOnExceptions', { state: pauseState });
      return { op: 'debugger', action, state: pauseState };
    }
    case 'evaluate': {
      const expression = dbgText(step, 'expression', 'evaluate');
      const frame = dbgRequirePaused(state).callFrames[0];
      const reply = await s.send('Debugger.evaluateOnCallFrame', {
        callFrameId: frame.callFrameId,
        expression,
        returnByValue: true,
      });
      if (reply.exceptionDetails) {
        const ex = reply.exceptionDetails.exception;
        throw new Error(`evaluate threw: ${(ex && ex.description) || reply.exceptionDetails.text}`);
      }
      return { op: 'debugger', action, expression, value: dbgCallValue(reply.result) };
    }
    case 'inspect_local':
    case 'inspect_global': {
      const frame = dbgRequirePaused(state).callFrames[0];
      const scopeType = action === 'inspect_local' ? 'local' : 'global';
      return { op: 'debugger', action, scope: scopeType, ...(await dbgScopeVariables(ctx, frame, scopeType)) };
    }
    case 'list_scripts': {
      const matches = step.search === undefined || step.search === '' ? () => true : searchMatcher(step.search);
      const scripts = [...state.scripts.values()].filter((script) => matches(script.url));
      return { op: 'debugger', action, count: scripts.length, scripts };
    }
    case 'set_xhr_breakpoint': {
      const url = dbgText(step, 'url', 'set_xhr_breakpoint');
      await s.send('DOMDebugger.setXHRBreakpoint', { url });
      state.xhr.add(url);
      return { op: 'debugger', action, url };
    }
    case 'remove_xhr_breakpoint': {
      const url = dbgText(step, 'url', 'remove_xhr_breakpoint');
      await s.send('DOMDebugger.removeXHRBreakpoint', { url });
      state.xhr.delete(url);
      return { op: 'debugger', action, url };
    }
    case 'list_xhr_breakpoints':
      return { op: 'debugger', action, urls: [...state.xhr] };
    case 'set_blackbox_patterns': {
      if (!Array.isArray(step.patterns) || !step.patterns.every((p) => typeof p === 'string')) {
        throw new Error('set_blackbox_patterns needs patterns, a list of strings');
      }
      state.blackbox = [...step.patterns];
      await dbgSyncBlackbox(s, state);
      return { op: 'debugger', action, patterns: state.blackbox };
    }
    case 'add_blackbox_pattern': {
      const pattern = dbgText(step, 'pattern', 'add_blackbox_pattern');
      if (!state.blackbox.includes(pattern)) state.blackbox.push(pattern);
      await dbgSyncBlackbox(s, state);
      return { op: 'debugger', action, patterns: state.blackbox };
    }
    case 'remove_blackbox_pattern': {
      const pattern = dbgText(step, 'pattern', 'remove_blackbox_pattern');
      state.blackbox = state.blackbox.filter((item) => item !== pattern);
      await dbgSyncBlackbox(s, state);
      return { op: 'debugger', action, patterns: state.blackbox };
    }
    case 'list_blackbox_patterns':
      return { op: 'debugger', action, patterns: state.blackbox };
    default:
      throw new Error(`unhandled debugger action '${action}'`);
  }
}
