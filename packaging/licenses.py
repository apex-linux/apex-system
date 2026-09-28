#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Compute the RPM License: expression for the apex package from `cargo metadata`.

The apex RPM ships statically linked Rust binaries, so its License tag is Apex's own
license AND the license of every crate linked into them. This script derives that
expression from the dependency graph instead of by hand:

1. `cargo metadata --format-version 1` (no build; it resolves Cargo.lock, with
   `--locked` when the lock file exists), filtered to the platform the RPM is built
   for (default x86_64-unknown-linux-gnu; `--target all` reproduces the more
   conservative view of Fedora's `%cargo_license_summary`).
2. Start from the workspace packages that own the binaries (apexctl, apex-updated,
   apex-boot-assess) and follow only normal dependency edges. Build and dev
   dependencies and proc-macro crates never end up in the binaries, so they are
   skipped. That matches `cargo tree --edges no-build,no-dev,no-proc-macro`,
   the edge set cargo2rpm uses for Fedora's license macros.
3. Normalise each crate's license (legacy `MIT/Apache-2.0` becomes
   `Apache-2.0 OR MIT`; flat OR lists are sorted so equivalent spellings collapse),
   split top-level ANDs into their parts, de-duplicate, and join with AND: simple
   identifiers first, OR expressions in parentheses after them.

Usage (from the repository root; network may be needed the first time cargo resolves):

    python3 packaging/licenses.py                 # breakdown plus the License: line
    python3 packaging/licenses.py --expression    # only the expression
    python3 packaging/licenses.py --check packaging/apex.spec   # CI: tag up to date?
    python3 packaging/licenses.py --self-test     # run the doctests

