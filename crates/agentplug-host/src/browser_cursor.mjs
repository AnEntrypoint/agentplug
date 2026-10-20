// Ghost cursor overlay (parity row 54). Concatenated into the browser helper after
// browser_cdp_tools.mjs, so every top-level name here carries the GHOST_ or cursor prefix.
// Step shape: { op: 'cursor', action: 'show' | 'hide' }.

const GHOST_CURSOR_ID = '__gm_ghost_cursor';
const GHOST_CURSOR_ACTIONS = new Set(['show', 'hide']);

const GHOST_CURSOR_SHOW_SCRIPT = `(() => {
  if (!document.documentElement) return false;
  let el = document.getElementById('${GHOST_CURSOR_ID}');
  if (!el) {
    el = document.createElement('div');
    el.id = '${GHOST_CURSOR_ID}';
    el.setAttribute('aria-hidden', 'true');
    el.style.cssText = 'position:fixed;left:0;top:0;width:16px;height:16px;margin:-8px 0 0 -8px;border-radius:50%;background:rgba(255,64,64,0.55);border:2px solid #fff;box-sizing:border-box;pointer-events:none;z-index:2147483647;';
    document.documentElement.appendChild(el);
    document.addEventListener('mousemove', (e) => {
      el.style.left = e.clientX + 'px';
      el.style.top = e.clientY + 'px';
    }, true);
  }
  el.style.display = 'block';
  return true;
})()`;

const GHOST_CURSOR_HIDE_SCRIPT = `(() => {
  const el = document.getElementById('${GHOST_CURSOR_ID}');
  if (el) el.style.display = 'none';
  return el !== null;
})()`;

async function cursorStep(ctx, step) {
  const action = String(step.action || '');
  if (!GHOST_CURSOR_ACTIONS.has(action)) {
    throw new Error(`cursor action must be show or hide, got '${step.action}'`);
  }
  const script = action === 'show' ? GHOST_CURSOR_SHOW_SCRIPT : GHOST_CURSOR_HIDE_SCRIPT;
  const changed = await ctx.host.evaluate(ctx.session, script);
  return { op: 'cursor', action, changed: changed === true };
}
