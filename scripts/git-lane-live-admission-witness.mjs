#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const VERB = 'git_stash_list';
const BODY = {};
const SESSION_ID = 'spoint-orch-b186-w125-gla';
const ACCEPTED_BANDS = ['general', 'git-band'];
const DEFAULT_MAX_MS = 10000;
const DEFAULT_CAP_MS = 120000;
const POLL_MS = 25;
const DEFAULT_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const REGISTRY = path.join('crates', 'agentplug-host', 'src', 'registry.rs');

class UsageError extends Error {}

function parseArgs(argv) {
  const opts = { mode: null, root: null, maxMs: DEFAULT_MAX_MS, capMs: DEFAULT_CAP_MS };
  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i];
    const value = () => {
      if (i + 1 >= argv.length) throw new UsageError(`${flag} needs a value`);
      i += 1;
      return argv[i];
    };
    if (flag === '--static' || flag === '--live') {
      if (opts.mode) throw new UsageError('choose one of --static or --live');
      opts.mode = flag.slice(2);
    } else if (flag === '--root') opts.root = path.resolve(value());
    else if (flag === '--max-ms') opts.maxMs = Number(value());
    else if (flag === '--cap-ms') opts.capMs = Number(value());
    else throw new UsageError(`unknown argument ${flag}`);
  }
  if (!opts.mode) throw new UsageError('choose --static or --live');
  if (opts.mode === 'live' && !opts.root) {
    throw new UsageError('--live needs --root <project dir holding .gm/exec-spool>');
  }
  if (opts.mode === 'static' && !opts.root) opts.root = DEFAULT_ROOT;
  for (const [name, value] of [['--max-ms', opts.maxMs], ['--cap-ms', opts.capMs]]) {
    if (!Number.isFinite(value) || value <= 0) throw new UsageError(`${name} must be a positive number`);
  }
  if (opts.capMs < opts.maxMs) throw new UsageError('--cap-ms must be at least --max-ms');
  return opts;
}

function countMatches(text, pattern) {
  return (text.match(pattern) ?? []).length;
}

