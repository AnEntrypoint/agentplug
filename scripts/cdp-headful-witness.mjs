// Headful contract for the cdp engine. Static checks read the crawl sources; live
// checks drive the runner's browser lease and crawl verbs and inspect the Chrome they
// start. Usage: node scripts/cdp-headful-witness.mjs [--static-only] [--src=<dir>] [--root=<project root>]
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const options = Object.fromEntries(
  process.argv.slice(2).map((arg) => {
    const match = /^--([^=]+)(?:=(.*))?$/.exec(arg);
    return match ? [match[1], match[2] ?? 'true'] : [arg, 'true'];
  }),
);
const srcDir = path.resolve(options.src || path.join(here, '..', 'crates', 'agentplug-host', 'src'));
const root = (options.root || 'C:/dev/gm/agentplug').replace(/\\/g, '/');
const staticOnly = options['static-only'] === 'true';
const gmCli = path.join(os.homedir(), '.gm-tools', 'gm-mcp-server.mjs');
const session = `cdp-headful-witness-${process.pid}`;
const agent = session;
const results = [];

function check(name, ok, detail = '') {
  results.push({ name, ok: ok === true });
  console.log(`CHECK ${name}: ${ok === true ? 'PASS' : 'FAIL'}${detail ? ` (${detail})` : ''}`);
}

function readSource(name) {
  return fs.readFileSync(path.join(srcDir, name), 'utf8');
}

function fnBody(text, signature) {
  const start = text.indexOf(signature);
  if (start < 0) return null;
  const end = text.indexOf('\n}\n', start);
  return end < 0 ? text.slice(start) : text.slice(start, end + 3);
}

function staticChecks() {
  const lease = readSource('crawl_lease.rs');
  const crawl = readSource('crawl.rs');
  const helper = readSource('crawl_cdp.mjs');
  const launch = fnBody(lease, 'fn launch(root: &Path)');
  const shell = fnBody(lease, 'fn is_headless_shell(');
  const release = fnBody(lease, 'pub fn release(');
  const cdpParse = fnBody(crawl, 'pub fn parse_cdp_crawl_body(');
  const reap = fnBody(crawl, 'pub(crate) fn reap_stale_crawl_browsers(');
  const candidateBodies = [
    fnBody(crawl, 'fn windows_chrome_candidates('),
    fnBody(crawl, 'fn macos_chrome_candidates('),
    fnBody(crawl, 'fn linux_chrome_candidates('),
    fnBody(crawl, 'pub(crate) fn find_chrome('),
  ];

  check('static.launch_function_found', launch !== null);
  check('static.launch_spawns_debuggable_chrome', launch !== null && launch.includes('--remote-debugging-port={port}') && launch.includes('Command::new(&chrome)'));
  check('static.launch_passes_no_headless_flag', launch !== null && !launch.includes('--headless'));
  check('static.launch_refuses_headless_shell', launch !== null && launch.includes('is_headless_shell(&chrome)') && launch.includes('is a headless shell'));
  check('static.headless_shell_test_matches_name', shell !== null && shell.includes('"headless"'));
  check('static.cdp_parser_refuses_headless_lines', cdpParse !== null && cdpParse.includes('is_headless_request(line)') && cdpParse.includes('headless is refused: engine=cdp'));
  check('static.headless_request_covers_flag_forms', crawl.includes('lower == "headless"') && crawl.includes('starts_with("--headless")'));
  check('static.no_idle_linger_constant', !lease.includes('SHARED_BROWSER_IDLE_CLOSE'));
  check('static.release_closes_on_last_lease', release !== null && release.includes('last_lease_gone') && release.includes('close_browser'));
  check('static.helper_closes_its_own_target', helper.includes('/json/close/'));
  check('static.reaper_scoped_to_profile_marker', reap !== null && reap.includes('--user-data-dir') && reap.includes('--type=') && reap.includes('marker'));
  check('static.chrome_candidates_exclude_headless', candidateBodies.every((body) => body !== null && !/headless/i.test(body)));
}

function gm(verb, body, raw) {
  const args = [gmCli, 'dispatch', verb, '--cwd', root];
  if (raw !== undefined) args.push('--raw', raw);
  else args.push('--body', JSON.stringify({ SESSION_ID: session, ...body }));
  const run = spawnSync(process.execPath, args, { encoding: 'utf8', windowsHide: true, timeout: 150000, maxBuffer: 32 * 1024 * 1024 });
  return run.stdout || '';
}

function field(text, key) {
  const match = new RegExp(`^${key}: (.*)$`, 'm').exec(text);
  return match ? match[1].trim() : null;
}

function ps(script) {
  const run = spawnSync('powershell', ['-NoProfile', '-NonInteractive', '-Command', script], { encoding: 'utf8', windowsHide: true, timeout: 60000 });
  return (run.stdout || '').trim();
}

function processInfo(pid) {
  const out = ps(`$p = Get-CimInstance Win32_Process -Filter "ProcessId=${pid}"; if ($p) { $w = (Get-Process -Id ${pid} -ErrorAction SilentlyContinue).MainWindowHandle; [pscustomobject]@{ alive = $true; window = [int64]$w; cmd = [string]$p.CommandLine } | ConvertTo-Json -Compress } else { '{"alive":false}' }`);
  try {
    return JSON.parse(out);
  } catch (_) {
    return { alive: false };
  }
}

function killTree(pid) {
  spawnSync('taskkill', ['/PID', String(pid), '/T', '/F'], { windowsHide: true, stdio: 'ignore' });
}

function startChrome(chrome, dir) {
  const argList = ['--remote-debugging-port=0', `--user-data-dir=${dir}`, '--no-first-run', '--no-default-browser-check', 'about:blank'].join(' ');
  return Number(ps(`$p = Start-Process -FilePath '${chrome}' -ArgumentList '${argList}' -PassThru; $p.Id`));
}

