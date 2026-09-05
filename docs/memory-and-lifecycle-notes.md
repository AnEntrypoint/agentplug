# Memory and lifecycle notes

Rationale that the code cannot carry in a name. Each heading names the
function or item the note belongs to. Every note is a measurement, a defect
in another system, or a rejected alternative.

## agentplug-host: host_state.rs

### HostState::take_lost_response

A host response that cannot be handed to the guest returns a zero packed
value. The guest cannot tell that from a genuine empty response. The reason
is recorded here and `dispatch_on` reads and clears it. A lost response then
fails loudly instead of arriving as a bodyless success.

### HostState::allow_extra_root

A root is only readable after a real dispatch names it (for example
`codesearch {root: "<abs>"}`). This is narrower than a standing sandbox
grant. `sandboxed_guest_path_with_extra_roots` checks this list in addition
to `cwd` and `~/.gm`, never wider.

### HostState::call_deadline_secs

Holds the deadline `dispatch_on` resolved for the current call. See
`write_guest_bytes` below for why it must be restored after every host to
guest response handoff.

## agentplug-host: imports.rs

### write_guest_bytes

Every host to guest response narrows the epoch deadline to a 5 second
handoff window. One dispatch can cross that handoff many times. For
example memorize-fire runs its embed, then a dedup-check read, then its md
and vector writes. Leaving the deadline at 5 seconds after the first
handoff silently shrank every caller-supplied `deadline_secs` (180, 700,
any value) to a de facto 5 second budget. That was the real cause of
memorize-fire's persistent `plugin_call_deadline_exceeded` failures under
real embed latency. It was previously misread as bert-pool contention or a
stuck holder. The deadline is restored to the dispatch's own value after
each handoff.

### atomic_write_locked

The shared plugin pool dispatches guest writes from several wasmtime Store
instances on separate OS threads. The pool is keyed by plugin name across
every project, not by file path. A plain `fs::write` raced two concurrent
guest writers to the same path into a lost update. This happened with two
back-to-back prd-add dispatches doing read-modify-write on `.gm/prd.yml`.
The guest side (`orchestrator/cas.rs`, `cas_retry_write`) already does an
optimistic recheck. It cannot see true interleaving of two independent
`fs::write` calls at the host layer. The per-path lock closes that gap.
The temp-file plus rename pattern matches `memory_md.rs`'s `rename_batch`
on the guest side, so a reader never observes a torn write.

### atomic_cas_write_locked

`atomic_write_locked` alone serializes only the write half of a
read-modify-write cycle. Two guest callers can read the same "before"
content through separate unlocked `host_fs_read` calls, compute new
documents, and both write. The lock stops tearing but not one clobbering
the other's landed change. The guest's own recheck cannot close this
because its recheck read and its write are two host calls with no shared
lock scope. This function does the "is `expected` still current" check and
the write inside one lock hold. `Ok(false)` means CAS mismatch and the guest
should re-read and retry.

### has_project_marker

A bare existence check would let a guest name any real directory (a home
folder, `.ssh`). That is a larger grant than "reach another real project"
requires. `.git` and `.gm` cover every gm project and plain git repo. A
manifest file covers a real codebase gm has never run against.

## agentplug-host: registry.rs

### PluginFiberLifecycle

This is the paper's Section 4.3 Definition 49 lifecycle, reduced to three
states, the same reduction `discipline_note.rs::FiberLifecycle` uses. A
plugin load is one synchronous `Module::from_file` plus `load_plugin`
call, so there is no `Reloading` window to model. `Unloading` marks a
failed load or an eviction for one dispatch before collapsing to
`Inactive`.

### advance_plugin_fiber

`Active` with a failed load becomes `Unloading`. `load_plugin`'s own LIFO
revert leaves the prior content structurally in place. The lifecycle still
records the attempt as a withdrawal in progress.

### SharedPluginPool::acquire_within

Waiting is FIFO-fair by ticket and never denies. `timeout_ms` is only the
threshold for reporting an abnormally long wait. The real backstop against
a wedged pool is each dispatch's own outer call deadline
(`DISPATCH_CALL_DEADLINE_SECS`), which aborts the holder and frees its slot.
A `try_lock` race across waiters was the rejected alternative. It restarted
the race every poll tick, so a project's own repeat requests did not drain
in issue order.

