#!/usr/bin/env python3
"""Bump the Switchyard version (the workspace version every crate inherits).

    scripts/bump-version.py patch        # 0.1.0 -> 0.1.1
    scripts/bump-version.py minor        # 0.1.1 -> 0.2.0
    scripts/bump-version.py major        # 0.2.0 -> 1.0.0
    scripts/bump-version.py 1.4.2        # set an exact version
    scripts/bump-version.py --current    # print the current version

Updates `[workspace.package] version` in Cargo.toml and the workspace crates' entries in
Cargo.lock (not path dependencies outside the workspace, such as vendor/tiberius), then
checks the lockfile with `cargo metadata --locked`. Prints the new version on the last line.
"""

import argparse
import glob
import os
import re
import subprocess
import sys
import tomllib

SEMVER = re.compile(r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$")


def bumped(current: str, part: str) -> str:
    m = SEMVER.match(current)
    if not m:
        raise SystemExit(f"current version {current!r} is not MAJOR.MINOR.PATCH")
    major, minor, patch = (int(x) for x in m.groups())
    if part == "major":
        return f"{major + 1}.0.0"
    if part == "minor":
        return f"{major}.{minor + 1}.0"
    if part == "patch":
        return f"{major}.{minor}.{patch + 1}"
    if not SEMVER.match(part):
        raise SystemExit(f"{part!r} is not major, minor, patch or MAJOR.MINOR.PATCH")
    return part


def workspace_crates(root: str, manifest: dict) -> set[str]:
    names = set()
    for pattern in manifest["workspace"]["members"]:
        for path in glob.glob(os.path.join(root, pattern, "Cargo.toml")):
            with open(path, "rb") as f:
                names.add(tomllib.load(f)["package"]["name"])
    return names


def set_manifest_version(text: str, new: str) -> str:
    """Replace `version = "…"` inside the `[workspace.package]` table only."""
    start = text.index("[workspace.package]")
    end = text.find("\n[", start + 1)
    end = len(text) if end == -1 else end
    section, n = re.subn(
        r'(?m)^version\s*=\s*"[^"]*"', f'version = "{new}"', text[start:end], count=1
    )
    if n != 1:
        raise SystemExit("no version in [workspace.package]")
    return text[:start] + section + text[end:]


def set_lock_versions(text: str, crates: set[str], new: str) -> tuple[str, int]:
    """Set the version of each workspace crate's `[[package]]` entry (they have no source)."""
    blocks = text.split("\n[[package]]\n")
    changed = 0
    for i, block in enumerate(blocks[1:], start=1):
        name = re.search(r'(?m)^name = "([^"]+)"', block)
        if name and name.group(1) in crates and "\nsource = " not in block:
            blocks[i], n = re.subn(
                r'(?m)^version = "[^"]*"', f'version = "{new}"', block, count=1
            )
            changed += n
    return "\n[[package]]\n".join(blocks), changed


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("bump", nargs="?", help="major, minor, patch or MAJOR.MINOR.PATCH")
    parser.add_argument("--current", action="store_true", help="print the current version")
    parser.add_argument("--dry-run", action="store_true", help="show the change, write nothing")
    parser.add_argument("--no-verify", action="store_true", help="skip `cargo metadata --locked`")
    parser.add_argument(
        "--root", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..")
    )
    args = parser.parse_args()

    root = os.path.abspath(args.root)
    manifest_path = os.path.join(root, "Cargo.toml")
    lock_path = os.path.join(root, "Cargo.lock")
    with open(manifest_path, "rb") as f:
        manifest = tomllib.load(f)
    current = manifest["workspace"]["package"]["version"]
    if args.current:
        print(current)
        return
    if not args.bump:
        parser.error("say major, minor, patch or a version")

    new = bumped(current, args.bump)
    if new == current:
        raise SystemExit(f"already at {current}")
    crates = workspace_crates(root, manifest)
    with open(manifest_path, encoding="utf-8") as f:
        manifest_text = set_manifest_version(f.read(), new)
    with open(lock_path, encoding="utf-8") as f:
        lock_text, changed = set_lock_versions(f.read(), crates, new)
    if changed != len(crates):
        raise SystemExit(
            f"Cargo.lock has {changed} of {len(crates)} workspace crates; run cargo build first"
        )

    print(f"{current} -> {new} ({len(crates)} crates)", file=sys.stderr)
    if args.dry_run:
        print(new)
        return
    with open(manifest_path, "w", encoding="utf-8", newline="\n") as f:
        f.write(manifest_text)
    with open(lock_path, "w", encoding="utf-8", newline="\n") as f:
        f.write(lock_text)
    if not args.no_verify:
        # Fails if the lockfile no longer matches the manifests.
        subprocess.run(
            ["cargo", "metadata", "--locked", "--format-version", "1"],
            cwd=root,
            check=True,
            stdout=subprocess.DEVNULL,
        )
    print(new)


if __name__ == "__main__":
    main()
