# Runner and plugin update integrity

`agentplug-runner` downloads its own binary and every plugin `.wasm` from GitHub
releases and installs them automatically. Integrity is the sha256 digest published
next to each artifact (`<asset>.sha256` in the same release). The updater downloads
the artifact, hashes the bytes, and installs them only when the digest matches the
sidecar. A mismatch is a hard error and nothing is written.

The sidecar comes from the same release channel as the artifact. The check therefore
proves that the downloaded bytes are the bytes that release published (transport
corruption, truncation, and a partial or tampered download are caught). It does not
prove who published the release.

## What the updater enforces

- The sha256 of every downloaded runner or plugin matches its published sidecar.
- A runner self-update is staged only when the latest release tag is strictly newer
  than the running version (see `AGENTPLUG_NO_SELF_UPDATE` and
  `agentplug-runner.no-self-update` for the freeze switches).
- A locally built runner is never replaced by a release unless it is unpinned or
  `AGENTPLUG_ALLOW_UPDATE_OVER_LOCAL_BUILD=1` is set (`pin-local-build` and
  `unpin-local-build` maintain the pin).
- A staged runner is promoted onto the canonical path only after the local-build
  guard passes again at takeover, and the staged copy is self-checked with
  `--version` before handoff.
- A plugin whose release is older than the installed version is refused as a
  downgrade. A plugin version that is unchanged is refetched only when its sidecar
  sha256 changes.

## Local source and release bootstrap

A local checkout is never an updater source. The runner only stages a strictly newer
published release artifact. It does not read, export, copy, or configure GitHub
credentials, and it does not replace a runner binary or plugin from local source.

Run `agentplug-runner release-bootstrap-status` to inspect this boundary. If a local
source fix has not yet reached a published release, publish it through the repository's
release workflow; after CI publishes the artifact, the daemon's normal update path can
stage it.

## Release publication

`.github/workflows/release.yml` runs on every push to `main`. It bumps the patch
version, builds each runner target, writes a `.sha256` next to every built asset, and
publishes the release only after the draft holds exactly the built asset set. Each
published asset is then checked for HTTP 200.

## Daemon and guard counts per install dir

An install dir is `install_dir()`: `AGENTPLUG_HOME` when it is set, otherwise
`~/.agentplug`. One install dir runs **at most one daemon and at most one
daemon-guard**, so N live install dirs legitimately mean N daemons and N guards.
A second watcher in one install dir is never a spare: each watcher's orphan sweep
answers `dispatch_orphaned` for the other's running work and deletes the claim
under it.

- `daemon-owner.lock` elects the single daemon. A boot that does not win it exits
  before touching any shared plugin state.
- `daemon-guard.lock` names the guard's pid. It stays a **bare integer** so an
  older runner image still understands it and still refuses to start a second
  guard; the guard's identity (pid, exe, version, install dir, heartbeat ts)
  lives beside it in `daemon-guard.owner.json`.
- A guard re-reads the lock every poll and **retires itself** when it no longer
  holds it, so a surplus guard exits on its own. Nothing is killed from outside.
- A lock whose pid is dead is always reclaimable. A lock whose pid is alive is
  reclaimed only when the holder cannot prove its mandate: no owner record, a
  record naming some other pid, or a heartbeat older than 120 s. Proof, not
  seniority, is what keeps the lock.
- A guard in an **overridden** install dir (`AGENTPLUG_HOME` set) whose daemon has
  published `active_projects: 0` for 10 minutes asks the daemon to stop
  (`daemon-stop-request.json`) and retires with it. The daemon exits by design on
  its own loop, and only after 60 s of its own quiet, so a stop never lands under
  dispatch work the guard cannot see. The shared default home is never torn down
  this way; its daemon self-recycles on its own idle clock.
- A daemon that stops by design leaves `daemon-intentional-exit.json` in place.
  Every watcher of that home reads it and stands down, instead of the first one
  deleting it and the rest restarting the daemon that just retired.

## Where updater decisions are recorded

- `~/.agentplug/daemon-status.json`: runner version parity, the last completed runner
  swap, and the last handoff attempt and its error.
- `agentplug-runner release-bootstrap-status` prints the local-source boundary.
