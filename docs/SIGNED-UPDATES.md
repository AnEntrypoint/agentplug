# Signed runner and plugin updates

`agentplug-runner` downloads its own binary and every plugin `.wasm` from GitHub
releases and installs them automatically. A sha256 sidecar published in the same
release as the artifact it checks only proves that the bytes match what that
channel says they are: whoever can publish the release controls both sides of
the check, which is how a release carrying an older build can be promoted over a
newer one under the same version tag. A detached ed25519 signature over the
artifact adds a second, independent check against public keys pinned locally and
never written by the updater.

## Default: warn

With no trust file, updates install as they always did, and every unverified
install is logged once per version and recorded under the install dir. Nothing
is refused and nothing is frozen for anyone who has not opted in.

## Opting a machine into strict mode

Strict mode refuses to stage or promote a runner update that does not verify:

- env `AGENTPLUG_REQUIRE_RUNNER_SIGNATURE=1`, or
- `require_runner_signature: true` in `~/.agentplug/daemon-config.json`

Either one is enough; `agentplug-runner trust-status` reports
`runner_signature_required` and which of the two turned it on. Strict mode
applies to the runner asset only, so a machine that pins runner signatures is
not also blocked from installing unsigned plugin `.wasm` files.

Strict mode is fail-closed: with no pinned key, or with a release that has no
`.sig`, every runner update is rejected and the running version stays. Pin a
key before turning it on.

`AGENTPLUG_NO_SELF_UPDATE`, `agentplug-runner.no-self-update`, the local-build
pin and the strictly-newer version requirement are all still checked first, and
the local-build check is re-run at takeover; `AGENTPLUG_ALLOW_UPDATE_OVER_LOCAL_BUILD=1`
still overrides it.


## Local source and release bootstrap

A local checkout is never an updater source. The runner only stages a strictly newer published release artifact and verifies it under the configured trust mode. It does not read, export, copy, or configure GitHub credentials, and it does not replace a runner binary or plugin from local source.

Run `agentplug-runner release-bootstrap-status` to inspect this boundary. If a local source fix has not yet reached a published release, publish it through the repository's ordinary CI release workflow; after CI publishes the artifact, the daemon's normal update path can stage it.

## Trust file

`~/.agentplug/trusted-keys.json` (or `$AGENTPLUG_HOME/trusted-keys.json`).
Never created or modified by the runner.

```json
{
  "mode": "warn",
  "threshold": 1,
  "keys": [
    { "id": "2026-offline-primary", "public_key": "<64 hex chars>" }
  ]
}
```

- `mode`: `off` (no verification at all), `warn` (verify, log loudly, install
  anyway), `enforce` (refuse and keep the running version). A missing file
  behaves as `warn` with a one-time notice; a file with `mode` omitted behaves
  as `enforce`.
- `threshold`: how many distinct pinned keys must sign an artifact. Use 2 during
  key rotation so the old key alone and the new key alone are each insufficient
  while both remain valid signers of a transition release.
- `min_sequence` (optional): `{"<asset name>": <n>}` floors for hand-raising the
  rollback floor of one asset.

## Generating a key pair (on a clean, offline device)

```
cargo run -p agentplug-trust --bin agentplug-sign -- keygen --id 2026-offline-primary --out /path/on/removable/media
```

It refuses to run under CI (`GITHUB_ACTIONS`/`CI` set) and refuses to write
inside a git work tree. Copy the printed `public_key` into `trusted-keys.json`
on every machine that should trust it. Keep the `.secret` file offline.

## Signing a release artifact

```
agentplug-sign sign --key /path/to/2026-offline-primary.secret \
  --artifact ./agentplug-runner-windows-x64.exe \
  --name agentplug-runner-windows-x64.exe \
  --version 0.1.200 --sequence 200 \
  --out agentplug-runner-windows-x64.exe.sig
```

`--sequence` must increase on every release of that asset; the runner rejects a
lower sequence than it has already accepted even when the signature is valid, so
a re-published older build cannot roll a machine back. Add a second `--key` (or
`--merge <existing .sig>`) for a threshold-2 rotation.

## Publishing signatures

The updater fetches `<asset url>.sig` next to the asset it is downloading, so
publishing a signature is just uploading that one file to the same release:

```
gh release upload v0.1.200 ./agentplug-runner-windows-x64.exe.sig --repo AnEntrypoint/agentplug-bin
```

CI holds no private key, so it cannot produce a signature -- it only attaches
one. `release.yml` reads a committed `release-signatures/manifest.json`
(`{"entries": [<doc>, ...]}`) and, for each built asset whose name and sha256
match an entry there, writes that entry out as `<asset>.sig` before publishing.
An asset with no matching entry still publishes, unsigned, under a `::notice`.
Produce the manifest entries offline with `agentplug-sign sign` as above, then
commit them to `main`.

## Verifying locally

```
agentplug-sign verify --artifact ./agentplug-runner-windows-x64.exe \
  --sig ./agentplug-runner-windows-x64.exe.sig \
  --trust-dir ~/.agentplug --version 0.1.200
```

Exit 0 verified, 2 accepted unverified (off/warn), 1 rejected.

## Where an unverified install is recorded

- `~/.agentplug/update-events.json`: `unverified` and `rejected` per artifact,
  with version, reason and timestamp. The WARN is logged only when the version
  or the reason changes, so a poll that keeps failing the same way stays quiet.
- `~/.agentplug/runner-unverified-update.json`: written when an unverified
  runner is actually promoted, with the release tag, sha256 and reason.
- `~/.agentplug/daemon-status.json`: `runner_update_trust_mode`,
  `runner_signature_required` and `runner_unverified_update`.
- `agentplug-runner trust-status` prints all of it.

## Switching this machine to enforce

1. Generate a key pair as above.
2. Write `~/.agentplug/trusted-keys.json` with `mode: "warn"` and the public
   key; the runner reads it on its next poll, no restart needed.
3. Confirm `agentplug-runner trust-status` shows `configured: true`, no
   `problem`, and no `rejected` entry for the runner.
4. Wait for or trigger an update and confirm the log shows
   `[agentplug update-trust] verified ...`.
5. Set `mode` to `"enforce"`, or set `AGENTPLUG_REQUIRE_RUNNER_SIGNATURE=1`.