The metadata resolve is feature-unified across the whole workspace, so a crate that is
enabled only for a sibling member can appear here; the error is on the safe side.
"""

from __future__ import annotations

import argparse
import doctest
import json
import re
import subprocess
import sys
from collections import deque
from pathlib import Path

APEX_LICENSE = "GPL-3.0-or-later"
DEFAULT_BINS = ("apexctl", "apex-updated", "apex-boot-assess")
DEFAULT_TARGET = "x86_64-unknown-linux-gnu"
REPO_ROOT = Path(__file__).resolve().parent.parent


class LicenseError(Exception):
    """A problem that makes the computed expression unusable."""


def normalize(expr: str) -> str:
    """Normalise one crate license expression.

    >>> normalize("MIT/Apache-2.0")
    'Apache-2.0 OR MIT'
    >>> normalize("MIT / Apache-2.0")
    'Apache-2.0 OR MIT'
    >>> normalize("Apache-2.0 OR MIT")
    'Apache-2.0 OR MIT'
    >>> normalize("MIT OR Apache-2.0 WITH LLVM-exception OR Apache-2.0")
    'Apache-2.0 OR Apache-2.0 WITH LLVM-exception OR MIT'
    >>> normalize("(MIT OR Apache-2.0) AND Unicode-3.0")
    '(MIT OR Apache-2.0) AND Unicode-3.0'
    >>> normalize("  MIT  ")
    'MIT'
    """
    text = " ".join(expr.replace("/", " OR ").split())
    # Only a flat OR list is reordered: with AND or parentheses, order carries meaning
    # for readers and the expression is kept as written.
    if "(" in text or " AND " in text:
        return text
    operands = sorted(set(text.split(" OR ")))
    return " OR ".join(operands)


def conjuncts(expr: str) -> list[str]:
    """Split a normalised expression at its top-level ANDs (A AND (B AND C) == A AND B AND C).

    Each part loses redundant outer parentheses and is normalised again, so
    `(MIT OR Apache-2.0) AND Unicode-3.0` contributes `Apache-2.0 OR MIT` and
    `Unicode-3.0` to the package expression.

    >>> conjuncts("(MIT OR Apache-2.0) AND Unicode-3.0")
    ['Apache-2.0 OR MIT', 'Unicode-3.0']
    >>> conjuncts("Apache-2.0 OR MIT")
    ['Apache-2.0 OR MIT']
    >>> conjuncts("((MIT) AND (BSD-3-Clause AND ISC))")
    ['MIT', 'BSD-3-Clause', 'ISC']
    """
    parts, depth, start, i = [], 0, 0, 0
    while i < len(expr):
        char = expr[i]
        if char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
        elif depth == 0 and expr.startswith(" AND ", i):
            parts.append(expr[start:i])
            start = i + len(" AND ")
            i = start
            continue
        i += 1
    parts.append(expr[start:])
    if len(parts) == 1:
        term = parts[0].strip()
        if term.startswith("(") and _closing_paren(term) == len(term) - 1:
            return conjuncts(term[1:-1])
        return [normalize(term)]
    return [c for part in parts for c in conjuncts(part.strip())]


def _closing_paren(text: str) -> int:
    """Index of the parenthesis that closes the one at index 0."""
    depth = 0
    for i, char in enumerate(text):
        depth += {"(": 1, ")": -1}.get(char, 0)
        if depth == 0:
            return i
    raise LicenseError(f"unbalanced parentheses in {text!r}")


def is_compound(expr: str) -> bool:
    """Whether an expression needs parentheses inside an AND list.

    >>> is_compound("MIT")
    False
    >>> is_compound("Apache-2.0 WITH LLVM-exception")
    False
    >>> is_compound("Apache-2.0 OR MIT")
    True
    """
    return " OR " in expr or " AND " in expr


def combine(expressions: set[str]) -> str:
    """Join normalised expressions with AND: simple ones first, then compound ones.

    >>> combine({"MIT", "Apache-2.0 OR MIT", "GPL-3.0-or-later", "Unicode-3.0"})
    'GPL-3.0-or-later AND MIT AND Unicode-3.0 AND (Apache-2.0 OR MIT)'
    >>> combine({"MIT", "(MIT OR Apache-2.0) AND Unicode-3.0"})
    'MIT AND Unicode-3.0 AND (Apache-2.0 OR MIT)'
    """
    flat = {c for e in expressions for c in conjuncts(e)}
    ordered = sorted(flat, key=lambda e: (is_compound(e), e))
    return " AND ".join(f"({e})" if is_compound(e) else e for e in ordered)


def is_proc_macro(package: dict) -> bool:
    """Proc-macro crates run inside rustc and are never linked into a binary."""
    return any("proc-macro" in target["kind"] for target in package["targets"])


def closure(metadata: dict, bins: tuple[str, ...]) -> list[dict]:
    """Packages linked into the given binaries (roots included), sorted by name.

    A breadth-first walk over normal edges of the resolve graph. A dict keyed by
    package id is the visited set, so every package is expanded once even though
    the graph is a DAG with heavy sharing.
    """
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    members = set(metadata["workspace_members"])

    roots = []
    for name in bins:
        owners = [
            pid
            for pid in members
            if any(t["name"] == name and "bin" in t["kind"] for t in packages[pid]["targets"])
        ]
        if len(owners) != 1:
            raise LicenseError(f"binary {name!r} is owned by {len(owners)} workspace packages")
        roots.append(owners[0])

    seen: dict[str, dict] = {}
    queue = deque(roots)
    while queue:
        pid = queue.popleft()
        if pid in seen:
            continue
        seen[pid] = packages[pid]
        for dep in nodes[pid]["deps"]:
            # kind None is a normal dependency; "build" and "dev" never reach the binary.
            if not any(k["kind"] is None for k in dep["dep_kinds"]):
                continue
            if is_proc_macro(packages[dep["pkg"]]):
                continue
            queue.append(dep["pkg"])
    return sorted(seen.values(), key=lambda p: (p["name"], p["version"]))


def breakdown(linked: list[dict]) -> dict[str, list[str]]:
    """Map each normalised license expression to the crates that carry it."""
    result: dict[str, list[str]] = {}
    missing = []
    for package in linked:
        license_expr = package.get("license")
        if not license_expr:
            missing.append(f"{package['name']} {package['version']}")
            continue
        result.setdefault(normalize(license_expr), []).append(
            f"{package['name']} {package['version']}"
        )
    if missing:
        raise LicenseError(
            "no SPDX license field (license-file only; review by hand): " + ", ".join(missing)
        )
    return result


def run_cargo_metadata(manifest: Path, target: str, cargo: str) -> dict:
    """Run `cargo metadata` without building anything."""
    cmd = [cargo, "metadata", "--format-version", "1", "--manifest-path", str(manifest)]
    if target != "all":
        cmd += ["--filter-platform", target]
    if (manifest.parent / "Cargo.lock").exists():
        cmd.append("--locked")
    proc = subprocess.run(cmd, capture_output=True, text=True, check=False)  # noqa: S603
    if proc.returncode != 0:
        raise LicenseError(f"{' '.join(cmd)} failed:\n{proc.stderr.strip()}")
    return json.loads(proc.stdout)


def spec_license(spec: Path) -> str:
    """The value of the License: tag in an RPM spec."""
    for line in spec.read_text(encoding="utf-8").splitlines():
        match = re.match(r"License:\s*(.+?)\s*$", line)
        if match:
            return match.group(1)
    raise LicenseError(f"{spec} has no License: tag")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--manifest-path", type=Path, default=REPO_ROOT / "Cargo.toml")
    parser.add_argument("--target", default=DEFAULT_TARGET, help="target triple, or 'all'")
    parser.add_argument("--bin", action="append", dest="bins", help="binary (repeatable)")
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--expression", action="store_true", help="print only the expression")
    parser.add_argument("--check", type=Path, metavar="SPEC", help="compare with SPEC's tag")
    parser.add_argument("--self-test", action="store_true", help="run the doctests")
    args = parser.parse_args(argv)

    if args.self_test:
        failed, _ = doctest.testmod(verbose=False)
        print("doctests: " + ("FAILED" if failed else "ok"))
        return 1 if failed else 0

    bins = tuple(args.bins) if args.bins else DEFAULT_BINS
    try:
        metadata = run_cargo_metadata(args.manifest_path, args.target, args.cargo)
        licenses = breakdown(closure(metadata, bins))
        expression = combine({APEX_LICENSE, *licenses})
        if args.check:
            current = spec_license(args.check)
            if current != expression:
                print(f"{args.check}: License tag is out of date.", file=sys.stderr)
                print(f"  spec:     {current}", file=sys.stderr)
                print(f"  computed: {expression}", file=sys.stderr)
                return 1
            print(f"{args.check}: License tag matches the dependency tree.")
            return 0
    except LicenseError as err:
        print(f"licenses.py: {err}", file=sys.stderr)
        return 1

    if args.expression:
        print(expression)
        return 0
    print(f"# Binaries: {', '.join(bins)}; target: {args.target}")
    for expr in sorted(licenses, key=lambda e: (is_compound(e), e)):
        crates = licenses[expr]
        print(f"# {expr}: {len(crates)} crate(s): {', '.join(crates)}")
    print(f"License:        {expression}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
