# AGENTS.md

AgentPlug is the native WebAssembly host used by gm. It contains the `agentplug-runner` daemon,
and the `agentplug-host` imports. Work on `main` as GitHub user `lanmower`.

## Source and release boundaries

- Keep code self-explanatory. Do not add comments except Rust `// SAFETY:` justifications; retain
  language attributes and shell shebangs. Put non-derivable operational contracts here or in the
  relevant user documentation.
- A push to `main` runs `.github/workflows/release.yml`: it bumps the patch version, builds every
  runner target, and publishes the release consumed by running daemons. Do not install, copy, or
  execute a local source-built runner as an update path.
- Runner and plugin updates install only bytes whose sha256 matches the published `.sha256` sidecar.
  The locally-built-runner guard, version monotonicity, and the SHA-256 check are independent
  protections; do not weaken one to repair another.
- Build provenance watches Git-resolved HEAD, every symbolic ref, and packed refs across submodule
  and worktree gitdirs. Missing loose refs watch their nearest existing parent; ordinary commits
  must refresh embedded COMMIT without touching source or clearing Cargo caches.
- Canonical runner promotion rechecks the local-build guard before it replaces the installed binary
  and re-execs from the canonical path. A refused promotion keeps the staged daemon serving.
- A discovered same-user GitHub CLI credential directory is shared with Git, execution children,
  and updater API calls; fresh boot, takeover and canonical re-exec discover it before workers start.
  Inherited `GH_CONFIG_DIR` takes precedence. Git uses transient
  `gh auth git-credential`. The updater preserves the `GITHUB_TOKEN.or_else(GH_TOKEN)` selector:
  an empty selected value falls back to system `gh auth token`, not the other environment name.
  CLI retrieval is noninteractive with a 5s lookup/drain deadline and 8KiB output cap. Its
  memory-only cache retains successes for 60s and misses for 10s, keyed by PATH, directory,
  and cwd for relative PATH entries. CLI 401 invalidates that cache and in-flight refreshes;
  explicit-token 401 does not rescue through CLI. Never export tokens or expose them in files/output.
- Execution children inherit explicit `SHELL` and `XDG_RUNTIME_DIR`. On Unix, an absent `SHELL`
  selects the effective user’s account shell only when it is absolute, executable, root/user-owned,
  and not group/other-writable. One cached login probe captures only `PATH` and `XDG_RUNTIME_DIR`,
  bounded to five seconds and 64 KiB; an absent runtime directory is filled only from that probe.
  Never guess a runtime directory or forward the login shell’s wider environment.

## Runtime invariants

- The dream cycle trigger ticks every 60 s (`DREAM_RSI_CYCLE_TICK_INTERVAL_MS`, `spawn_dream_rsi_cycle_ticker` in `crates/agentplug-runner/src/daemon.rs`). Each queueing needs 20 new observations (`MIN_NEW_OBSERVATIONS`) and 15 min since the owner's last cycle reply or deferral (`COOLDOWN_MS`, `crates/agentplug-runner/src/dream_cycle.rs`).
- Dream observation maintenance queues only registered `dream-replay-cycle` for the
  actual owner through canonical `session_id`. Private atomic per-owner state is
  persisted before its fixed pending request is published; recover that same request,
  never advance the dispatch cursor on queueing or deferred/failed replies. Accept
  acknowledgments only after its input/claim is gone, from a matching owner/cycle
  response with new verified replay
  evidence. Opaque dispatch IDs are compared for equality, not ordered. Missing
  cursors in the capped observation window indicate partial coverage, not exact
  counts. Replies and observations are bounded to 1 MiB; walks process at most 64
  entries and check a cooperative 50 ms budget between entries. Rotate roots after
  batches of eight entries; retain at most 256 root-directory cursors and remove
  deregistered roots. Cache pressure explicitly defers discovery, never proves
  complete coverage of an unbounded roster. Retry attempts have a durable
  fifteen-minute cooldown; maintenance never evaluates or deploys policies.

- `host_fs_readdir` returns zero on directory or entry-read failure, never a successful empty
  or partial array. Structural indexing propagates that failure and refuses pruning or graph
  evidence; legacy guest wrappers may explicitly retain their empty-list fallback.