### get_active_provider

This mirrors the paper's `provider_k(gamma)` (Definition 46). Each shared
plugin is a singleton service, so its pool already is the stable entrypoint
a Cordis service broker (Section 6.2) provides. A pool with mixed content
hashes during a swap returns the hash of the first filled slot. Callers
wanting the full picture use `shared_plugin_slot_content_hashes`.

### resolve_routed_plugin_name

This is the Section 6.2 integration point. `plugin_name` doubles as the
broker's `service_key`. Zero or one registered provider returns the name
unchanged, so exclusive binding stays the default with no behavior change.
The returned `RouteLease` must stay alive for the whole dispatch call.
Dropping it early decrements `in_flight` before the wasm call starts. That
defeats `LeastLoaded` selection and lets `unregister_provider`'s in-flight
check race a still-running dispatch.

### ProjectPlugins::is_loaded_current

For non-shared per-session plugins, a loaded instance whose content hash no
longer matches is treated as not loaded. A rebuilt `.wasm` is then picked
up on the next dispatch instead of served stale forever. Shared plugins
already refresh their hash inside `load_plugin`'s pool-fill check.

### ProjectPlugins::load_plugin

Each slot fill is a revertible effect whose inverse is "put the prior
occupant back". If a later slot's instantiate fails, every slot filled in
this call is reverted in LIFO order. A partial swap would leave the pool
straddling old and new hashes. `is_loaded_current` only checks that any
slot matches, so a mixed pool would silently route some dispatches to the
stale plugin indefinitely.

## agentplug-runner: main.rs

### reconcile_plugin_manifest

This is the paper's Section 5.2.1 declarative component-loader
reconciliation. `load_plugin` already no-ops on a matching content hash, so
the function's value is naming the loop as one entry point. Theorem 73
(confluence) licenses skipping an already-current plugin: the quiescent
state answers to the roster alone, whatever order it is driven in. The
provider check after a successful load is Theorem 61's recovery-exactness
spot-check. A multi-slot pool fills lazily per slot, so a stale hash on
another slot is logged as a signal, not treated as a failed load.

## agentplug-runner: download.rs

### is_download_tmp_name

The tmp file is always `dest.with_extension("tmp.<pid>")`, so the suffix is
the last dot-segment. Matching `.tmp.<digits>` anchored at the end cannot
collide with a name that merely contains `.tmp.` elsewhere, such as
`backup.tmp.old`.

### gc_stale_tmp_files

A rename failure in `download_and_verify` leaves a sha256-verified tmp file
on disk. No caller retries the rename today, so without this sweep every
rename failure leaks one file forever. Age, not pid liveness, is the safety
check: a pid can be reused by an unrelated process after the writer exits.
One hour is longer than any real download can be in flight. The two
directories swept are `install_dir()/plugins` and the running runner's own
directory. Those are different trees on a real install. Known gap: if the
runner binary is relocated between a leaked failure and the next startup,
the old location is never swept. There is no durable record of prior
install locations. Accepted because the install path is stable in normal
operation.

### download_and_verify

A distinct pid-suffixed tmp path per call means a failed write or rename
never collides with a later run's tmp file. On write failure the tmp file
is incomplete and is removed. On rename failure the tmp file still holds
verified bytes and is left in place. The normal rename failure is Windows
`ERROR_SHARING_VIOLATION` when `dest` is locked by a running process, which
is the usual case for self-update staging and plugin hot-swap.

### try_ensure_plugin_installed_via_direct_release_latest

`https://github.com/{repo}/releases/latest/download/{asset}` is a plain
redirect served by GitHub's web frontend, not the REST API. It resolves
"latest" to the current non-prerelease tag and 302s to the asset URL.
Environments that proxy or scope `api.github.com` still serve this path.
This runner's own dev sandbox is one: a session-scoped GitHub proxy 403s
`api.github.com/repos/.../releases/latest` for any repo outside its
allowlist. Unauthenticated `api.github.com` calls also exhaust the 60 per
hour limit fast when many agents share one egress IP. This is the
second-tier fallback and needs no version pre-resolution.

### fetch_latest_runner_version

The API path can be blocked or rate-limited independently of the redirect
path above. On an API error the code resolves "latest" through the redirect
instead of skipping the self-update check for the rest of the process's
lifetime.

