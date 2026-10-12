#!/usr/bin/env node
// Idle-floor witness for the agentplug shared daemon.
//
// Boots --runner as `daemon` inside an isolated AGENTPLUG_HOME (never the live home), attaches
// synthetic git roots that carry the spool verb directories a live project has, samples the
// daemon's CPU through Get-Process TotalProcessorTime over --samples windows of --window-ms, and
// prints RESULT: PASS only when the mean core-equivalent is at or below --threshold, the daemon
// stayed alive with no self-update handoff, every root was attached, and freshly written
// requests are still answered within --liveness-max-ms.
//
//   node scripts/daemon-idle-floor-witness.mjs --runner <agentplug-runner.exe> [options]
//     --roots 121  --warmup-ms 60000  --samples 60  --window-ms 2000  --threshold 0.05
//     --liveness 3  --liveness-max-ms 3000  --home <dir>  --live-home <dir>  --keep
//
// --live-home is read only: its default plugins and precompiled cache are copied into the
// isolated home so the daemon never downloads and never writes outside --home. The runner is
// copied into <home>/bin and run from there with AGENTPLUG_NO_SELF_UPDATE=1 and a frozen marker,
// so a staged or released runner in the shared install directory is never adopted or swapped in.
// The shared installed runner must be unchanged by the run, or the run is invalid.
// Exit 0 on PASS, 1 on FAIL, 2 on a usage or setup error.

import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const SPOOL_VERBS = [".gm","bash","bootstrap","branch_status","browser","browser-close","browser_lease_acquire","browser_lease_release","browser_lease_status","c","cache_get","cache_invalidate","cache_put","cache_stats","callees","callers","cdp","ci-status","ci_status","claim-audit","close","codeinsight","codeinsight_index","codesearch","config-sync-now","config_resolve","cpp","crawl","dataflow_resolve","deno","discipline","dream-replay-cycle","dreamrsi-replay","env_get","exec_js","fetch","filter","forget","fs_delete","fs_read","fs_readdir","fs_rm","fs_stat","fs_write","gh","git","git_add","git_amend","git_apply","git_branch","git_branch_delete","git_checkout","git_commit","git_diff","git_fetch","git_finalize","git_log","git_ls_files","git_merge","git_merge_abort","git_poll","git_pull","git_push","git_reset","git_reset_head","git_revert","git_rm","git_show","git_status","git_worktree","git_worktree_add","git_worktree_list","git_worktree_prune","git_worktree_remove","glob","go","grep","health","help","impact","instruction","java","kv_get","kv_put","kv_query","lang","memorize","memorize-backfill","memorize-fire","memorize-prune","memorize-retention","memorize-vacuum","mutable-add","mutable-list","mutable-resolve","mutable_resolve","phase-status","phases","pool-brief","pool-observe","powershell","prd-add","prd-block","prd-create","prd-defer","prd-delete","prd-edit","prd-get","prd-list","prd-note","prd-patch","prd-resolve","prd-set","prd-show","prd-status","prd-update","prd-upsert","prd-write","prd_add","prd_list","prd_resolve","python","recall","residual-scan","residual_scan","rust","scan_deps","search","serp","session","sql_close","sql_deserialize","sql_exec","sql_list_dbs","sql_open","sql_query","sql_serialize","sql_smoke","ssh","status","store","task-list","task-output","task-spawn","task-stop","tencentdb-compat-probe","tencentdb-memory-import","transition","verbs","verb_placeholder","wait"];
const DEFAULT_PLUGINS = ['gm', 'libsql', 'bert', 'treesitter', 'crux', 'lightpanda'];
const PLUGIN_FILE_SUFFIXES = ['.wasm', '.version', '.wasm.sha256', '.build.json'];
const DAEMON_CONFIG = {
  registry_poll_interval_secs: 5,
  heartbeat_interval_secs: 10,
  plugin_update_poll_interval_secs: 3600,
  plugin_update_poll_interval_secs_by_name: {},
  runner_update_poll_interval_secs: 3600,
  instruction_source_poll_interval_secs: 3600,
  require_runner_signature: false,
};
const HANDOFF_MARKER = /handed off to version|staged self-update to|found pre-existing staged runner/;
const ATTACH_MARKER = /lease attached: serving /;

class UsageError extends Error {}

