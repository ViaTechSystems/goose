#!/usr/bin/env bash
set -euo pipefail

cargo_toml=${1:-Cargo.toml}
desktop_package_json=${2:-ui/desktop/package.json}

workspace_version=$(sed -n 's/^version = "\([^"]*\)"$/\1/p' "$cargo_toml" | head -n 1)
desktop_version=$(jq -er '.version' "$desktop_package_json")

if [[ -z "$workspace_version" ]]; then
  echo "Could not read the workspace version from Cargo.toml" >&2
  exit 1
fi

if [[ "$workspace_version" != "$desktop_version" ]]; then
  echo "Release version mismatch: Cargo.toml=$workspace_version ui/desktop/package.json=$desktop_version" >&2
  exit 1
fi

echo "Release versions agree: $workspace_version"