### project_declared_plugin_specs

`.agentplug/plugins.json` is a JSON array of `{name, repo, asset_basename}`
objects. `.agentplug/plugins.txt` already names extra plugins to load but
has no way to say where to download one from. A project's declared spec,
from any known root, wins over the compiled-in built-ins. A project can
therefore re-point `gm` or `bert` at a fork by declaring one.

### plugin_asset_spec

A one-shot `plugin` or `dispatch` CLI invocation never populates the
daemon's known-roots registry. The current process's own cwd is always
added so a single-shot command still honors that project's own
`.agentplug/plugins.json`.

### snapshot_prev_wasm_and_version

The `.wasm` and `.version` files must be snapshotted and restored together.
Restoring only the `.wasm` bytes while `.version` still names the failed
release tag would make `refresh_plugin_if_stale` believe the rolled-back
binary is already the latest and stop checking for updates.

### stage_runner_self_update

Windows refuses to rename or overwrite the backing file of a live process's
mapped image ("Access is denied"). That lock never clears for the process's
lifetime. A process still executing from a `.new` path therefore stages the
next update as `.new2`. Otherwise every later self-update would try to
rename onto the exact path this process is locked to and fail forever. A
process running from the canonical path keeps using `.new`.

### fetch_latest_plugin_version

Some release repos are shared across plugin families. `AnEntrypoint/plugkit-bin`
receives releases from both rs-plugkit (`gm`) and rs-codeinsight. `GET
/releases/latest` returns the single newest release in the whole repo with
no regard for which asset it carries. Witnessed live: `gm` v0.1.1243 was
masked by a same-day rs-codeinsight v0.3.48 release, and every poll 404ed on
`plugkit-slim.wasm`. The fix fetches the releases list and picks the newest
release whose assets include this plugin's basename. `per_page=100` is
GitHub's maximum. Witnessed live: the top 20 most recent releases in
plugkit-bin were all rs-codeinsight; 100 surfaced gm's latest at about
position 21. `plugkit-slim`'s release step also uploads the fat
`plugkit.wasm`, so either name is accepted.

### clear_all_known_bad_version_markers

A known-bad mark records a host-ABI incompatibility, not a defect in the
plugin content. A runner self-update handoff is exactly the event that
resolves it. Clearing every plugin's list lets `refresh_plugin_if_stale`
re-test each version against the new host on its next poll.

### record_known_bad_version

Without this record a release that fails to instantiate against this
runner's host ABI is re-downloaded on the very next poll. "The tag changed"
and "the tag is loadable" are different questions. The list appends rather
than overwrites because a plugin can accumulate more than one bad tag
across separate publish mistakes upstream.

### refresh_plugin_if_stale

A latest tag that is known-bad is almost always a real upstream ABI
mismatch: the plugin repo published a build against a newer host-import
contract than this runner implements. Re-fetching it every poll would just
repeat the same failed load. The poll skips it until a newer tag appears or
this runner updates on its own schedule.

### record_plugin_load_failure_and_rollback

Observed live: the per-plugin release channel (for example
`AnEntrypoint/plugkit-bin` for `gm`) outpaced `agentplug-bin`'s cadence. A
plugin built against a changed host-import signature such as
`host_browser_exec` or `host_fs_cas_write` was auto-fetched before the
runner had a matching update. Rather than leaving the project unable to
dispatch until a human restores `plugin.wasm.prev` by hand, the code rolls
back to the last version that loaded and records the failed tag. `Ok(false)`
means there was nothing to roll back to and the caller should surface the
original error. An older install may have a `.wasm.prev` with no
`.version.prev` sibling; the `.version` file is then left as-is instead of
failing the whole rollback.

### PLUGIN_INSTALL_RETRY_COOLDOWN

`get_or_compile` calls `ensure_plugin_installed` on every daemon loop tick
(100ms) for every configured plugin. Without a gate, a plugin that fails to
install (bad token, exhausted rate limit, network down) is re-attempted at
about 10Hz. That alone exhausts GitHub's unauthenticated 60 per hour limit
in under two seconds and keeps it exhausted.

### plugin_install_failure_marker_path