function parseArgs(argv) {
  const opts = {
    runner: null, roots: 121, warmupMs: 60000, samples: 60, windowMs: 2000, threshold: 0.05,
    liveness: 3, livenessMaxMs: 3000, home: null, liveHome: path.join(os.homedir(), '.agentplug'), keep: false,
  };
  const numeric = new Set(['roots', 'warmupMs', 'samples', 'windowMs', 'threshold', 'liveness', 'livenessMaxMs']);
  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i];
    if (flag === '--keep') { opts.keep = true; continue; }
    const match = /^--([a-z-]+)$/.exec(flag);
    const key = match ? match[1].replace(/-([a-z])/g, (_, c) => c.toUpperCase()) : null;
    if (!key || !(key in opts)) throw new UsageError(`unknown option ${flag}`);
    const value = argv[++i];
    if (value === undefined) throw new UsageError(`${flag} needs a value`);
    opts[key] = numeric.has(key) ? Number(value) : value;
    if (numeric.has(key) && !Number.isFinite(opts[key])) throw new UsageError(`${flag} must be a number`);
  }
  if (!opts.runner) throw new UsageError('--runner <agentplug-runner.exe> is required');
  return opts;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function prepareHome(home, liveHome, opts) {
  fs.rmSync(home, { recursive: true, force: true });
  for (const dir of ['userhome', 'plugins', 'precompiled', 'leases']) fs.mkdirSync(path.join(home, dir), { recursive: true });
  fs.writeFileSync(path.join(home, 'daemon-config.json'), JSON.stringify(DAEMON_CONFIG, null, 4));
  const pluginSrc = path.join(liveHome, 'plugins');
  for (const name of DEFAULT_PLUGINS) {
    const files = PLUGIN_FILE_SUFFIXES.map((suffix) => `${name}${suffix}`).filter((f) => fs.existsSync(path.join(pluginSrc, f)));
    if (!files.includes(`${name}.wasm`)) throw new Error(`default plugin ${name}.wasm is missing from ${pluginSrc}`);
    for (const f of files) fs.copyFileSync(path.join(pluginSrc, f), path.join(home, 'plugins', f));
  }
  fs.cpSync(path.join(liveHome, 'precompiled'), path.join(home, 'precompiled'), { recursive: true });
  const roots = [];
  for (let i = 0; i < opts.roots; i++) {
    const root = path.join(home, 'roots', `r${String(i).padStart(3, '0')}`);
    fs.mkdirSync(path.join(root, '.git'), { recursive: true });
    fs.writeFileSync(path.join(root, '.git', 'HEAD'), 'ref: refs/heads/main\n');
    for (const verb of SPOOL_VERBS) fs.mkdirSync(path.join(root, '.gm', 'exec-spool', 'in', verb), { recursive: true });
    fs.mkdirSync(path.join(root, '.gm', 'exec-spool', 'out'), { recursive: true });
    roots.push(root);
  }
  for (const root of roots) {
    const key = crypto.createHash('sha256').update(root.toLowerCase()).digest().subarray(0, 8).toString('hex');
    fs.writeFileSync(path.join(home, 'leases', `${key}.0.lease`), root);
  }
  return roots;
}

function launchDaemon(runner, home, logFile) {
  const userHome = path.join(home, 'userhome');
  const env = { ...process.env, AGENTPLUG_HOME: home, HOME: userHome, USERPROFILE: userHome };
  delete env.AGENTPLUG_NO_DAEMON;
  delete env.AGENTPLUG_ALLOW_UPDATE_OVER_LOCAL_BUILD;
  env.AGENTPLUG_NO_SELF_UPDATE = '1';
  const child = spawn(runner, ['daemon'], { env, stdio: ['ignore', 'pipe', 'pipe'], windowsHide: true });
  const log = fs.createWriteStream(logFile);
  child.stdout.pipe(log, { end: false });
  child.stderr.pipe(log, { end: false });
  const state = { exit: null };
  child.on('exit', (code, signal) => { state.exit = { code, signal }; });
  child.on('error', (err) => { state.exit = { error: err.message }; });
  return { child, state, log };
}

function sampleDaemonCpu(pid, samples, windowMs, workDir) {
  const script = path.join(workDir, 'sample-daemon-cpu.ps1');
  fs.writeFileSync(script, [
    "$ErrorActionPreference = 'Stop'",
    '$inv = [Globalization.CultureInfo]::InvariantCulture',
    `for ($i = 0; $i -le ${samples}; $i++) {`,
    `  $p = Get-Process -Id ${pid}`,
    "  [Console]::Out.WriteLine(([DateTime]::UtcNow.Ticks).ToString($inv) + ',' + $p.TotalProcessorTime.TotalSeconds.ToString('R', $inv))",
    `  if ($i -lt ${samples}) { Start-Sleep -Milliseconds ${windowMs} }`,
    '}',
  ].join('\n'));
  return new Promise((resolve, reject) => {
    const ps = spawn('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', script], { windowsHide: true });
    let out = '';
    let err = '';
    ps.stdout.on('data', (d) => { out += d; });
    ps.stderr.on('data', (d) => { err += d; });
    ps.on('error', reject);
    ps.on('close', (code) => {
      const rows = out.split(/\r?\n/).filter(Boolean).map((line) => {
        const [ticks, cpu] = line.split(',');
        return { wall: Number(ticks) / 1e7, cpu: Number(cpu) };
      });
      const valid = rows.length === samples + 1 && rows.every((r) => Number.isFinite(r.wall) && Number.isFinite(r.cpu));
      if (code !== 0 || !valid) {
        reject(new Error(`CPU sampler failed (exit ${code}, ${rows.length}/${samples + 1} samples): ${err.trim().slice(0, 400)}`));
        return;
      }
      resolve(rows);
    });
  });
}

