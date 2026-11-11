#!/usr/bin/env python3
"""Copy the PSCat plugin's skills from the pinned PSCat revision into guide/.

    python3 -I scripts/vendor-guide.py           # refresh guide/ from the pin
    python3 -I scripts/vendor-guide.py --check   # exit 1 if guide/ differs

The server serves these files as MCP resources and builds its prompts from
them (src/guide.rs). They are PSCat's text, copied byte for byte from
`plugin/skills/` of the commit Cargo.lock pins; nothing here edits them.
`scripts/bump-pscat.sh` runs this after it moves the pin, and
`tests/guide.rs` fails when guide/ and the pin disagree.

The checkout is found with `cargo metadata`, so it is the same one the
build compiles. Every copied file must be MIT OR Apache-2.0 according to
PSCat's own licence inventory (`licenses/assets.json`, first matching entry
wins); the script refuses to vendor anything else.

Writes:
  guide/skills/...        the files of plugin/skills/, unchanged
  guide/LICENSE-MIT       PSCat's licence files, unchanged
  guide/LICENSE-APACHE
  guide/SOURCE.json       tag, version, commit, path, licence, file list

Needs: cargo, python3 (standard library only).
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GUIDE = ROOT / "guide"
SKILLS_PATH = "plugin/skills"
LICENSE_FILES = ("LICENSE-MIT", "LICENSE-APACHE")
EXPECTED_LICENSE = "MIT OR Apache-2.0"


def pscat_package() -> dict:
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    meta = json.loads(out)
    for pkg in meta["packages"]:
        if pkg["name"] == "pscat" and (pkg.get("source") or "").startswith("git+"):
            return pkg
    sys.exit("cargo metadata: no git source for the pscat package")


def cargo_toml_tag() -> str:
    text = (ROOT / "Cargo.toml").read_text()
    m = re.search(r'^pscat = \{ git = "[^"]*", tag = "([^"]*)"', text, re.M)
    if not m:
        sys.exit("Cargo.toml: no 'pscat = { git = ..., tag = ... }' line")
    return m.group(1)


def glob_regex(pattern: str) -> re.Pattern:
    """licenses/assets.json path patterns: `**` any depth, `*` and `?` within
    one path segment."""
    out = []
    i = 0
    while i < len(pattern):
        c = pattern[i]
        if pattern.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
            continue
        if pattern.startswith("**", i):
            out.append(".*")
            i += 2
            continue
        out.append({"*": "[^/]*", "?": "[^/]"}.get(c, re.escape(c)))
        i += 1
    return re.compile("".join(out) + r"\Z")


def licence_of(entries: list[dict], path: str) -> dict:
    for entry in entries:
        if any(glob_regex(p).match(path) for p in entry.get("paths", [])):
            return entry
    sys.exit(f"licenses/assets.json: no entry matches {path}")


def collect(checkout: Path) -> dict[str, bytes]:
    base = checkout / SKILLS_PATH
    if not base.is_dir():
        sys.exit(f"{base}: not a directory (did PSCat move its skills?)")
    files: dict[str, bytes] = {}
    for p in sorted(base.rglob("*")):
        if p.is_symlink():
            sys.exit(f"{p}: a symlink; refusing to follow it")
        if p.is_file():
            files[p.relative_to(base).as_posix()] = p.read_bytes()
    if not files:
        sys.exit(f"{base}: no files")
    return files


def build(checkout: Path, pkg: dict) -> dict[str, bytes]:
    """Every file guide/ should hold, relative to guide/."""
    inventory = json.loads((checkout / "licenses" / "assets.json").read_text())["entries"]
    skills = collect(checkout)
    copyright_holders = set()
    for rel in skills:
        entry = licence_of(inventory, f"{SKILLS_PATH}/{rel}")
        if entry["license"] != EXPECTED_LICENSE:
            sys.exit(f"{SKILLS_PATH}/{rel} is {entry['license']}, not {EXPECTED_LICENSE}; not vendoring it")
        copyright_holders.add(entry.get("copyright", ""))
    source = pkg["source"]
    commit = source.rsplit("#", 1)[-1]
    record = {
        "repository": "https://github.com/PSCatLabs/pscat",
        "tag": cargo_toml_tag(),
        "version": pkg["version"],
        "commit": commit,
        "path": SKILLS_PATH,
        "license": EXPECTED_LICENSE,
        "copyright": sorted(copyright_holders),
        "license_files": list(LICENSE_FILES),
        "note": "Copied unchanged by scripts/vendor-guide.py from the PSCat commit above. "
        "Do not edit these files here; refresh them with that script.",
        "files": sorted(skills),
    }
    out = {f"skills/{rel}": data for rel, data in skills.items()}
    for name in LICENSE_FILES:
        out[name] = (checkout / name).read_bytes()
    out["SOURCE.json"] = (json.dumps(record, indent=2) + "\n").encode()
    return out


def current() -> dict[str, bytes]:
    if not GUIDE.is_dir():
        return {}
    return {
        p.relative_to(GUIDE).as_posix(): p.read_bytes()
        for p in sorted(GUIDE.rglob("*"))
        if p.is_file()
    }


def main(argv: list[str]) -> int:
    check = "--check" in argv
    if any(a not in ("--check",) for a in argv):
        print("usage: python3 -I scripts/vendor-guide.py [--check]", file=sys.stderr)
        return 2
    pkg = pscat_package()
    checkout = Path(pkg["manifest_path"]).parent
    want = build(checkout, pkg)
    have = current()
    if want == have:
        print(f"guide/ matches PSCat {json.loads(want['SOURCE.json'])['tag']}")
        return 0
    changed = sorted(k for k in set(want) | set(have) if want.get(k) != have.get(k))
    if check:
        print("guide/ differs from the pinned PSCat revision:", *changed, sep="\n  ", file=sys.stderr)
        print("run: python3 -I scripts/vendor-guide.py", file=sys.stderr)
        return 1
    # Write into a fresh directory, then swap, so a failure leaves guide/ as it was.
    tmp = Path(tempfile.mkdtemp(prefix=".guide-", dir=ROOT))
    tmp.chmod(0o755)
    try:
        for rel, data in want.items():
            dest = tmp / rel
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.write_bytes(data)
        if GUIDE.exists():
            shutil.rmtree(GUIDE)
        os.replace(tmp, GUIDE)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    print("guide/ refreshed:", *changed, sep="\n  ")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