The cooldown is persisted to disk, not an in-process map. The self-update
handoff launches the staged runner as a separate OS process to pre-warm
every plugin, and every one-shot CLI invocation is its own process. An
in-memory clock is invisible across that boundary, so a takeover landing
mid-cooldown would immediately re-attempt the install and reintroduce the
burst.

## agentplug-runner: daemon.rs

### DaemonConfig

`max_concurrent_projects`, `gm_concurrency`, `side_plugin_concurrency`,
`shared_store_recycle_private_mb` and `shared_store_recycle_dispatches` are
deliberately absent from the scaffolded example file. Leaving them unset
lets the accessors derive from this machine's `available_parallelism()` on
every boot. A literal in the scaffold froze that number into the file on
first run, so every later boot re-read a stale value.

`plugin_update_poll_interval_secs_by_name` controls how often the poll
check fires per plugin. Reload independence is already unconditional:
`refresh_plugin_if_stale` only reloads a plugin whose content hash changed.

### DaemonConfig::runner_update_poll_interval

Runner binaries update least often by design. They are the sole spool
loader and every project's daemon depends on one staying stable. The one
hour default, longer than the 600 second plugin poll, is deliberate.

### DaemonConfig::instruction_source_poll_interval

This used to share `plugin_update_poll_interval`'s Duration value (same
number, not the same timer). It is an independent key so tuning one cadence
never silently retunes the other.

### DaemonConfig::gm_concurrency

This still scales with core count. It feeds `shared_store_recycle_dispatches`
and, through `max_concurrent_projects`, the worker thread pool that
dispatches across different projects in parallel. That is independent
CPU-bound work. It no longer sizes the gm plugin's own Store pool.

### DaemonConfig::gm_pool_size and side_plugin_concurrency

Every plugin type gets exactly one hot, warm-resident Store instance, and
calls are serialized through it. Side plugins used to default to half the
host's cores. Each filled slot holds its own copy of the plugin's
instantiated Store. For bert that is its own copy of the loaded
BAAI/bge-small-en-v1.5 model weights in linear memory. On a 16-core host
that derived 8 slots per side plugin. Live-witnessed: bert filling 2 or 3
of its 8 slots under ordinary concurrent use pushed process memory from a
~300MB single-slot baseline past 1.7GB. Per-call latency was live-measured
at 600ms to 1.4s. These plugins are memory-costly but fast, not CPU-bound
work that benefits from N-way oversubscription. Serial throughput under
heavy load is the accepted tradeoff.

### DaemonConfig::shared_store_recycle_private_bytes

The threshold used to scale with `gm_concurrency()` at 400MB per slot,
uncapped. On a 16-core host that derived a 6400MB ceiling. Live-witnessed
2026-08-24: this daemon's own restart churn correlated with the host down to
about 3GB free of 15.6GB. The 1.7GB "steady state" an earlier 2048MB
default was calibrated against was itself a symptom of the core-scaled pool
sizes above, not a legitimate baseline. With one hot instance per plugin
the live-witnessed single-slot baseline is about 300MB. 768MB leaves real
headroom above that. It remains overridable for an operator whose host's
working set differs.

### DaemonConfig::project_idle_evict_ms

Shared plugins are policed by the recycle gate. Each project's own
non-shared plugins (libsql, oxibrowser, crux) get a dedicated Store per
project. `register_project` only drops a project once its path stops
existing on disk, never on inactivity. Live-witnessed: 103 registered
projects, nearly all warm at once, 1.7GB real process memory. The 30 minute
window was previously a hard-coded constant in agentplug-host with no config
plumbing. The default stays at 30 minutes: adversarial review of a 300
second draft found it was an unvalidated guess with no idle-time
distribution evidence, and an interactive session dispatching every 60 to
90 seconds would never register the win. The floor was raised from 30
seconds to 60 seconds. A floor below an interactive cadence evicts and
cold-reloads on every dispatch, which is worse than no fix.

### DaemonConfig::shared_plugin_release_idle_ms

