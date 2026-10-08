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

## Where updater decisions are recorded

- `~/.agentplug/daemon-status.json`: runner version parity, the last completed runner
  swap, and the last handoff attempt and its error.
- `agentplug-runner release-bootstrap-status` prints the local-source boundary.
