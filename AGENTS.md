# AGENTS.md

AgentPlug is the native WebAssembly host used by gm. It contains the `agentplug-runner` daemon,
the `agentplug-host` imports, and `agentplug-trust`. Work on `main` as GitHub user `lanmower`.

## Source and release boundaries

- Keep code self-explanatory. Do not add comments except Rust `// SAFETY:` justifications; retain
  language attributes and shell shebangs. Put non-derivable operational contracts here or in the
  relevant user documentation.
- A push to `main` runs `.github/workflows/release.yml`: it bumps the patch version, builds every
  runner target, signs release assets in protected CI, and publishes the release consumed by running
  daemons. Do not install, copy, or execute a local source-built runner as an update path.
- Runner updates fail closed without configured trust. `warn` and `off` are explicit operator
  choices. The locally-built-runner guard, version monotonicity, SHA-256 check, signature
  verification, and update-sequence floor are independent protections; do not weaken one to repair
  another.
- Keep signing material only in the protected CI environment. Bind each signature to the immutable
  source revision that built its artifact, and publish a release only after every expected asset and
  signature is present.
- Canonical runner promotion atomically persists the staging receipt beside the promoted binary
  and rechecks its artifact, version, bytes and signature before re-exec. Receipt or verification
  failure keeps the permitted staged daemon serving. Preserve the authoritative checker’s explicit
  `warn`/`off` policies; only `off` allows an absent receipt, never enforced signature mode.
- A discovered same-user GitHub CLI credential directory is shared with Git, execution children,
  and updater API calls; inherited `GH_CONFIG_DIR` takes precedence. Git uses transient
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
  Dispatch origin preserves its owner scoping but does not refresh session/browser
  activity for this exact maintenance verb.

- `host_fs_readdir` returns zero on directory or entry-read failure, never a successful empty
  or partial array. Structural indexing propagates that failure and refuses pruning or graph
  evidence; legacy guest wrappers may explicitly retain their empty-list fallback.
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
- `DispatchOrigin::page_session` is the only browser session resolver. Browser pages are keyed by
  project root and GM session; serialize work per page before writing its temporary files.
  Chrome and Steel reuse only their remembered target; a missing target creates a fresh owned page,
  never adopts another session's tab. Explicit `url=` reloads an identical URL; hash-only changes
  retain the document. Preserve document telemetry and captured diagnostics on evaluation failure.
- Acquire the browser page guard before global plugin admission. Waiting for the same page must
  not consume execution slots needed by unrelated commands; distinct page sessions stay parallel.
- Kill only processes and browser profiles owned by this host. An adopted or externally supplied
  CDP/Steel endpoint is never treated as host-owned merely because its PID or profile resembles one.
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
  If poisoned-store recovery fails, capture the active module map and instantiation error before
  changing registry behavior.

## Configuration and compatibility

- `.agentplug/plugins.json` declares download specs; `.agentplug/plugins.txt` declares loaded
  plugins. A project declaration overrides built-ins of the same name.
- The browser config is optional and unknown keys are ignored. Preserve explicit precedence: an
  existing Chrome endpoint, then Steel endpoint, then configured engine. Invalid extra Chrome
  arguments are rejected individually without invalidating the whole config.
- Do not add tracked long generated paths: Windows release checkout has a practical path-length
  limit. `.agentplug-kv` is runtime index state and must not be committed.