The prior 120 second default cold-dropped the hot bert, treesitter and gm
slots on every ordinary lull. Live-witnessed: 10 or more drops in one
session with 103 registered projects. Each drop cost a wasm instantiate plus
first-forward-pass warmup; bert alone measured 5.5 seconds for one embed
call right after a reload. That figure predates the pool_size=1 design and
was never revisited. The 30 minute default matches `project_idle_evict_ms`
for the same reason: there is no idle-time distribution evidence beyond the
single "120 seconds was too short" data point, which bounds the problem
only from below. The 5 minute floor is re-derived rather than copied.
"Quiet" here means the whole daemon saw zero dispatch work across every
registered project, a rarer condition than one project going quiet, so a 60
second floor would react to an ordinary cross-project lull. Bursty traffic
cannot starve reclaim: `shared_store_recycle_dispatches` is checked every
tick independent of idle state. The two mechanisms cover disjoint
conditions.

### claim_ownership

Observed live 2026-08-24: three daemons launched within about 80ms of a
fresh Windows boot. Each read a pre-reboot owner pid as dead, each
unconditionally overwrote `daemon-owner.lock`, and authority volleyed
between them with no stable winner, so none ever self-exited. Two things
close this. First, a deterministic tie-break: the lowest pid wins, so every
challenger computes the same winner from the same observed set. Second,
the file is only overwritten if it still names a stale or dead pid at the
moment of the write, immediately before the rename. A live lower-pid
challenger defers instead of racing a rename, so exactly one process in a
concurrent group writes.

### ensure_daemon_running

The spawn lock is held across `spawn_detached_daemon()` and the freshness
wait, not just the spawn call. Releasing it right after the spawn opened a
real TOCTOU window: a concurrent caller saw the lock gone, saw
`is_daemon_fresh()` still false because the new process had not written its
first heartbeat, re-acquired the lock, and spawned a second daemon.
Observed live: four separate `agentplug-runner.exe` processes running at
once from a handful of `spool` calls a few minutes apart.

### run_takeover

After promoting the staged exe, the process is still bound to the `.new`
image for its whole lifetime. Continuing in-process would leave that file
permanently un-removable and collide with every future staging path. The
process re-execs from the canonical path and exits.

### record_completed_runner_swap

This is the durable "a swap just happened" record, distinct from
`staged_runner_awaiting_handoff` which only signals a pending swap. An agent
can diff it against its own last-seen value without polling
`daemon-status.json` continuously.

### PluginModules::get_or_compile

Root-caused 2026-08-14: `get_or_compile` ran unconditionally once per sweep
tick with no cadence gate. It did a full `fs::read` plus SHA-256 of every
default plugin's `.wasm` on the main sweep thread before any project's
worker pool started. bert.wasm is 136MB and treesitter.wasm 56MB on the
measuring machine, so about 200MB of disk read and hashing sat on the
critical path per tick. The plugin's own `dispatch.end` timing was 12 to
160ms for the same calls that took 0.8 to 2.5 seconds wall-clock. The fix
caches each plugin's `(mtime, len)` at the time its hash was last computed
and skips the read and hash when a fresh stat matches. A swapped file with
identical mtime and len is not distinguished from an unchanged one. That is
accepted: the download path always advances mtime and almost always changes
len, and this is an optimization over the hash check, not a replacement.
Post-fix steady-state calls dropped from 900ms to 2.5s down to 128 to 183ms.

### RAW_PLUGIN_SPOOL_VERBS

A caller drops `in/libsql/<N>.txt` with the real libsql verb carried inside
the JSON body as `"verb"`. The directory name is the plugin name here,
unlike every other spool directory where it is a gm orchestrator verb. This
lets a plain host process with no wasm runtime (for example freddie's
Node.js) reach a raw plugin through the same file-drop protocol instead of
spawning `agentplug-runner dispatch` per call. The remaining 130 to 180ms
per-call floor means this path is still not a substitute for an in-process
client library on a session-write hot path.

### run_gm_dispatch_to_file

The dispatch loop's join path only removes in-flight entries whose join
handle it still owns. A worker auto-detached after
`WORKER_AUTO_DETACH_AFTER_MS` had its handle dropped with the entry left
behind, which leaked it permanently. `detached_still_running` then stayed
true forever, so every later runner self-update went down the starve path
and force-handed off "despite N in-flight dispatches" where N counted dead
entries, killing real dispatches. The worker now removes its own entry on
completion.

### project_has_pending_dispatch_work