- `host_fs_read` reserves packed value `1` for a successful empty UTF-8 read, without allocation.
  Actual path, I/O and UTF-8 failures remain `0`; nonempty reads retain their pointer/length ABI.
  Guests must recognize the empty marker before decoding a pointer; older guests still refuse it.
- Spawn and JavaScript adoption share process-instance checked monotonic IDs and never overwrite
  occupied entries. Failed registration cleans up only the newly owned child and process group.
  Task output first checks the live registry, then its private durable result store; missing handles
  distinguish registry-instance mismatch from a missing current-instance task.
  Adopted tasks expose raw captured streams, without decoding foreground JavaScript result frames.
- Runner handoff acquires execution admission before preserving completed results; active execution,
  children, or pipe drains defer it. Acquire shared admission before a child can execute and retain
  it through registration/adoption. Failed preparation or ownership transfer releases admission;
  successful transfer closes it before the old host can start another child.
- Completed results retain the last 64 KiB of each stream with explicit omitted-byte counts for
  30 minutes after child exit. Private schema-checked atomic records are bounded to 1 MiB each,
  512 entries and 64 MiB total; directory scans stop at 1024 entries or five seconds. Expiry and
  explicit task-stop remove records; full or invalid stores refuse handoff without evicting
  unexpired results. Default task-list does not persist; explicit `prepare_handoff:true` uses the
  production preparation function and releases its guard without transferring ownership.
  New and loaded records share validation; reversed wall-clock timestamps refuse preservation.
  Unix result directories/files must be private and effective-user-owned. `libc::geteuid` has no
  pointer inputs or failure mode; record reads reject symlinks with `O_NOFOLLOW`.
- The spool daemon is a singleton per project. Requests are written atomically, claimed by rename,
  and identified by `(verb, session-id-task-number)`; preserve those properties when changing
  dispatch or recovery.
- Dispatch concurrency is intentionally lane-based: read-only work remains parallel while state,
  store, and Git mutations serialize only against their own lane. A claimed request must always
  produce an out-file or be released for recovery.
- Kill only processes owned by this host.
- Every subprocess has a bounded wall-clock deadline and tree cleanup. Keep stdout/stderr draining
  concurrent with child execution, and keep oversized dispatch results in a spill file rather than
  an unbounded JSON reply.
- Foreground execution waits at most 50 ms for both output drains after child exit, capped by the
  remaining execution deadline. Unfinished readers transfer to task ownership; they do not prove a
  descendant holds a pipe.
- Foreground default JavaScript results use the last sentinel candidate followed by a complete JSON line;
  remove only that validated frame. Sentinel text inside returned strings or ordinary stdout
  must remain data, including when stdout has no preceding newline.
- Plugin reload recovery must receive the current engine and module map at dispatch construction.
  Nested calls reload registered empty sibling pools from that module map after shared-store eviction;
  populated or busy pools retain their owners. Failed reloads remain failures, not indexed evidence.
  If poisoned-store recovery fails, capture the active module map and instantiation error before
  changing registry behavior.
- The `gm` pool defaults to 8 slots (`DEFAULT_GM_POOL_SIZE`, capped by host parallelism and
  `MAX_GM_POOL_SIZE`; `gm_pool_size` in `daemon-config.json` overrides). A dispatch holds one slot for
  its whole run, including a long `exec_js`, so the pool size bounds how many slow dispatches can run
  before a quick one waits. Heavy verbs stay capped at `MAX_CONCURRENT_HEAVY_DISPATCHES` (3) slots.
- The claim pass is the daemon main loop, so anything synchronous on that thread delays every queued
  dispatch in every project. Measured on a scratch daemon with 128 registered roots of 139 empty verb
  dirs each, claim latency p50 15 s / p95 28 s / max 30 s before and p50 2.1 s / p95 6.0 s / max 8.1 s
  after. The causes, largest first: a `git rev-parse --show-toplevel` per registry entry on every poll
  (now `cached_project_root`, 10 min TTL); the browser orphan sweep and the plugin and runner update
  polls inline (now a housekeeping timer thread and the `update-poll` worker, which returns a staged
  runner over a channel); and re-reading every empty verb dir each pass (now skipped with one `stat`
  while its mtime is unchanged and at least 2 s old). A new synchronous call in the main loop needs the
  same measurement.
