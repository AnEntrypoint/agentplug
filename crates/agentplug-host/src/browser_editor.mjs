// Script editor (parity rows 61 and 62). Concatenated into the browser helper after browser_cdp_tools.mjs
// and browser_debugger.mjs, so every top-level name carries the edt or EDT_ prefix. Argument names
// follow the Editor class in docs/browser-parity.md section 1.6. Step shape:
// { op: 'editor', action: 'list' | 'read' | 'grep' | 'edit' | 'write', ...arguments }.

const EDT_ACTIONS = new Set(['list', 'read', 'grep', 'edit', 'write']);
const EDT_DEFAULT_READ_LIMIT = 2000;

function edtCount(value, what, fallback) {
  if (value === undefined) return fallback;
  const parsed = Number(value);
  if (!Number.isInteger(parsed) || parsed < 0) {
    throw new Error(`${what} must be a whole number of 0 or more, got '${value}'`);
  }
  return parsed;
}

function edtFilter(state, pattern) {
  const matches = pattern === undefined || pattern === '' ? () => true : searchMatcher(pattern);
  return [...state.scripts.values()].filter((script) => matches(script.url));
}

function edtScriptByUrl(state, url) {
  if (typeof url !== 'string' || url === '') throw new Error('editor action needs url, the script url');
  const script = [...state.scripts.values()].find((item) => item.url === url);
  if (!script) throw new Error(`no script loaded at ${url}: list the scripts to see the loaded urls`);
  return script;
}

async function edtSource(ctx, script) {
  const reply = await ctx.session.send('Debugger.getScriptSource', { scriptId: script.scriptId });
  return reply.scriptSource;
}

// Replaces the whole script source through Debugger.setScriptSource. dryRun asks the page to
// check the new source without applying it.
async function edtApply(ctx, action, script, scriptSource, dryRun) {
  const reply = await ctx.session.send('Debugger.setScriptSource', {
    scriptId: script.scriptId,
    scriptSource,
    dryRun,
  });
  return {
    op: 'editor',
    action,
    url: script.url,
    dryRun,
    status: reply.status || 'Ok',
    length: scriptSource.length,
  };
}

async function editorStep(ctx, step) {
  const action = String(step.action || '');
  if (!EDT_ACTIONS.has(action)) {
    throw new Error(`editor action '${step.action}' is not one of: list, read, grep, edit, write`);
  }
  const state = await dbgState(ctx);

  switch (action) {
    case 'list': {
      const scripts = edtFilter(state, step.pattern);
      return { op: 'editor', action, count: scripts.length, scripts };
    }
    case 'read': {
      const script = edtScriptByUrl(state, step.url);
      const offset = edtCount(step.offset, 'offset', 0);
      const limit = edtCount(step.limit, 'limit', EDT_DEFAULT_READ_LIMIT);
      const lines = (await edtSource(ctx, script)).split('\n');
      const lineSlice = lines.slice(offset, offset + limit).map((text, index) => ({
        lineNumber: offset + index + 1,
        text,
      }));
      return { op: 'editor', action, url: script.url, offset, limit, total: lines.length, lines: lineSlice };
    }
    case 'grep': {
      if (typeof step.regex !== 'string' || step.regex === '') throw new Error('grep needs regex');
      const hits = [];
      for (const script of edtFilter(state, step.pattern)) {
        const { result } = await ctx.session.send('Debugger.searchInContent', {
          scriptId: script.scriptId,
          query: step.regex,
          caseSensitive: false,
          isRegex: true,
        });
        for (const hit of result || []) {
          hits.push({ url: script.url, lineNumber: hit.lineNumber, lineContent: hit.lineContent });
        }
      }
      return { op: 'editor', action, regex: step.regex, count: hits.length, matches: hits };
    }
    case 'edit': {
      const script = edtScriptByUrl(state, step.url);
      if (typeof step.oldString !== 'string' || step.oldString === '') throw new Error('edit needs oldString');
      if (typeof step.newString !== 'string') throw new Error('edit needs newString');
      const source = await edtSource(ctx, script);
      const occurrences = source.split(step.oldString).length - 1;
      if (occurrences !== 1) {
        throw new Error(`oldString matches ${occurrences} times in ${script.url}: it must match exactly once`);
      }
      const next = source.replace(step.oldString, () => step.newString);
      return edtApply(ctx, action, script, next, step.dryRun === true);
    }
    case 'write': {
      const script = edtScriptByUrl(state, step.url);
      if (typeof step.content !== 'string') throw new Error('write needs content');
      return edtApply(ctx, action, script, step.content, step.dryRun === true);
    }
    default:
      throw new Error(`unhandled editor action '${action}'`);
  }
}