A cold project's work can arrive on two independent surfaces:
`.agentplug/plugin-dispatch/in/<plugin>/<verb>/*.txt` (the CLI client path)
and `.gm/exec-spool/in/<verb>/*.txt` (gm's own spool ABI). Checking only the
first left a gm session's freshly dropped spool file invisible to the
"genuinely active" probe. The probe reads two `read_dir` levels and stops
at the first non-empty verb directory, so it stays cheap on every cold root
every tick.

### dispatch_project

The idle fast path tries the scan-and-claim pass first, before paying for
two `create_dir_all` calls, a heartbeat write and a `plugins.txt` read. With
87 registered projects observed live, that setup cost was paid by every
worker for every project on every tick. Root-caused 2026-08-14 alongside
the `get_or_compile` finding above.

The early return must also check `project_has_pending_dispatch_work`. The
`.agentplug/plugin-dispatch` scan sits after the return. Without the check
a project whose gm spool is idle returned every tick and the CLI client's
file sat unclaimed for the full 30 second `MAX_WAIT_MS`, then fell through
to a cold wasm reload. Root-caused 2026-08-24 via an exact 34 second latency
reproduction matching `MAX_WAIT_MS` plus reload overhead.

The eagerly loaded plugin set is additive. A non-empty
`.agentplug/plugins.txt` used to replace it, so a project naming the three
side plugins silently dropped `gm` and every dispatch failed to load. The
failure named the plugin that was listed, not the one that went missing.

An instantiate failure (as opposed to a compile failure) means the bytes are
fine but the host-import contract does not match this runner. See
`record_plugin_load_failure_and_rollback`. `get_or_compile`'s content-hash
check picks up the rolled-back bytes on its next invocation, so no separate
cache eviction is needed.

`PROJECT_BATCH_ABSORB_WINDOW_MS` bounds how long one call keeps absorbing
newly arriving requests for one project. Without it a project under
sustained load kept the drain loop permanently non-empty. Each entry
auto-detaches at its own 45 second mark but a fresh one arrives first.
`run_daemon_body` runs a fixed `worker_count` of these calls over all
projects, so one project that never returns occupies a worker slot forever.
Witnessed live: 26 concurrent subagent sessions hammering one project's
spool wedged the whole daemon, including unrelated projects with no pending
work. After the deadline the call stops claiming new files but still waits
for spawned work, itself bounded by `WORKER_AUTO_DETACH_AFTER_MS`.

### run_daemon_body

The cold-project sweep: a project with an in-memory `ProjectPlugins` entry
has dispatched within `project_idle_evict_ms` and is checked every tick. A
project with none has been silent that whole window. Because
`register_project` never drops on inactivity, the cold set dominates the
registry almost immediately. Scanning all of it every tick paid a
`read_dir` per cold project for work that essentially never arrives. A root
that is new this registry poll is force-included so a fresh project's first
dispatch does not wait for the 30 second sweep. A cold root with pending
client-side work is also force-included; otherwise it races
`try_dispatch_via_daemon`'s own 30 second timeout and loses under
multi-project contention, falling through to a cold reload on every call.

`is_genuinely_active` runs parallel to `all_projects` and only affects what
is reported as `queue_depth` and `queue_position`. Scheduling still
cold-refreshes everything on the sweep cadence. A routine cold-sweep tick
must not report the whole lifetime registry as live backlog.

The outer plugin-poll gate uses the shortest configured interval across the
default and every per-plugin override. The per-plugin decision happens
inside the loop. A forced refresh bypasses the plugin's own cadence. A
project setting bert to 3600 seconds no longer has its check re-fired every
time libsql's shorter interval trips the outer gate.

`any_work` only covers dispatches the loop still owns a join handle for. An
auto-detached dispatch keeps running after `any_work` goes false. Handing
off there killed it and the caller got a `dispatch_orphaned` blaming a wasm
trap. `in_flight_map` keeps the detached entry until its thread finishes,
so it is the signal that survives detachment. The same signal guards the
idle self-recycle exit.

A runner self-update is never urgent; the current binary keeps serving.
Once starved, a second grace window (`SELF_UPDATE_HARD_CAP_MS`) lets heavy
verbs finish. `instruction` running a codeinsight rebuild routinely takes
15 to 30 seconds or more. Killing at the first deadline was the recurring
`instruction`-specific `dispatch_orphaned` pattern. The hard cap still
forces the handoff eventually so a wedged dispatch cannot block updates
forever.
