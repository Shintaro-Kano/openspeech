#!/usr/bin/env python3
"""License audit for OpenSpeech (Rust crates + bundled speech.cpp).

Reads `cargo metadata --format-version 1 --locked` for aarch64-apple-darwin. Fails (exit 1) when a package's license expression
cannot be satisfied without a GPL / AGPL / LGPL / SSPL term, when a license is missing or an
identifier is not known, unless the package is listed in ALLOW with a reason.
Also checks that vendor/speech-cpp/LICENSE and LICENSE-ggml exist.

With --write FILE it also writes the license texts of every crate in the public build's graph (and
of the fonts egui embeds) to FILE; with --check FILE it fails when FILE differs from what --write
would write (CI runs this, so THIRD-PARTY-LICENSES.txt follows Cargo.lock). The texts are read from
the crates' own files in cargo's registry; a crate published without a license file gets the
standard text of the MIT license (every such crate offers MIT) with its authors.

Usage: python3 -I scripts/check-licenses.py [--write FILE | --check FILE] [repo root]
(standard library only)
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

ARGS = argparse.ArgumentParser(description="License audit for OpenSpeech")
MODE = ARGS.add_mutually_exclusive_group()
MODE.add_argument("--write", metavar="FILE", help="write the third-party license texts to FILE")
MODE.add_argument("--check", metavar="FILE", help="fail when FILE is not what --write would write")
ARGS.add_argument("root", nargs="?", help="repository root (default: the folder above scripts/)")
OPTIONS = ARGS.parse_args() if __name__ == "__main__" else ARGS.parse_args([])
ROOT = Path(OPTIONS.root).resolve() if OPTIONS.root else Path(__file__).resolve().parent.parent
CARGO = Path.home() / ".cargo" / "bin" / "cargo"

FORBIDDEN_TOKENS = ("GPL", "AGPL", "LGPL", "SSPL", "GENERAL PUBLIC LICENSE")

# Identifiers that have been reviewed. Anything else needs review (add here or in ALLOW).
KNOWN = {
    "MIT", "APACHE-2.0", "BSD-2-CLAUSE", "BSD-3-CLAUSE", "ISC", "ZLIB", "UNICODE-3.0",
    "MPL-2.0", "0BSD", "BSL-1.0", "UNLICENSE", "OFL-1.1", "UBUNTU-FONT-1.0", "CC0-1.0",
    "LLVM-EXCEPTION", "CDLA-PERMISSIVE-2.0", "UNICODE-DFS-2016",
}

# License exceptions (after WITH) that have been reviewed.
KNOWN_EXCEPTIONS = {"LLVM-EXCEPTION"}

# "name": "reason". The key is the crate name; use only with a written reason.
ALLOW: dict[str, str] = {
    # (empty on purpose: the dual-licensed r-efi is handled by the expression evaluator, because
    #  "MIT OR Apache-2.0 OR LGPL-2.1-or-later" can be satisfied by choosing MIT.)
}

# Crates that are not plain MIT / Apache-2.0, printed for the THIRD-PARTY-NOTICES.md review.
NOTABLE = ("MPL", "OFL", "UBUNTU", "UNICODE", "LGPL", "BSL", "CDLA")

TOKEN = re.compile(r"\(|\)|[^\s()]+")


def tokenize(expr: str) -> list[str]:
    expr = expr.replace("/", " OR ")  # old crates.io style "MIT/Apache-2.0"
    return TOKEN.findall(expr)


def evaluate(expr: str) -> tuple[bool, list[str]]:
    """True when the expression can be satisfied without a forbidden term.
    Returns (ok, list of unknown identifiers on the satisfying path)."""
    toks = tokenize(expr)
    pos = 0
    unknown: list[str] = []
    broken = False  # a missing operand ("MIT OR", "()") makes the whole expression invalid

    def peek():
        return toks[pos] if pos < len(toks) else None

    def parse_or():
        nonlocal pos
        left = parse_and()
        while peek() is not None and peek().upper() == "OR":
            pos += 1
            right = parse_and()
            left = pick(left, right)
        return left

    def parse_and():
        nonlocal pos
        left = parse_atom()
        while peek() is not None and peek().upper() == "AND":
            pos += 1
            right = parse_atom()
            left = (left[0] and right[0], left[1] + right[1])
        return left

    def parse_atom():
        nonlocal pos, broken
        t = peek()
        if t is None or t == ")" or t.upper() in ("AND", "OR", "WITH"):
            broken = True
            return (False, ["<missing operand>"])
        pos += 1
        if t == "(":
            v = parse_or()
            if peek() == ")":
                pos += 1
            else:
                broken = True  # "(" without ")"
            return v
        ident = t.rstrip("+")
        exception_unknown = []
        if peek() is not None and peek().upper() == "WITH":
            pos += 1
            exception = peek()
            if exception is None or exception in ("(", ")") or exception.upper() in ("AND", "OR", "WITH"):
                broken = True
                return (False, ["<missing exception>"])
            pos += 1
            if exception.upper() not in KNOWN_EXCEPTIONS:
                exception_unknown = [exception]
        up = ident.upper()
        if any(tok in up for tok in FORBIDDEN_TOKENS):
            return (False, [ident])
        if up not in KNOWN:
            return (True, [ident] + exception_unknown)  # permitted syntactically, flagged as unknown
        return (True, exception_unknown)

    def pick(a, b):
        # prefer a satisfiable alternative without unknown ids
        cands = [x for x in (a, b) if x[0]]
        if not cands:
            return (False, a[1] + b[1])
        cands.sort(key=lambda x: len(x[1]))
        return cands[0]

    ok, unknown = parse_or()
    if broken:
        return False, unknown + ["<missing operand>"]
    if pos != len(toks):
        # Trailing tokens ("MIT GPL-3.0", "MIT) AND GPL-3.0", "MIT OR") make the expression invalid.
        return False, unknown + [f"<unparsed: {' '.join(toks[pos:])}>"]
    return ok, unknown


def metadata(features: list[str]) -> dict:
    cargo = str(CARGO) if CARGO.exists() else "cargo"
    cmd = [cargo, "metadata", "--format-version", "1", "--locked",
           "--filter-platform", "aarch64-apple-darwin", *features]
    try:
        return json.loads(subprocess.check_output(cmd, cwd=ROOT, stderr=subprocess.PIPE))
    except subprocess.CalledProcessError as e:
        print(e.stderr.decode(errors="replace"), file=sys.stderr)
        raise SystemExit(f"cargo metadata failed: {' '.join(cmd)}")


def audit(label: str, features: list[str]):
    meta = metadata(features)
    # packages that are really in the resolved graph for this platform
    in_graph = {n["id"] for n in meta["resolve"]["nodes"]}
    roots = set(meta.get("workspace_members") or [])
    rows, violations = [], []
    by_lic: dict[str, int] = {}
    for pkg in meta["packages"]:
        if pkg["id"] not in in_graph:
            continue
        if pkg["id"] in roots:  # the workspace's own crate (MIT, see LICENSE)
            continue
        name, ver = pkg["name"], pkg["version"]
        lic = pkg.get("license")
        if not lic:
            lic = "MISSING" + (f" (license_file={pkg['license_file']})" if pkg.get("license_file") else "")
            ok, unknown = False, ["missing"]
        else:
            ok, unknown = evaluate(lic)
        by_lic[lic] = by_lic.get(lic, 0) + 1
        if (not ok or unknown) and name not in ALLOW:
            what = "forbidden or missing license" if not ok else f"unknown identifier {unknown}"
            violations.append(f"[{label}] {name} {ver}: {lic} -> {what}")
        if any(t in lic.upper() for t in NOTABLE):
            rows.append((name, ver, lic))
    print(f"\n[{label}] {sum(by_lic.values())} packages, {len(by_lic)} license expressions")
    w = max(len(k) for k in by_lic)
    for k, v in sorted(by_lic.items(), key=lambda kv: (-kv[1], kv[0])):
        print(f"  {k:<{w}}  {v:>4}")
    return violations, rows


def check_vendor() -> list[str]:
    out = []
    for f in ("LICENSE", "LICENSE-ggml"):
        p = ROOT / "vendor" / "speech-cpp" / f
        if not p.is_file() or p.stat().st_size == 0:
            out.append(f"vendor/speech-cpp/{f} is missing or empty")
    print("\n[vendor] speech.cpp / ggml license files:",
          "present" if not out else "PROBLEM")
    return out


LICENSE_FILE = re.compile(r"(?i)^(licen[cs]e|copying|notice|unlicense)")
# Folders of a crate that are not compiled into it.
NOT_COMPILED = {"tests", "benches", "examples", "fuzz", "target", "docs", "doc"}
# Files outside the crate's top folder that carry licenses of embedded assets.
EXTRA_LICENSE_FILES = {"epaint_default_fonts": "fonts"}

MIT_TEXT = """MIT License

