#!/usr/bin/env bash
# Move the PSCat dependency to another release tag.
#
#   scripts/bump-pscat.sh v0.7.0                # a specific tag
#   scripts/bump-pscat.sh --latest              # the newest v* tag (pre-releases included)
#   scripts/bump-pscat.sh v0.7.0 --release-version 0.2.0   # also set this server's version
#   scripts/bump-pscat.sh v0.7.0 --test         # then run the test suite
#
# The tag in Cargo.toml is the single place the PSCat version is chosen.
# Cargo.lock records the exact commit, build.rs reports the version from
# Cargo.lock, and every build uses `--locked`, so the binary, the image tag
# (pscat-<version>) and get_renderer_info all agree.
#
# Refuses to move to an older tag unless --allow-downgrade is given. When
# GITHUB_OUTPUT is set (GitHub Actions), writes changed/old_tag/new_tag/
# old_version/new_version to it.
#
# Needs: git, cargo, python3.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
REPO_URL="https://github.com/PSCatLabs/pscat"

usage() { sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit "${1:-2}"; }

target=""
release_version=""
run_tests=0
allow_downgrade=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --latest) target="--latest" ;;
    --release-version) release_version="${2:?--release-version needs a value}"; shift ;;
    --test) run_tests=1 ;;
    --allow-downgrade) allow_downgrade=1 ;;
    -h|--help) usage 0 ;;
    -*) echo "unknown option: $1" >&2; usage ;;
    *) target="$1" ;;
  esac
  shift
done
[[ -n "$target" ]] || usage

# Semver ordering (pre-releases sort before the release: 0.7.0-rc.4 < 0.7.0).
semver_max() {
  python3 -I -c '
import re, sys
def key(tag):
    m = re.fullmatch(r"v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?", tag)
    if not m:
        return None
    core = tuple(int(x) for x in m.groups()[:3])
    pre = m.group(4)
    if pre is None:
        return core + ((1,),)
    parts = tuple((0, int(p), "") if p.isdigit() else (1, 0, p) for p in pre.split("."))
    return core + ((0,) + parts,)
tags = [t for t in sys.stdin.read().split() if key(t) is not None]
print(max(tags, key=key) if tags else "")
'
}

lock_version() {
  awk '/^name = "pscat"$/ {found=1; next} found && /^version = / {gsub(/"/, "", $3); print $3; exit}' Cargo.lock
}

old_tag="$(sed -n 's/^pscat = { git = "[^"]*", tag = "\([^"]*\)".*/\1/p' Cargo.toml)"
[[ -n "$old_tag" ]] || { echo "Cargo.toml: no 'pscat = { git = ..., tag = ... }' line" >&2; exit 1; }
old_version="$(lock_version)"

tags="$(git ls-remote --tags --refs "$REPO_URL" 'v*' | awk '{sub("refs/tags/", "", $2); print $2}')"
if [[ "$target" == "--latest" ]]; then
  new_tag="$(printf '%s\n' "$tags" | semver_max)"
  [[ -n "$new_tag" ]] || { echo "no v* tags found in $REPO_URL" >&2; exit 1; }
else
  new_tag="$target"
  printf '%s\n' "$tags" | grep -qxF "$new_tag" || { echo "tag $new_tag does not exist in $REPO_URL" >&2; exit 1; }
fi

if [[ "$new_tag" != "$old_tag" && $allow_downgrade -eq 0 ]]; then
  newest="$(printf '%s\n%s\n' "$old_tag" "$new_tag" | semver_max)"
  if [[ "$newest" == "$old_tag" ]]; then
    echo "refusing to move PSCat from $old_tag down to $new_tag (pass --allow-downgrade)" >&2
    exit 1
  fi
fi

before="$(cat Cargo.toml Cargo.lock | git hash-object --stdin)"
if [[ "$new_tag" == "$old_tag" ]]; then
  echo "PSCat is already pinned to $old_tag ($old_version)."
else
  sed -i.bak "s|^\(pscat = { git = \"[^\"]*\", tag = \"\)[^\"]*\"|\1$new_tag\"|" Cargo.toml
  rm -f Cargo.toml.bak
  # Re-resolve only pscat. If the new PSCat needs newer versions of shared
  # dependencies, fall back to also updating pscat's own dependency tree.
  if ! cargo update -p pscat; then
    echo "cargo update -p pscat failed; retrying with --recursive" >&2
    cargo update -p pscat --recursive
  fi
fi
new_version="$(lock_version)"

if [[ -n "$release_version" ]]; then
  [[ "$release_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] \
    || { echo "--release-version must be semver (e.g. 0.2.0), got $release_version" >&2; exit 1; }
  # The first top-level `version = ` line is [package]'s.
  python3 -I -c '
import re, sys
p, v = "Cargo.toml", sys.argv[1]
s = open(p).read()
open(p, "w").write(re.sub(r"(?m)^version = \".*\"", "version = \"%s\"" % v, s, count=1))
' "$release_version"
  grep -q "^version = \"$release_version\"" Cargo.toml || { echo "could not set the package version" >&2; exit 1; }
  cargo metadata --format-version 1 >/dev/null   # refresh the root entry in Cargo.lock
fi

# "changed" means the files changed, whatever options were given.
after="$(cat Cargo.toml Cargo.lock | git hash-object --stdin)"
changed=false
[[ "$before" == "$after" ]] || changed=true

echo "PSCat: $old_tag ($old_version) -> $new_tag ($new_version)"
[[ -z "$release_version" ]] || echo "pscat-mcp-server version: $release_version"

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  {
    echo "changed=$changed"
    echo "old_tag=$old_tag"
    echo "new_tag=$new_tag"
    echo "old_version=$old_version"
    echo "new_version=$new_version"
  } >> "$GITHUB_OUTPUT"
fi

if [[ $run_tests -eq 1 ]]; then
  cargo test --all --locked
fi
