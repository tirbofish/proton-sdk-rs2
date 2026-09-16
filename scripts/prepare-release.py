#!/usr/bin/env python3
"""Bump versions and prepend a changelog for a pdcli/SDK release."""

from __future__ import annotations

import argparse
import datetime as dt
import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]


def run(args: list[str]) -> str:
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def current_version() -> str:
    match = re.search(
        r'^package\.version\s*=\s*"([^"]+)"',
        (ROOT / "Cargo.toml").read_text(),
        re.M,
    )
    if not match:
        raise SystemExit("could not find package.version in Cargo.toml")
    return match.group(1)


def bump_version(version: str, kind: str) -> str:
    major, minor, patch = (int(part) for part in version.split("."))
    if kind == "major":
        return f"{major + 1}.0.0"
    if kind == "minor":
        return f"{major}.{minor + 1}.0"
    if kind == "patch":
        return f"{major}.{minor}.{patch + 1}"
    raise SystemExit(f"unknown bump kind: {kind}")


def crate_req(version: str) -> str:
    major, minor, _patch = version.split(".")
    if major == "0":
        return f"{major}.{minor}"
    return major


def replace_file(path: pathlib.Path, pattern: str, repl: str, count: int = 1) -> None:
    text = path.read_text()
    updated, n = re.subn(pattern, repl, text, count=count, flags=re.M)
    if n == 0:
        raise SystemExit(f"no match for {pattern!r} in {path}")
    path.write_text(updated)


def previous_tag() -> str | None:
    try:
        return run(["git", "describe", "--tags", "--abbrev=0"])
    except subprocess.CalledProcessError:
        return None


def changelog_entries(since: str | None) -> list[str]:
    args = ["git", "log", "--pretty=format:%s"]
    if since:
        args.append(f"{since}..HEAD")
    subjects = [line for line in run(args).splitlines() if line.strip()]
    skip_prefixes = ("release:", "Merge ", "chore: release")
    entries = []
    for subject in subjects:
        if subject.startswith(skip_prefixes):
            continue
        entries.append(f"- {subject}")
    return entries or ["- Packaging and release tooling updates."]


def rfc2822_now() -> str:
    now = dt.datetime.now(dt.timezone.utc)
    return now.strftime("%a, %d %b %Y %H:%M:%S +0000")


def rpm_date() -> str:
    now = dt.datetime.now(dt.timezone.utc)
    return now.strftime("%a %b %d %Y")


def iso_date() -> str:
    return dt.date.today().isoformat()


def update_files(old: str, new: str, notes: list[str]) -> None:
    old_req = crate_req(old)
    new_req = crate_req(new)

    replace_file(ROOT / "Cargo.toml", rf'^package\.version = "{re.escape(old)}"', f'package.version = "{new}"')
    replace_file(
        ROOT / "Cargo.toml",
        rf'^proton-sdk-rs2 = \{{ version = "{re.escape(old_req)}", path = "crates/proton-sdk-rs2" \}}',
        f'proton-sdk-rs2 = {{ version = "{new_req}", path = "crates/proton-sdk-rs2" }}',
    )
    replace_file(
        ROOT / "crates/pdcli/Cargo.toml",
        rf'^proton-drive-sdk = \{{ version = "{re.escape(old_req)}", path = "../proton-drive-sdk"',
        f'proton-drive-sdk = {{ version = "{new_req}", path = "../proton-drive-sdk"',
    )

    debian = ROOT / "debian/changelog"
    debian.write_text(
        f"pdcli ({new}-1) unstable; urgency=medium\n\n"
        f"  * Release {new}.\n\n"
        f" -- pdcli contributors <4tkbytes@pm.me>  {rfc2822_now()}\n\n"
        + debian.read_text()
    )

    spec = ROOT / "packaging/rpm/pdcli.spec"
    spec_text = spec.read_text()
    spec_text = re.sub(r"^Version:\s+.*$", f"Version:        {new}", spec_text, count=1, flags=re.M)
    spec_text = spec_text.replace(
        "%changelog\n",
        "%changelog\n"
        f"* {rpm_date()} pdcli contributors <4tkbytes@pm.me> - {new}-1\n"
        f"- Release {new}\n",
        1,
    )
    spec.write_text(spec_text)

    replace_file(ROOT / "packaging/arch/PKGBUILD", r"^pkgver=.*$", f"pkgver={new}")

    release_doc = ROOT / "docs/RELEASE.md"
    release_text = release_doc.read_text()
    release_text = re.sub(r"^# pdcli .*$", f"# pdcli {new}", release_text, count=1, flags=re.M)
    release_doc.write_text(release_text)

    changelog = ROOT / "CHANGELOG.md"
    section = f"## {new} - {iso_date()}\n\n" + "\n".join(notes) + "\n\n"
    if changelog.exists():
        body = changelog.read_text()
        if not body.startswith("# Changelog"):
            body = "# Changelog\n\n" + body
        parts = body.split("\n", 2)
        rest = parts[2] if len(parts) == 3 else ""
        changelog.write_text("# Changelog\n\n" + section + rest.lstrip("\n"))
    else:
        changelog.write_text("# Changelog\n\n" + section)

    notes_path = ROOT / "dist/release-notes.md"
    notes_path.parent.mkdir(parents=True, exist_ok=True)
    notes_path.write_text(f"# pdcli {new}\n\n" + "\n".join(notes) + "\n")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bump", choices=("patch", "minor", "major", "none"), default="patch")
    args = parser.parse_args()

    old = current_version()
    new = old if args.bump == "none" else bump_version(old, args.bump)
    notes = changelog_entries(previous_tag())
    if args.bump != "none":
        update_files(old, new, notes)
    else:
        notes_path = ROOT / "dist/release-notes.md"
        notes_path.parent.mkdir(parents=True, exist_ok=True)
        notes_path.write_text(f"# pdcli {new}\n\n" + "\n".join(notes) + "\n")

    output = f"version={new}\ntag=v{new}\n"
    print(output, end="")
    if github_output := os.environ.get("GITHUB_OUTPUT"):
        with open(github_output, "a", encoding="utf-8") as handle:
            handle.write(output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
