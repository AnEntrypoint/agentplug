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
- A discovered same-user GitHub CLI credential directory is shared with Git and execution child
  processes through `GH_CONFIG_DIR`; an explicit inherited directory takes precedence. Git uses
  the transient `gh auth git-credential` helper. Never export, copy, print, or persist its token.
- Execution children inherit explicit `SHELL` and `XDG_RUNTIME_DIR`. On Unix, an absent `SHELL`
  selects the effective user’s account shell only when it is absolute, executable, root/user-owned,
  and not group/other-writable. One cached login probe captures only `PATH` and `XDG_RUNTIME_DIR`,
  bounded to five seconds and 64 KiB; an absent runtime directory is filled only from that probe.
  Never guess a runtime directory or forward the login shell’s wider environment.

## Runtime invariants

- The spool daemon is a singleton per project. Requests are written atomically, claimed by rename,
  and identified by `(verb, session-id-task-number)`; preserve those properties when changing
  dispatch or recovery.
- Dispatch concurrency is intentionally lane-based: read-only work remains parallel while state,
  store, and Git mutations serialize only against their own lane. A claimed request must always
  produce an out-file or be released for recovery.
- `DispatchOrigin::page_session` is the only browser session resolver. Browser pages are keyed by
  project root and GM session; serialize work per page before writing its temporary files.
- Acquire the browser page guard before global plugin admission. Waiting for the same page must
  not consume execution slots needed by unrelated commands; distinct page sessions stay parallel.
- Kill only processes and browser profiles owned by this host. An adopted or externally supplied
  CDP/Steel endpoint is never treated as host-owned merely because its PID or profile resembles one.
- Every subprocess has a bounded wall-clock deadline and tree cleanup. Keep stdout/stderr draining
  concurrent with child execution, and keep oversized dispatch results in a spill file rather than
  an unbounded JSON reply.
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