Copyright (c) {holders}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
"""


def license_texts(pkg: dict) -> list[tuple[str, str]]:
    """(file name, text) of the crate's license files; the MIT text if it has none."""
    folder = Path(pkg["manifest_path"]).parent
    names = sorted(f for f in os.listdir(folder) if LICENSE_FILE.match(f) and (folder / f).is_file())
    found = [(name, (folder / name).read_text(encoding="utf-8", errors="replace")) for name in names]
    if not found:
        if "MIT" not in re.split(r"[\s()/]+", pkg.get("license") or ""):
            raise SystemExit(f"{pkg['name']} {pkg['version']} has no license file and no MIT option")
        authors = [re.sub(r"\s*<[^>]*>", "", a).strip() for a in pkg.get("authors") or []]
        holders = ", ".join(a for a in authors if a) or f"the {pkg['name']} developers"
        found.append(("MIT, no file in the package", MIT_TEXT.format(holders=holders)))
    # License files of code vendored inside the crate (e.g. tracing-core's src/spin/LICENSE), outside the
    # folders that are not compiled into the crate.
    for root, dirs, files in os.walk(folder):
        dirs[:] = sorted(d for d in dirs if d not in NOT_COMPILED and not d.startswith("."))
        if Path(root) == folder:
            continue
        for name in sorted(files):
            if LICENSE_FILE.match(name):
                path = Path(root) / name
                found.append((str(path.relative_to(folder)), path.read_text(encoding="utf-8", errors="replace")))
    extra = EXTRA_LICENSE_FILES.get(pkg["name"])
    if extra and (folder / extra).is_dir():
        for name in sorted(os.listdir(folder / extra)):
            if name.endswith(".txt"):
                found.append((f"{extra}/{name}", (folder / extra / name).read_text(encoding="utf-8", errors="replace")))
    return found