function summarizeWindows(rows) {
  const windows = [];
  for (let i = 1; i < rows.length; i++) {
    windows.push((rows[i].cpu - rows[i - 1].cpu) / (rows[i].wall - rows[i - 1].wall));
  }
  const last = rows[rows.length - 1];
  const meanCoreEq = (last.cpu - rows[0].cpu) / (last.wall - rows[0].wall);
  const sorted = [...windows].sort((a, b) => a - b);
  return {
    meanCoreEq,
    maxWindow: sorted[sorted.length - 1],
    p50Window: sorted[Math.floor(sorted.length / 2)],
    windowCount: windows.length,
  };
}

async function livenessProbe(roots, count, maxMs) {
  const results = [];
  for (let k = 0; k < count; k++) {
    const root = roots[Math.floor((k * roots.length) / count)];
    const sid = `idlefloor-${k}`;
    const task = `${sid}-1`;
    const inDir = path.join(root, '.gm', 'exec-spool', 'in', 'browser_lease_status');
    const outDir = path.join(root, '.gm', 'exec-spool', 'out');
    const tmp = path.join(inDir, `${task}.txt.tmp`);
    fs.writeFileSync(tmp, JSON.stringify({ session_id: sid }));
    const started = performance.now();
    fs.renameSync(tmp, path.join(inDir, `${task}.txt`));
    let latencyMs = null;
    let ok = false;
    while (performance.now() - started < maxMs) {
      const hit = fs.readdirSync(outDir).find((n) => n.startsWith(`browser_lease_status-${task}`) && n.endsWith('.json'));
      if (hit) {
        latencyMs = Math.round(performance.now() - started);
        try { ok = JSON.parse(fs.readFileSync(path.join(outDir, hit), 'utf8')).ok === true; } catch { ok = false; }
        break;
      }
      await sleep(25);
    }
    results.push({ root: path.basename(root), latencyMs, ok });
  }
  return results;
}

async function stopDaemon(child, state) {
  if (state.exit) return;
  if (process.platform === 'win32') spawnSync('taskkill', ['/PID', String(child.pid), '/T', '/F'], { windowsHide: true });
  else child.kill('SIGTERM');
  for (let i = 0; i < 100 && !state.exit; i++) await sleep(100);
}

function isolateRunner(runner, home) {
  const binDir = path.join(home, 'bin');
  fs.mkdirSync(binDir, { recursive: true });
  const copy = path.join(binDir, path.basename(runner));
  fs.copyFileSync(runner, copy);
  fs.writeFileSync(path.join(home, 'agentplug-runner.no-self-update'), 'frozen by daemon-idle-floor-witness: the isolated runner never self-updates\n');
  return copy;
}

function sharedInstallStamp() {
  try {
    const st = fs.statSync(path.join(os.homedir(), '.gm-tools', 'agentplug-runner.exe'));
    return `${st.size}@${st.mtimeMs}`;
  } catch {
    return 'absent';
  }
}

