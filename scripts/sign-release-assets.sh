#!/usr/bin/env bash
set -euo pipefail

asset_dir=${1:?asset directory is required}
version=${2:?release version is required}
source_dir=${3:?source directory is required}
key_id=${AGENTPLUG_RELEASE_SIGNING_KEY_ID:?AGENTPLUG_RELEASE_SIGNING_KEY_ID is required}
key=${AGENTPLUG_RELEASE_SIGNING_KEY:?AGENTPLUG_RELEASE_SIGNING_KEY is required}

if [[ ! -d "$asset_dir" ]]; then
  echo "release asset directory does not exist: $asset_dir" >&2
  exit 1
fi
if [[ ! "$key_id" =~ ^[[:alnum:]_.-]+$ ]]; then
  echo "release signing key id has unsupported characters" >&2
  exit 1
fi
key=${key//$'\n'/}
key=${key//$'\r'/}
if [[ ! "$key" =~ ^[[:xdigit:]]{64}$ ]]; then
  echo "release signing key must be a 64-hex-digit ed25519 seed" >&2
  exit 1
fi
if [[ ! "$version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]; then
  echo "release version must be numeric major.minor.patch" >&2
  exit 1
fi

major=${BASH_REMATCH[1]}
minor=${BASH_REMATCH[2]}
patch=${BASH_REMATCH[3]}
sequence=$((10#$major * 1000000000000 + 10#$minor * 1000000 + 10#$patch))
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
umask 077
key_file="$work/$key_id.secret"
printf '%s\n' "$key" > "$key_file"
unset AGENTPLUG_RELEASE_SIGNING_KEY
export AGENTPLUG_RELEASE_SIGNING_CI_AUTHORIZED=true
public_key=$(cargo run --quiet --locked --manifest-path "$source_dir/Cargo.toml" -p agentplug-trust --bin agentplug-sign -- pubkey --key "$key_file")
jq -n --arg id "$key_id" --arg public_key "$public_key" '{mode:"enforce",threshold:1,keys:[{id:$id,public_key:$public_key}]}' > "$work/trusted-keys.json"

shopt -s nullglob
assets=("$asset_dir"/*)
if ((${#assets[@]} == 0)); then
  echo "release asset directory is empty: $asset_dir" >&2
  exit 1
fi
signatures=()
for asset in "${assets[@]}"; do
  name=$(basename "$asset")
  case "$name" in
    *.sha256|*.sig) continue ;;
  esac
  if [[ ! -f "$asset" ]]; then
    echo "release asset is not a regular file: $asset" >&2
    exit 1
  fi
  signature="$work/$name.sig"
  cargo run --quiet --locked --manifest-path "$source_dir/Cargo.toml" -p agentplug-trust --bin agentplug-sign -- \
    sign --allow-ci --key "$key_file" --artifact "$asset" --name "$name" --version "$version" --sequence "$sequence" --out "$signature"
  signatures+=("$signature")
done
if ((${#signatures[@]} == 0)); then
  echo "release asset directory contains no signable artifacts: $asset_dir" >&2
  exit 1
fi
jq -s '{entries:.}' "${signatures[@]}" > "$work/manifest.json"
scripts/attach-release-signatures.sh "$asset_dir" "$work/manifest.json" "$version" "$source_dir" "$work"
