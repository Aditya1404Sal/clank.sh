#!/usr/bin/env python3
"""Count the panic surface of shipped library code.

Backs the numbers in docs/audit/RELIABILITY.md. Excludes comment lines, `#[cfg(test)]` modules
(including the compound `#[cfg(all(test, not(target_arch = "wasm32")))]` form the utilities use),
and `tests.rs`. Run from the repo root; prints every site so a changed total can be diffed, not
just noticed.
"""

import re
import pathlib
import sys

CFG_TEST = re.compile(r'^\s*#\[cfg\(.*\btest\b.*\)\]')

PATTERNS = {
    "unwrap()": r"\.unwrap\(\)",
    "expect(": r"\.expect\(",
    "panic!": r"\bpanic!\(",
    "unreachable!": r"\bunreachable!\(",
    "todo!": r"\btodo!\(",
    "unimplemented!": r"\bunimplemented!\(",
}


def blank_noncode(text):
    """Blank comment lines and cfg(test) modules, preserving line numbers."""
    lines = text.split("\n")
    out, i, n = [], 0, len(lines)
    while i < n:
        if CFG_TEST.match(lines[i]):
            depth, started = 0, False
            while i < n:
                depth += lines[i].count("{") - lines[i].count("}")
                started = started or "{" in lines[i]
                out.append("")
                i += 1
                if started and depth <= 0:
                    break
            continue
        out.append("" if lines[i].strip().startswith("//") else lines[i])
        i += 1
    return "\n".join(out)


def main():
    src = sorted(
        p
        for d in ("crates", "utilities")
        for p in pathlib.Path(d).rglob("*.rs")
        if "/src/" in str(p) and p.name != "tests.rs" and "/target/" not in str(p)
    )
    if not src:
        sys.exit("no sources found — run from the repo root")

    totals = {k: 0 for k in PATTERNS}
    sites, unsafe_n, lock_n, lock_ok = [], 0, 0, 0
    for path in src:
        text = blank_noncode(path.read_text())
        unsafe_n += len(re.findall(r"\bunsafe\s*\{", text))
        for m in re.finditer(r"\.lock\(\)", text):
            lock_n += 1
            tail = text[m.end() : m.end() + 90]
            if re.match(r"\s*\.unwrap_or_else\(\s*(std::sync::)?PoisonError::into_inner", tail):
                lock_ok += 1
        for name, rx in PATTERNS.items():
            for m in re.finditer(rx, text):
                totals[name] += 1
                sites.append((name, path, text[: m.start()].count("\n") + 1))

    print("PANIC PRIMITIVES — shipped library code only")
    for name, count in totals.items():
        print(f"  {name:16} {count}")
    print(f"  {'TOTAL':16} {sum(totals.values())}\n")
    print(f"unsafe blocks:  {unsafe_n}")
    print(f".lock() sites:  {lock_n}   poison-safe: {lock_ok}   NOT poison-safe: {lock_n - lock_ok}\n")
    for name, path, line in sorted(sites, key=lambda s: (str(s[1]), s[2])):
        print(f"  {name:16} {path}:{line}")


if __name__ == "__main__":
    main()