function killIsolatedProcesses(home) {
  if (process.platform !== 'win32') return;
  const prefix = `${path.resolve(home)}\\`.replace(/'/g, "''");
  const script = `Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -and $_.ExecutablePath.StartsWith('${prefix}', [StringComparison]::OrdinalIgnoreCase) } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }`;
  spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { windowsHide: true, encoding: 'utf8' });
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  const runner = path.resolve(opts.runner);
  if (!fs.existsSync(runner)) throw new UsageError(`runner not found: ${runner}`);
  const liveHome = path.resolve(opts.liveHome);
  const home = path.resolve(opts.home ?? path.join(os.tmpdir(), `agentplug-idle-floor-${process.pid}`));
  if (home === liveHome || home.startsWith(liveHome + path.sep)) {
    throw new UsageError('--home must not be the live AGENTPLUG_HOME or inside it');
  }
  const roots = prepareHome(home, liveHome, opts);
  const isolatedRunner = isolateRunner(runner, home);
  const version = spawnSync(isolatedRunner, ['--version'], { encoding: 'utf8', windowsHide: true }).stdout.trim();
  const sharedBefore = sharedInstallStamp();
  const logFile = path.join(home, 'witness-daemon.stderr.log');
  const { child, state, log } = launchDaemon(isolatedRunner, home, logFile);
  const reasons = [];
  let summary = null;
  let liveness = [];
  let attachedLines = 0;
  let handoffLines = 0;
  try {
    console.log(`runner=${runner} isolated=${isolatedRunner} version=${version} home=${home} roots=${roots.length} verbs=${SPOOL_VERBS.length} pid=${child.pid}`);
    const warmupEnd = Date.now() + opts.warmupMs;
    while (Date.now() < warmupEnd && !state.exit) await sleep(250);
    if (state.exit) throw new Error(`daemon exited during warm-up: ${JSON.stringify(state.exit)}`);
    console.log(`warm-up ${opts.warmupMs} ms done; sampling ${opts.samples} windows of ${opts.windowMs} ms`);
    const rows = await sampleDaemonCpu(child.pid, opts.samples, opts.windowMs, home);
    summary = summarizeWindows(rows);
    if (state.exit) reasons.push(`daemon exited during the measurement: ${JSON.stringify(state.exit)}`);
    if (opts.liveness > 0) liveness = await livenessProbe(roots, opts.liveness, opts.livenessMaxMs);
  } catch (err) {
    reasons.push(err instanceof Error ? err.message : String(err));
  } finally {
    await stopDaemon(child, state);
    killIsolatedProcesses(home);
    await sleep(1500);
    killIsolatedProcesses(home);
    log.end();
    await new Promise((resolve) => log.once('finish', resolve));
    const text = fs.existsSync(logFile) ? fs.readFileSync(logFile, 'utf8') : '';
    attachedLines = text.split(/\r?\n/).filter((l) => ATTACH_MARKER.test(l)).length;
    handoffLines = text.split(/\r?\n/).filter((l) => HANDOFF_MARKER.test(l)).length;
  }
  if (summary) {
    console.log(`floor: mean_core_eq=${summary.meanCoreEq.toFixed(4)} p50_window=${summary.p50Window.toFixed(4)} max_window=${summary.maxWindow.toFixed(4)} windows=${summary.windowCount} threshold=${opts.threshold}`);
    if (!(summary.meanCoreEq <= opts.threshold)) reasons.push(`mean core-equivalent ${summary.meanCoreEq.toFixed(4)} is above the ${opts.threshold} threshold`);
  }
  if (liveness.length > 0) {
    const answered = liveness.filter((p) => p.ok && p.latencyMs !== null && p.latencyMs <= opts.livenessMaxMs).length;
    console.log(`liveness: ${answered}/${liveness.length} fresh requests answered within ${opts.livenessMaxMs} ms ${JSON.stringify(liveness)}`);
    if (answered !== liveness.length) reasons.push(`only ${answered}/${liveness.length} fresh requests were answered in time`);
  }
  console.log(`attached_roots_logged=${attachedLines} handoff_markers=${handoffLines} log=${logFile}`);
  if (attachedLines < roots.length) reasons.push(`only ${attachedLines}/${roots.length} synthetic roots were attached`);
  if (handoffLines > 0) reasons.push('a runner self-update or handoff fired during the measurement, so the run is invalid');
  const sharedAfter = sharedInstallStamp();
  if (sharedAfter !== sharedBefore) {
    reasons.push(`the shared installed runner changed during the run (${sharedBefore} -> ${sharedAfter}), so isolation was breached and the run is invalid`);
  }
  const verdict = reasons.length === 0 ? 'PASS' : 'FAIL';
  for (const reason of reasons) console.log(`reason: ${reason}`);
  if (!opts.keep) {
    const kept = path.join(os.tmpdir(), `agentplug-idle-floor-${Date.now()}.stderr.log`);
    if (fs.existsSync(logFile)) fs.copyFileSync(logFile, kept);
    try {
      fs.rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 500 });
    } catch (err) {
      console.log(`warning: could not remove ${home}: ${err.message}`);
    }
    console.log(`daemon log kept at ${kept}`);
  }
  console.log(`RESULT: ${verdict}`);
  process.exitCode = verdict === 'PASS' ? 0 : 1;
}

main().catch((err) => {
  console.error(err instanceof Error ? err.message : String(err));
  console.log('RESULT: FAIL');
  process.exitCode = err instanceof UsageError ? 2 : 1;
});