def third_party_licenses() -> str:
    meta = metadata([])
    in_graph = {n["id"] for n in meta["resolve"]["nodes"]}
    roots = set(meta.get("workspace_members") or [])
    packages = sorted(
        (p for p in meta["packages"] if p["id"] in in_graph and p["id"] not in roots),
        key=lambda p: (p["name"], p["version"]),
    )
    texts: dict[str, int] = {}
    bodies: list[str] = []
    users: list[list[str]] = []
    rows = []
    for pkg in packages:
        numbers = []
        for name, text in license_texts(pkg):
            text = text.replace("\r\n", "\n").strip() + "\n"
            key = hashlib.sha256(text.encode()).hexdigest()
            if key not in texts:
                texts[key] = len(bodies) + 1
                bodies.append(text)
                users.append([])
            number = texts[key]
            users[number - 1].append(f"{pkg['name']} {pkg['version']} ({name})")
            numbers.append(number)
        rows.append(f"{pkg['name']} {pkg['version']}: {pkg.get('license') or '-'}  [text "
                    + ", ".join(f"#{n}" for n in sorted(set(numbers))) + "]")
    out = [
        "OpenSpeech: third-party licenses (Rust crates)",
        "",
        "The license texts of the Rust crates in the dependency graph of OpenSpeech for",
        "aarch64-apple-darwin (the distributed build, without optional features; a few of them only",
        "generate code at build time), including the fonts that egui embeds. The bundled speech.cpp /",
        "ggml library, the models and the app itself are covered in THIRD-PARTY-NOTICES.md.",
        "Generated by scripts/check-licenses.py --write from Cargo.lock; do not edit by hand.",
        "",
        f"== Packages ({len(rows)}) ==",
        "",
        *rows,
        "",
        f"== License texts ({len(bodies)}) ==",
    ]
    for number, (body, used) in enumerate(zip(bodies, users), start=1):
        out += ["", "-" * 72, f"#{number}  used by: " + "; ".join(used), "-" * 72, "", body.rstrip("\n")]
    return "\n".join(out) + "\n"


def main() -> int:
    if OPTIONS.write or OPTIONS.check:
        text = third_party_licenses()
        if OPTIONS.write:
            Path(OPTIONS.write).write_text(text, encoding="utf-8")
            print(f"wrote {OPTIONS.write} ({len(text.encode())} bytes)")
        else:
            current = Path(OPTIONS.check).read_text(encoding="utf-8") if Path(OPTIONS.check).is_file() else ""
            if current != text:
                print(f"{OPTIONS.check} is out of date: run "
                      f"python3 -I scripts/check-licenses.py --write {OPTIONS.check}", file=sys.stderr)
                return 1
            print(f"{OPTIONS.check} is up to date")
    violations: list[str] = []
    notable_all: dict[tuple[str, str], str] = {}
    for label, feats in (("build", []),):
        v, rows = audit(label, feats)
        violations += v
        for n, ver, lic in rows:
            notable_all[(n, ver)] = lic
    print("\nNon-plain licenses (for THIRD-PARTY-NOTICES.md):")
    for (n, ver), lic in sorted(notable_all.items()):
        print(f"  {n} {ver}: {lic}")
    for name, why in ALLOW.items():
        print(f"  allow-listed: {name}: {why}")
    violations += check_vendor()
    if violations:
        print("\nLICENSE CHECK FAILED:", file=sys.stderr)
        for v in sorted(set(violations)):
            print(f"  - {v}", file=sys.stderr)
        return 1
    print("\nOK: no GPL / AGPL / LGPL / SSPL / unknown license in the dependency graph.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