- Every Windows child spawn in `agentplug` goes through `windowless::apply_windowless`, and
  `agentplug-runner` calls `ensure_hidden_console()` before anything else. `CREATE_NO_WINDOW` alone
  leaves each console-subsystem child (git, node, powershell) console-less, so its own console
  children allocate a fresh conhost and flash a window. A console the runner owns and hides is
  inherited by the whole subtree; `apply_windowless` falls back to `CREATE_NO_WINDOW` only when no
  console exists. Source: `agentplug/crates/agentplug-host/src/windowless.rs`.

## Configuration and compatibility

- `.agentplug/plugins.json` declares download specs; `.agentplug/plugins.txt` declares loaded
  plugins. A project declaration overrides built-ins of the same name.
- Do not add tracked long generated paths: Windows release checkout has a practical path-length
  limit. `.agentplug-kv` is runtime index state and must not be committed.

## agentplug-host: contracts moved from source comments

- Child ledger (`child_pids.rs`): each spawn appends one JSON line to `<root>/.gm/exec-spool/.child-pids.jsonl`. The host and `gm-mcp/src/child-ledger.js` both append without a lock. The file rotates to `.jsonl.1` above 1 MiB. A pid no ledger claims is never a reaping candidate. `exec_js` records the pid before any step that can fail.
- Redirects (`build_agent_without_redirects`): 3xx replies go back to the caller, so each hop can refuse a private address.
- Timeouts (`http_agent.rs`): the shared API agent uses 10 s. The download agent uses 900 s, because a release binary is about 14 MB and a plugin wasm is larger, and the whole body must fit the timeout on a slow link.
- Watcher log (`watcher_log.rs`): rotates at 10 MiB, keeps 2 generations (oldest first) before each append, and a rotation failure never drops the appended line. Incident: a log on C:\dev\gm grew to 18 MB because the prose promised rotation and nothing rotated.
- Runner console (`windowless.rs`): `ensure_hidden_console` is a no-op on purpose. Windows Terminal hosts every console and SW_HIDE does not hide its tab, so an allocated runner console opened a visible tab on each start. Children get CREATE_NO_WINDOW from `apply_windowless`.
- Job objects (`process_tree.rs`): members outlive the job handle. `terminate_job` is the only path that kills through the job.
- Precompiled artifacts (`precompiled.rs`): a compiled artifact is a pure function of the wasm bytes and the engine compatibility hash, so one copy serves every install dir. Publishing hard-links the artifact into the system dir and re-points the local name at those bytes. Hard links are safe because no writer mutates an artifact in place: writers stage a sibling and rename it over the path, and superseded artifacts are unlinked. Any failed step leaves the local artifact, so loads still succeed. Measured before this rule: bert took 4051 ms and 73 MB per install dir, with about 30 scratch homes per session.
- Crawl body grammar (`crawl.rs`, parsed by `agentplug-lightpanda`): an optional first non-blank line `engine=cdp|lightpanda`. Every other non-blank line that does not start with `#` is one step. Steps split on the first whitespace, so `wait 500` and `cdp Page.navigate` read alike. `cdp <Domain.method>` takes JSON params on the following line, and omitting it sends no params. `events <Domain.event> [where <field>~<value>] [limit=N]` captures events from the moment the call attaches. The tool-step list is `TOOL_STEP_GRAMMAR`.
- Crawl profile marker: the gm-owned crawl Chrome is closed by a match on its profile path in the command line. User Chrome never carries that path.
- Lightpanda engine (`set_lightpanda_engine`): the runner registers the native engine once at startup, because the engine crate depends on this crate and cannot be linked here.
- `run_helper`: the Node CDP helper is fed on stdin and receives its config in `GM_CRAWL_CONFIG`. The child is killed when the budget runs out.
- `crawl_cdp`: headful Chrome over CDP on the single browser shared by a project root. The call holds a lease and releases it on return.
- `crawl_cdp.mjs` (concatenated after `crawl_cdp_tools.mjs`, run as `node --input-type=module` on stdin): config JSON in `GM_CRAWL_CONFIG` has the shape `{ port, targetId?, browserSession?, cwd?, session?: {name, file}, steps, pageTimeoutMs, textLimit }`. `browserSession` selects the lightpanda protocol, where targets are created and attached over the browser websocket, because lightpanda serves no HTTP target endpoints. A session keeps its tab and uid map across calls, and its file is written atomically. The helper prints exactly one JSON object on stdout and exits 0 when every step ran.
- Browser lease (`crawl_lease.rs`): holders restamp `heartbeatAt` every 2 s (`HEARTBEAT_MS`). A holder whose heartbeat is older than 120 s (`TTL_MS`) is dead whatever its pid says. Liveness is heartbeat freshness, which reads the same on every OS. Acquire waits up to 600 s (`WAIT_MS`) for a live holder. Claims are exclusive because `create_new` on `owner.json` lets exactly one racer win. A claim file that exists but is unreadable belongs to a peer still writing it for 5 s (`CLAIM_CREATE_GRACE_MS`), so it is not a dead holder. Takeover kills the holder it names, reaps browsers on the profile, and unlinks the claim. Release removes only the caller's own claim. A shared Chrome with no lease stays hot for 10 min (`BROWSER_HOT_IDLE_MS`, env `GM_BROWSER_IDLE_MS`). Housekeeping runs every 30 s and closes dead, idle-expired and lost-claim browsers. The session dir is `.gm/browser-lease/session` per project root. Verbs `browser_lease_acquire`, `browser_lease_release` and `browser_lease_status` take `{root, agent, ttl_ms, wait_ms}`. `root` defaults to the dispatch root, and `agent` is required for acquire and release.
- `exec_js` SAFETY notes (no clippy lint enforces them): the passwd buffer stays live until the shell bytes are copied, and only the effective UID is queried. `fcntl` changes only read-side status flags of the owned pipe, and `O_NONBLOCK` is a valid status flag.
- `exec_js` module split: top-level `import` and `export` declarations are lifted from a stdin body, so the body runs with `--input-type=module`. A CommonJS body has an empty head and runs byte-identically. `require`, `module.exports` and `exports.x` never match, and dynamic `import(...)` is untouched. Comments and string bodies are skipped. A declaration ends at the first `;` or newline at bracket depth 0.
- Agent exec (`agent` and `claude` langs): the code is the prompt for a headless Claude Code agent, so a subagent with no host Agent tool can start its successor. Print mode cannot answer permission prompts, so the default permission mode is `DEFAULT_AGENT_PERMISSION_MODE` (`bypassPermissions`), matching the ungated exec surface.
- Login PATH: the first exec probes the login shell for PATH (cached, capped at 5 s). Tests warm that cache before their clock starts.
- Git memo (`imports.rs`): layout questions (`rev-parse --show-toplevel` and its siblings) are answered once per (directory, question). An answer is forgotten after a git command that can move the layout runs in an overlapping directory. Flags outside the list (`status --porcelain`, `hash-object`, gating `ls-files`) always reach git. An answer guarded by a file identity is dropped by that identity alone, so unguarded calls cannot delete listings. The `ls-files --stage` answer holds while the index mtime and length are unchanged. HEAD is read from disk when it is a loose symref. Packed refs, symref chains and unreadable `.git` fall through to git. Listings above 1 MiB are not memoized. The stats flush every 60 s and count asked versus reached (a real git process). One argv asked hundreds of times a minute marks a thrash.
- Durable rename: the tmp file and its parent directory are both synced. Without both syncs, a crash can leave the old or empty target while success is reported.
- Admission hold (`registry.rs`, bound `ADMISSION_HOLD_MAX_MS_DEFAULT` = 60 s, env `AGENTPLUG_ADMISSION_HOLD_MAX_MS`): a counter is a lease, not a grant for the dispatch's life. A waiter reclaims the oldest general-band counter older than the bound, and only while a physical slot is free. Reclaim returns the counter and does not stop the holder. Incident: a `git_push` held a counter for 1417734 ms and a `git_finalize` for 1164435 ms while a slot idled. Twenty starvations on 2026-10-12, all naming three holders, followed. `release_holder` returns false for an unknown id, so a reclaimed guard cannot release twice.
- `git_band_admission_limit` uses `.max(1)`. At POOL_SIZE 4, `short_reserved_slots()` is 1, so `1 - 1` admitted nobody and the git lane had no fallback. One git slot is the intended size.
- `admission_holds` lists every counter with its age, next to `lane_holds`, so stalls are read from the ledger.
- `acquire_slot_until`: past `give_up_at` the ticket is abandoned, the FIFO skips it, and the caller answers admission-starved.
- Admission ordering: a dispatch takes a plugin slot first and admission second. If admission is refused, it hands the plugin slot back and requeues. Waiting for a plugin slot while admitted wedges the gate, and waiting for admission while holding a slot takes the slot reserved for short verbs. `SLOT_HELD_ADMISSION_WAIT_MS` (250 ms) is that brief wait, and the gate does not count waiting dispatches.
- `release_admission_for_thread`: a dispatch calls it after writing its result. Normal unwinding already released the guards, so it is a no-op. A thread that died holding a slot leaves no guard, and that slot would starve later dispatches.
- Plugin recovery: `RememberedPluginModule` lets a refill rebuild an evicted slot in place when the published module map transiently lacks the plugin during a wasm recompile. Before this, an empty slot answered every later dispatch as unusable.
- `note_pending_eviction_consumed` counts owed evictions, and the entry disappears when the last one is paid. An older rule removed entries only when the same bytes were named again, which emptied a slot after every successful dispatch.
- Evicted-slot refill: a run that hit the call deadline is not repeated, because a second identical run would spend the budget again and hold the slot.
- Git network lane (`GIT_NETWORK_LANE_VERBS`: `git_push`, `git_pull`, `git_fetch`): hold time is set by the remote, in minutes, where a commit takes seconds. These verbs stay on the git lane, because one writer per worktree keeps git serialized. `git_push` can pull --rebase the checkout it publishes from. They yield their place to checkout-bound claims behind them until `LANE_NETWORK_YIELD_MS` (120 s, env `AGENTPLUG_LANE_NETWORK_YIELD_MS`) of their own waiting has passed.
- Lane queue (`LaneWaiter`): FIFO by claim age. The identity is the verb plus a hash of the body, which stays stable across re-dispatch while the spool mints fresh task ids and tokens. A re-dispatch reclaims its parked token. An entry survives starvation and is dropped when its dispatch gets the lane, or by TTL (`LANE_TICKET_TTL_MS`, 600 s) on `last_seen_ms`. `claimed_ms` is never refreshed, so it bounds the network yield. `active` is true while a dispatch is inside `acquire_within`. A place is inherited only from a parked ticket, so two byte-identical bodies from different callers get one ticket each.
- `LANE_TICKET_STALE_MS` (30 s): a waiter not re-claimed in that window stops contending, and the lane passes on. `lane_queue_depth` counts active and recently re-dispatched claims as contending. Parked remainders are reported as stale. They are neither added nor dropped.
- `lane_admissible` runs under the owner lock, so the head cannot change between check and insert. A claim ahead that stopped contending does not block. A network-bound claim ahead is outranked by a checkout-bound claim only before its own yield window ends.
- `requeue_lane_claimer_front`: a displaced holder (taken by the hold watchdog) is parked back at the head with its identity, because it never ran and is owed its place.
- `GmFairnessGuard::still_owns`: a stuck holder that lost the lane checks this before dispatching and answers starved, so a holder that lost the lane never touches the worktree.
- Lane release: `release_abandoned_lane_owners` frees lanes whose project root is no longer live. `release_dead_lane_owners` frees lanes whose one holder the caller reports as not live. A holder with an unknown verb or task is never reaped. The owning dispatch keeps running. Its guard finds a different token on drop and leaves the new owner alone.
- `lane_holds` is a watchdog read. It never releases a live holder, because one writer per lane keeps the index consistent. `lane_waiter_count` lets the watchdog spend that property only when someone is waiting.
- `UNSERIALIZED_STORE_WRITER_VERBS` (`codeinsight`, `callers`, `callees`, `impact`) run outside the store lane by design, so a `callers` question never queues behind an index pass. They must still claim `.gm/gm.db.lock.owner`, because the store liveness rule treats a lock with no owner record as having no live writer. `dispatch_claims_store_owner` is the ownership predicate.
- `fs_write` is not on the state lane. Each write is a per-path mutex plus an atomic temp-file rename (`atomic_write_locked`), so writes cannot interleave on one path.
- `LaneQueueState.lane_position` is 1-based and counts only waiting dispatchers. It is read through `.dispatch-wait.json`. Lock order is owners, then waiters.
- Lane yield class: git-network verbs are class 0 and every other verb is class 1. Only the relative order inside one lane matters.
