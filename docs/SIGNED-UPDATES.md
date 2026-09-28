# Signed runner and plugin updates

`agentplug-runner` downloads its own binary and every plugin `.wasm` from
GitHub releases and installs them automatically. Integrity today is a sha256
sidecar published in the same release as the artifact it checks, so whoever
can publish that release controls both sides of the check. This adds a second,
independent check: a detached ed25519 signature over the artifact, verified
against public keys you pin locally and that the updater itself never writes.

## Trust file

`~/.agentplug/trusted-keys.json` (or `$AGENTPLUG_HOME/trusted-keys.json`).
Never created or modified by the runner.

```json
{
  "mode": "enforce",
  "threshold": 1,
  "keys": [
    { "id": "2026-offline-primary", "public_key": "<64 hex chars>" }
  ]
}
```

- `mode`: `off` (no verification, current behavior), `warn` (verify, log
  loudly, install anyway), `enforce` (refuse and keep the running version).
  Missing file behaves as `warn` with a one-time notice; a file with `mode`
  omitted behaves as `enforce`.
- `threshold`: how many distinct pinned keys must sign an artifact. Use 2
  during key rotation so either the old or the new key alone is insufficient
  but both old+new remain individually valid signers of a transition release.
- `min_sequence` (optional): `{"<asset name>": <n>}` floors, in case you ever
  need to hand-raise the rollback floor for an asset below what this machine
  has already accepted.

## Generating a key pair (on a clean, offline device)

```
cargo run -p agentplug-trust --bin agentplug-sign -- keygen --id 2026-offline-primary --out /path/on/removable/media
```

This refuses to run under CI (`GITHUB_ACTIONS`/`CI` set) and refuses to write
into a git work tree. Copy the printed `public_key` into `trusted-keys.json`
on the machines that should trust it. Keep the `.secret` file offline.

## Signing a release artifact

```
agentplug-sign sign --key /path/to/2026-offline-primary.secret \
  --artifact ./agentplug-runner-windows-x64.exe \
  --name agentplug-runner-windows-x64.exe \
  --version 0.1.200 --sequence 200 \
  --out agentplug-runner-windows-x64.exe.sig
```

`--sequence` must increase on every release of that asset; the runner rejects
a lower sequence than it has already accepted, even if validly signed. Add a
second `--key` (or `--merge <existing .sig>`) to add a second signer for
threshold-2 rotation.

## Publishing signatures

Commit the produced `.sig` documents into `release-signatures/manifest.json`
(`{"entries": [<doc>, ...]}`) on `main`. CI's release workflow attaches
`<asset>.sig` for any asset whose name and sha256 match an entry there; it
never holds a private key and never fails the build when a signature is
missing.

## Verifying locally

```
agentplug-sign verify --artifact ./agentplug-runner-windows-x64.exe \
  --sig ./agentplug-runner-windows-x64.exe.sig \
  --trust-dir ~/.agentplug --version 0.1.200
```

## Switching this machine to enforce

1. Generate a key pair as above.
2. Write `~/.agentplug/trusted-keys.json` with `mode: "warn"` and the public
   key, restart nothing (the runner reads it on its next poll).
3. Confirm `agentplug-runner trust-status` shows `configured: true` and no
   `problem`.
4. Wait for or trigger an update and confirm the daemon log shows
   `[agentplug update-trust] verified ...`.
5. Change `mode` to `"enforce"`.