function chromeExecutable() {
  const candidates = [process.env.GM_BROWSER_CHROME_PATH, process.env.CHROME_PATH, 'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe'];
  return candidates.find((candidate) => candidate && fs.existsSync(candidate)) || null;
}

async function waitFor(probe, ms, every = 500) {
  const started = Date.now();
  for (;;) {
    const value = await probe();
    if (value) return value;
    if (Date.now() - started > ms) return value;
    await new Promise((resolve) => setTimeout(resolve, every));
  }
}

async function fetchJson(url) {
  try {
    const res = await fetch(url, { signal: AbortSignal.timeout(3000) });
    return res.ok ? await res.json() : null;
  } catch (_) {
    return null;
  }
}

async function targetIds(port) {
  const list = await fetchJson(`http://127.0.0.1:${port}/json/list`);
  return Array.isArray(list) ? list.map((target) => target.id) : null;
}

async function endpointUp(port) {
  return Boolean(await fetchJson(`http://127.0.0.1:${port}/json/version`));
}

async function liveChecks() {
  const refusal = gm('crawl', null, 'engine=cdp\nheadless\n');
  check('live.cdp_verb_refuses_headless_line', /headless is refused/.test(refusal), refusal.replace(/\s+/g, ' ').slice(-200));

  const acquired = gm('browser_lease_acquire', { root, agent });
  const port = Number(field(acquired, 'port'));
  const pid = Number(field(acquired, 'chrome_pid'));
  check('live.lease_acquired', field(acquired, 'ok') === 'true' && port > 0 && pid > 0, acquired.replace(/\s+/g, ' ').slice(0, 200));
  if (!(port > 0 && pid > 0)) return;

  try {
    const leased = await waitFor(() => { const info = processInfo(pid); return info.alive ? info : null; }, 20000);
    check('live.leased_process_is_running', leased !== null);
    check('live.command_line_has_no_headless_flag', leased !== null && !/--headless/.test(leased.cmd || ''));
    check('live.command_line_debugs_on_leased_port', leased !== null && (leased.cmd || '').includes(`--remote-debugging-port=${port}`));
    const visible = await waitFor(() => { const info = processInfo(pid); return info.alive && info.window > 0 ? info : null; }, 20000);
    check('live.chrome_window_is_visible', visible !== null, `MainWindowHandle=${visible ? visible.window : 'none'}`);
    check('live.devtools_endpoint_answers', await endpointUp(port));

    const run = gm('crawl', null, 'engine=cdp\nurl=about:blank\n');
    check('live.cdp_run_is_headful', /^ok: true$/m.test(run) && /^headless: false$/m.test(run), run.replace(/\s+/g, ' ').slice(-160));
    const runTarget = field(run, 'target_id');
    const afterRun = await targetIds(port);
    check('live.run_closes_its_own_tab', runTarget !== null && afterRun !== null && !afterRun.includes(runTarget), `target_id=${runTarget} still listed=${afterRun ? afterRun.includes(runTarget) : 'n/a'}`);
  } finally {
    const released = gm('browser_lease_release', { root, agent });
    const closed = await waitFor(async () => !processInfo(pid).alive && !(await endpointUp(port)), 10000);
    check('live.release_closes_the_browser', closed === true, `lease_count after release=${field(released, 'lease_count')}`);
    if (!closed) {
      killTree(pid);
      console.log(`cleanup: closed the Chrome this witness leased (pid ${pid})`);
    }
  }

  const chrome = chromeExecutable();
  check('live.chrome_executable_found', chrome !== null);
  if (!chrome) return;
  const profile = `${root}\\.gm\\crawl-cdp-profile`;
  const controlDir = path.join(os.tmpdir(), `cdp-headful-control-${process.pid}`);
  const marker = startChrome(chrome, profile);
  const control = startChrome(chrome, controlDir);
  const started = await waitFor(() => processInfo(marker).alive && processInfo(control).alive, 20000);
  try {
    check('reap.marker_and_control_chrome_started', started === true);
    const reacquired = gm('browser_lease_acquire', { root, agent: `${agent}-reap` });
    const reacquiredPid = Number(field(reacquired, 'chrome_pid'));
    check('reap.marker_chrome_reaped_on_launch', !processInfo(marker).alive, `marker pid ${marker}`);
    check('reap.control_chrome_untouched', processInfo(control).alive, `control pid ${control}`);
    gm('browser_lease_release', { root, agent: `${agent}-reap` });
    if (reacquiredPid > 0 && processInfo(reacquiredPid).alive) killTree(reacquiredPid);
  } finally {
    killTree(marker);
    killTree(control);
    try {
      fs.rmSync(controlDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 300 });
    } catch (error) {
      console.log(`cleanup: could not remove ${controlDir} (${error.code})`);
    }
  }
}

async function main() {
  staticChecks();
  if (staticOnly) {
    const failed = results.filter((result) => !result.ok).length;
    console.log(`RESULT: ${failed === 0 ? 'PASS' : 'FAIL'} (static checks only, ${results.length} checks, live checks not run)`);
    process.exitCode = failed === 0 ? 0 : 1;
    return;
  }
  try {
    await liveChecks();
  } catch (error) {
    check('live.witness_ran_to_completion', false, String(error.message).slice(0, 200));
  }
  const failed = results.filter((result) => !result.ok).map((result) => result.name);
  console.log(failed.length === 0
    ? `RESULT: PASS (${results.length} checks)`
    : `RESULT: FAIL (${failed.length} of ${results.length} failed: ${failed.join(', ')})`);
  process.exitCode = failed.length === 0 ? 0 : 1;
}

await main();
