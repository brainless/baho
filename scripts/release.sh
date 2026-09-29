#!/usr/bin/env bash
set -euo pipefail

if [[ $# -eq 1 && ( "$1" == -h || "$1" == --help ) ]]; then
  echo "Usage: ./scripts/release.sh X.Y.Z"
  echo "Set both app versions first, then run from a clean, pushed main branch."
  exit 0
fi
if [[ $# -ne 1 || ! "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Usage: ./scripts/release.sh X.Y.Z" >&2
  exit 1
fi

version="$1"
tag="v$version"
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

for command in git gh; do
  command -v "$command" >/dev/null || { echo "Missing $command" >&2; exit 1; }
done

if [[ "$(git branch --show-current)" != main ]]; then
  echo "Release must run from main" >&2
  exit 1
fi
if [[ -n "$(git status --porcelain)" ]]; then
  echo "Commit or stash working tree changes before releasing" >&2
  exit 1
fi
remote_main="$(git ls-remote origin refs/heads/main | awk '{ print $1 }')"
if [[ -z "$remote_main" || "$(git rev-parse HEAD)" != "$remote_main" ]]; then
  echo "Local main must match origin/main before releasing" >&2
  exit 1
fi

for manifest in apps/baho-cli/Cargo.toml apps/baho-gui/Cargo.toml; do
  package_version="$(awk -F '"' '/^version = "/ { print $2; exit }' "$manifest")"
  if [[ "$package_version" != "$version" ]]; then
    echo "$manifest has version $package_version; expected $version" >&2
    exit 1
  fi
done

if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "Local tag $tag already exists" >&2
  exit 1
fi
remote_tag="$(git ls-remote --tags origin "refs/tags/$tag")"
if [[ -n "$remote_tag" ]]; then
  echo "Remote tag $tag already exists" >&2
  exit 1
fi

echo "Create and push $tag, then start the release workflow for both macOS binaries."
read -r -p "Proceed? [y/N] " answer
[[ "$answer" =~ ^[Yy]$ ]] || exit 1

git tag "$tag"
git push origin "$tag"
gh workflow run release.yml --ref main -f "tag=$tag"
echo "Release workflow started for $tag"
