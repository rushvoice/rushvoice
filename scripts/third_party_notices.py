#!/usr/bin/env python3
"""Generate THIRD_PARTY_NOTICES.txt for RushVoice release builds.

Collects every Rust crate (from `cargo metadata`) and every production npm
package (from `pnpm licenses list --prod --json`) and copies the license /
notice files that ship inside each package.

Usage (from the repository root):
    python scripts/third_party_notices.py [--out THIRD_PARTY_NOTICES.txt]
"""
from __future__ import annotations

import argparse
import json
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
LICENSE_PREFIXES = ("license", "licence", "copying", "notice", "unlicense")


def run(cmd: list[str], cwd: pathlib.Path) -> str:
    return subprocess.run(
        cmd, cwd=cwd, check=True, capture_output=True, text=True, encoding="utf-8",
        shell=(sys.platform == "win32" and cmd[0] == "pnpm"),
    ).stdout


MISSING: list[str] = []
NEEDED_SPDX: set[str] = set()


def license_texts(pkg_dir: pathlib.Path, extra: pathlib.Path | None = None) -> list[tuple[str, str]]:
    texts = []
    if extra is not None and extra.is_file():
        texts.append((extra.name, extra.read_text(encoding="utf-8", errors="replace").strip()))
    if not pkg_dir.is_dir():
        return texts
    for f in sorted(pkg_dir.iterdir()):
        if extra is not None and f.resolve() == extra.resolve():
            continue
        if f.is_file() and f.name.lower().startswith(LICENSE_PREFIXES):
            try:
                texts.append((f.name, f.read_text(encoding="utf-8", errors="replace").strip()))
            except OSError:
                pass
    return texts


# Targets RushVoice is actually built and shipped for.
SHIP_TARGETS = ("x86_64-pc-windows-msvc", "aarch64-apple-darwin", "x86_64-apple-darwin")


def shipped_ids(meta: dict) -> set[str]:
    """Package ids reachable from the workspace through normal/build deps (no dev deps)."""
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    seen: set[str] = set()
    stack = list(meta["workspace_members"])
    while stack:
        pid = stack.pop()
        if pid in seen or pid not in nodes:
            continue
        seen.add(pid)
        for dep in nodes[pid].get("deps", []):
            kinds = [k.get("kind") for k in dep.get("dep_kinds", [])]
            if any(k in (None, "build") for k in kinds):
                stack.append(dep["pkg"])
    return seen


def rust_packages() -> list[dict]:
    pkgs: dict[str, dict] = {}
    ids: set[str] = set()
    workspace: set[str] = set()
    for target in SHIP_TARGETS:
        meta = json.loads(run(["cargo", "metadata", "--format-version", "1", "--locked",
                               "--filter-platform", target], ROOT / "src-tauri"))
        workspace |= set(meta["workspace_members"])
        ids |= shipped_ids(meta)
        for p in meta["packages"]:
            pkgs[p["id"]] = p
    out = []
    for pid in ids - workspace:
        p = pkgs[pid]
        out.append({
            "name": p["name"], "version": p["version"],
            "license": p.get("license") or "(see license file)",
            "authors": [a for a in p.get("authors", []) if a],
            "url": p.get("repository") or p.get("homepage") or "",
            "dir": pathlib.Path(p["manifest_path"]).parent,
            "license_file": (pathlib.Path(p["manifest_path"]).parent / p["license_file"])
            if p.get("license_file") else None,
        })
    return sorted(out, key=lambda x: (x["name"], x["version"]))


SPDX_TEXT_URL = "https://raw.githubusercontent.com/spdx/license-list-data/main/text/{}.txt"


def spdx_ids(expr: str) -> list[str]:
    import re
    expr = expr.replace("/", " OR ")
    return [t for t in re.findall(r"[A-Za-z0-9.\-+]+", expr) if t not in ("OR", "AND", "WITH")]


def spdx_texts(ids: set[str]) -> list[str]:
    import urllib.request
    lines: list[str] = []
    for lid in sorted(ids):
        url = SPDX_TEXT_URL.format(lid)
        try:
            with urllib.request.urlopen(url, timeout=30) as r:
                text = r.read().decode("utf-8", errors="replace").strip()
        except Exception as e:  # noqa: BLE001
            raise SystemExit(f"could not fetch the standard text of {lid}: {e}")
        lines += ["-" * 78, f"Standard license text: {lid}", "-" * 78, text, ""]
    return lines


def npm_packages() -> list[dict]:
    data = json.loads(run(["pnpm", "licenses", "list", "--prod", "--json"], ROOT / "frontend"))
    out = []
    for license_id, pkgs in data.items():
        for p in pkgs:
            versions = p.get("versions") or [p.get("version", "")]
            paths = p.get("paths") or [p.get("path", "")]
            for v, path in zip(versions, paths):
                out.append({
                    "name": p["name"], "version": v, "license": license_id,
                    "url": p.get("homepage") or "", "dir": pathlib.Path(path) if path else pathlib.Path(),
                })
    return sorted(out, key=lambda x: (x["name"], x["version"]))


def render(section: str, pkgs: list[dict]) -> list[str]:
    lines = [f"==== {section} ({len(pkgs)} packages) ====", ""]
    for p in pkgs:
        lines.append("-" * 78)
        lines.append(f"{p['name']} {p['version']}  [{p['license']}]")
        if p["url"]:
            lines.append(p["url"])
        texts = license_texts(p["dir"], p.get("license_file"))
        if not texts:
            if p["license"].startswith("("):
                raise SystemExit(f"{p['name']} {p['version']}: no license id and no license file")
            MISSING.append(f"{p['name']} {p['version']} [{p['license']}]")
            NEEDED_SPDX.update(spdx_ids(p["license"]))
            holders = ", ".join(p.get("authors") or []) or f"the {p['name']} authors"
            lines.append(f"Copyright (c) {holders}")
            lines.append(f"Licensed under {p['license']}. The package does not bundle a license file; "
                         "the standard text of each license is reproduced in the appendix at the end.")
        for name, text in texts:
            lines.append(f"--- {name} ---")
            lines.append(text)
        lines.append("")
    return lines


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default="THIRD_PARTY_NOTICES.txt")
    args = ap.parse_args()
    lines = [
        "RushVoice - third-party software notices",
        "RushVoice is distributed under the Apache License 2.0 (see LICENSE).",
        "It includes the following third-party components, each under its own license.",
        "",
    ]
    lines += render("Rust crates", rust_packages())
    lines += render("JavaScript packages (frontend)", npm_packages())
    if MISSING:
        lines += ["==== Appendix: standard license texts ====",
                  "Used for the following packages, which do not bundle a license file:", ""]
        lines += MISSING + [""]
        lines += spdx_texts(NEEDED_SPDX)
    pathlib.Path(args.out).write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {args.out} (packages without a license file: {len(MISSING)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
