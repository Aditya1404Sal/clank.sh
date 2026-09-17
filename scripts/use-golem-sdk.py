#!/usr/bin/env python3
"""Point the workspace at an existing Golem checkout, preserving absolute dependency paths."""
import argparse
from pathlib import Path
import re
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("checkout", type=Path)
parser.add_argument("--expected-revision", help="Require this exact Golem commit before changing Cargo.toml")
args = parser.parse_args()
checkout = args.checkout.resolve()
sdk = checkout / "sdks/rust/golem-rust"
if not (sdk / "Cargo.toml").is_file():
    parser.error(f"missing SDK at {sdk}")
if args.expected_revision:
    revision = subprocess.check_output(["git", "-C", str(checkout), "rev-parse", "HEAD"], text=True).strip()
    if revision != args.expected_revision:
        parser.error(f"expected Golem {args.expected_revision}, got {revision}")

manifest = Path(__file__).resolve().parents[1] / "Cargo.toml"
source = manifest.read_text()
# TOML basic-string escaping; never interpolate the path through a shell.
escaped = str(sdk).replace("\\", "\\\\").replace('"', '\\"')
updated, count = re.subn(r'(?m)^(golem-rust = \{ path = )"[^"\n]*"( \})$',
                        lambda m: m[1] + '"' + escaped + '"' + m[2], source)
if count != 1:
    parser.error("expected exactly one workspace golem-rust path dependency")
manifest.write_text(updated)
print(f"Workspace SDK: {sdk}")