function staticChecks(source) {
  const testStart = source.search(/#\[cfg\(test\)\]\s*mod admission_tests/);
  const production = testStart < 0 ? source : source.slice(0, testStart);
  return [
    {
      name: 'production admit_verb_within calls without a lane',
      got: countMatches(production, /SharedPluginPool::admit_verb_within\(/g),
      ok: (got) => got === 0,
      want: '0',
    },
    {
      name: 'production admissions passing the git lane for git_ verbs',
      got: countMatches(
        production,
        /SharedPluginPool::admit_verb_within_lane\(\s*&pool,\s*cost_class,\s*verb\.starts_with\("git_"\)\.then_some\("git"\),/g,
      ),
      ok: (got) => got >= 2,
      want: 'at least 2',
    },
    {
      name: 'git-lane gate in the admission loop',
      got: countMatches(source, /is_git_lane = !is_heavy && lane == Some\("git"\)/g),
      ok: (got) => got === 1,
      want: '1',
    },
    {
      name: 'git band limit expression',
      got: countMatches(source, /self\.short_reserved_slots\(\)\.saturating_sub\(1\)/g),
      ok: (got) => got === 1,
      want: '1',
    },
  ];
}

function runStatic(opts) {
  const file = path.join(opts.root, REGISTRY);
  let source;
  try {
    source = fs.readFileSync(file, 'utf8');
  } catch (error) {
    console.error(`git-lane-live-admission-witness: cannot read ${file}: ${error.message}`);
    process.exit(2);
  }
  console.log(`static tree ${opts.root}`);
  const failed = [];
  for (const check of staticChecks(source)) {
    const pass = check.ok(check.got);
    console.log(`check ${check.name}: ${check.got}, want ${check.want} ${pass ? 'PASS' : 'FAIL'}`);
    if (!pass) failed.push(check.name);
  }
  console.log(`witness git-lane-admission-wiring: ${failed.length === 0 ? 'PASS' : 'FAIL'}`);
  if (failed.length === 0) {
    console.log('RESULT: PASS');
    process.exit(0);
  }
  console.log(`RESULT: FAIL ${failed.join('; ')}`);
  process.exit(1);
}

function readStatus(spoolDir) {
  try {
    return JSON.parse(fs.readFileSync(path.join(spoolDir, '.status.json'), 'utf8'));
  } catch {
    return null;
  }
}

function snapshotLine(status) {
  if (!status) return 'status: .status.json unreadable';
  const pick = (key) => (status[key] === undefined ? 'absent' : JSON.stringify(status[key]));
  return [
    `runner_version=${pick('runner_version')}`,
    `runner_update_in_progress=${pick('runner_update_in_progress')}`,
    `runner_update_waiting_ms=${pick('runner_update_waiting_ms')}`,
    `queue_depth=${pick('queue_depth')}`,
    `queued_step_count=${pick('queued_step_count')}`,
    `claimed_step_count=${pick('claimed_step_count')}`,
    `gm_processor_capacity=${pick('gm_processor_capacity')}`,
  ].join(' ');
}

function dispatch(spoolDir, task) {
  const inDir = path.join(spoolDir, 'in', VERB);
  fs.mkdirSync(inDir, { recursive: true });
  const tmp = path.join(inDir, `${task}.txt.tmp`);
  fs.writeFileSync(tmp, JSON.stringify({ session_id: SESSION_ID, ...BODY }));
  const started = performance.now();
  fs.renameSync(tmp, path.join(inDir, `${task}.txt`));
  return started;
}

function tryReadReply(outFile) {
  try {
    return { value: JSON.parse(fs.readFileSync(outFile, 'utf8')) };
  } catch {
    return null;
  }
}

async function awaitReply(outFile, started, capMs) {
  while (performance.now() - started < capMs) {
    const read = fs.existsSync(outFile) ? tryReadReply(outFile) : null;
    if (read) return { reply: read.value, latencyMs: Math.round(performance.now() - started) };
    await new Promise((resolve) => setTimeout(resolve, POLL_MS));
  }
  return { reply: null, latencyMs: null };
}

function liveReasons(reply, latencyMs, status, opts) {
  const reasons = [];
  if (latencyMs === null) reasons.push(`no reply within ${opts.capMs} ms`);
  if (reply === null) {
    reasons.push('reply not parseable');
  } else {
    if (reply.ok !== true) reasons.push(`ok is ${JSON.stringify(reply.ok)}, want true`);
    if (reply.admission_decision !== 'admitted') {
      reasons.push(`admission_decision is ${JSON.stringify(reply.admission_decision)}, want "admitted"`);
    }
    if (!ACCEPTED_BANDS.includes(reply.admission_band)) {
      reasons.push(`admission_band is ${JSON.stringify(reply.admission_band)}, want one of ${ACCEPTED_BANDS.join(', ')}`);
    }
    if (typeof reply.admission_waited_ms !== 'number') reasons.push('admission_waited_ms is not a number');
  }
  if (latencyMs !== null && latencyMs > opts.maxMs) {
    reasons.push(`answered in ${latencyMs} ms, bound ${opts.maxMs} ms`);
  }
  if (status?.runner_update_in_progress !== true) {
    reasons.push('runner_update_in_progress not set at dispatch (acceptance needs a runner update in progress)');
  }
  return reasons;
}

async function runLive(opts) {
  const spoolDir = path.join(opts.root, '.gm', 'exec-spool');
  if (!fs.existsSync(path.join(spoolDir, 'out'))) {
    console.error(`git-lane-live-admission-witness: ${spoolDir} has no out directory; not a served project`);
    process.exit(2);
  }
  const before = readStatus(spoolDir);
  console.log(`live root ${opts.root}`);
  console.log(`status ${snapshotLine(before)}`);

  const task = `${SESSION_ID}-${process.pid}-${Date.now()}-1`;
  const outFile = path.join(spoolDir, 'out', `${VERB}-${task}.json`);
  const started = dispatch(spoolDir, task);
  console.log(`dispatched ${VERB} task ${task} at ${new Date().toISOString()}`);

  const { reply, latencyMs } = await awaitReply(outFile, started, opts.capMs);
  if (reply) {
    console.log(
      `reply after ${latencyMs} ms: ok=${JSON.stringify(reply.ok)} admission_decision=${JSON.stringify(reply.admission_decision)} ` +
        `admission_band=${JSON.stringify(reply.admission_band)} admission_waited_ms=${JSON.stringify(reply.admission_waited_ms)}`,
    );
  } else {
    console.log(`no reply within ${opts.capMs} ms; out file ${outFile}`);
  }

  const reasons = liveReasons(reply, latencyMs, before, opts);
  console.log(`witness git-lane-live-admission: ${reasons.length === 0 ? 'PASS' : 'FAIL'}`);
  if (reasons.length === 0) {
    console.log('RESULT: PASS');
    process.exit(0);
  }
  console.log(`RESULT: FAIL ${reasons.join('; ')}`);
  process.exit(1);
}

function main() {
  let opts;
  try {
    opts = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(`git-lane-live-admission-witness: ${error.message}`);
    process.exit(2);
  }
  if (opts.mode === 'static') runStatic(opts);
  else runLive(opts);
}

main();
