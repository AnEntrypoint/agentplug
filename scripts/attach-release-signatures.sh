#!/usr/bin/env bash
set -euo pipefail

asset_dir=${1:?asset directory is required}
manifest=${2:?signature manifest is required}
version=${3:?release version is required}
source_dir=${4:?source directory is required}
trust_dir=${5:?trust directory is required}

if [[ ! -d "$asset_dir" ]]; then
  echo "release asset directory does not exist: $asset_dir" >&2
  exit 1
fi
if [[ ! -f "$manifest" ]]; then
  echo "release signature manifest does not exist: $manifest" >&2
  exit 1
fi
if ! jq -e 'type == "object" and (.entries | type == "array")' "$manifest" >/dev/null; then
  echo "release signature manifest must be an object with an entries array: $manifest" >&2
  exit 1
fi
if [[ ! -f "$trust_dir/trusted-keys.json" ]]; then
  echo "release trust file does not exist: $trust_dir/trusted-keys.json" >&2
  exit 1
fi

shopt -s nullglob
assets=("$asset_dir"/*)
if ((${#assets[@]} == 0)); then
  echo "release asset directory is empty: $asset_dir" >&2
  exit 1
fi

for asset in "${assets[@]}"; do
  name=$(basename "$asset")
  case "$name" in
    *.sha256|*.sig) continue ;;
  esac
  if [[ ! -f "$asset" ]]; then
    echo "release asset is not a regular file: $asset" >&2
    exit 1
  fi
  sha256=$(sha256sum "$asset" | cut -d' ' -f1)
  entry=$(jq -cer --arg name "$name" --arg sha256 "$sha256" --arg version "$version" '
    [
      .entries[]
      | select(
          .v == 1
          and .artifact == $name
          and (.sha256 | type == "string" and ascii_downcase == $sha256)
          and .version == $version
          and (.sequence | type == "number" and floor == . and . >= 0)
          and (.signatures | type == "array" and length > 0)
          and all(.signatures[]; (.key_id | type == "string" and length > 0) and (.sig | type == "string" and test("^[[:xdigit:]]{128}$")))
        )
    ]
    | if length == 1 then .[0] else error("expected exactly one matching signature entry") end
  ' "$manifest") || {
    echo "missing or malformed signature for release asset $name at version $version" >&2
    exit 1
  }
  printf '%s\n' "$entry" > "$asset.sig"
  cargo run --quiet --locked --manifest-path "$source_dir/Cargo.toml" -p agentplug-trust --bin agentplug-sign -- \
    verify --artifact "$asset" --sig "$asset.sig" --trust-dir "$trust_dir" --version "$version" --name "$name"
  echo "attached verified offline signature for $name"
done
