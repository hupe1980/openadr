"""Review the public library and feature inventory from the repository root."""

import argparse
import difflib
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

NIGHTLY = "nightly-2026-11-14"
RENDERER = "0.52.0"
BASELINE = Path(__file__).resolve().with_name("public-api.txt")
ROOT = BASELINE.parents[2]


def output(*args):
    return subprocess.check_output(args, cwd=ROOT, text=True)


def inventory():
    try:
        version = output("cargo", "public-api", "--version").strip()
    except (OSError, subprocess.CalledProcessError) as error:
        raise ValueError("cargo-public-api unavailable. Rerun with --install-tools.") from error
    if version != f"cargo-public-api {RENDERER}":
        raise ValueError(
            f"expected cargo-public-api {RENDERER}, found {version}. Rerun with --install-tools."
        )

    cargo = ["cargo", f"+{NIGHTLY}"]
    metadata = json.loads(output(*cargo, "metadata", "--format-version=1", "--no-deps", "--locked"))
    package = next(pkg for pkg in metadata["packages"] if pkg["name"] == "openadr")
    # An explicit target gives the rustdoc JSON a known location even with custom Cargo config.
    target = (
        os.environ.get("CARGO_BUILD_TARGET")
        or output("rustc", f"+{NIGHTLY}", "--print", "host-tuple").strip()
    )
    subprocess.run(
        [
            *cargo,
            "rustdoc",
            "--locked",
            "-p",
            "openadr",
            "--lib",
            "--all-features",
            "--target",
            target,
            "--",
            "-Z",
            "unstable-options",
            "--output-format",
            "json",
        ],
        cwd=ROOT,
        check=True,
    )
    rustdoc = Path(metadata["target_directory"]) / target / "doc/openadr.json"
    signatures = output(
        "cargo",
        "public-api",
        "--rustdoc-json",
        rustdoc,
        "--omit",
        "blanket-impls",
        "--color",
        "never",
    )
    if not signatures.startswith("pub mod openadr\n"):
        raise ValueError("missing openadr library inventory")

    features = package["features"]
    lines = [f"rust-version = {package['rust_version']}", "", "[features]"]
    for name, enabled in sorted(features.items()):
        public = sorted(set(enabled).intersection(features))
        lines.append(f"{name} = [{', '.join(public)}]")
    return "\n".join(lines) + "\n\n[public Rust API]\n" + signatures


def check(baseline, generated, update):
    if update:
        # Replace only after a complete successful generation and write.
        with tempfile.TemporaryDirectory(dir=baseline.parent) as directory:
            staged = Path(directory) / baseline.name
            staged.write_text(generated, encoding="utf-8")
            staged.replace(baseline)
        print(f"Updated {baseline.name}")
        return 0

    expected = baseline.read_text(encoding="utf-8")
    if expected == generated:
        print("Public API and Cargo features match the reviewed inventory.")
        return 0
    sys.stdout.writelines(
        difflib.unified_diff(
            expected.splitlines(True),
            generated.splitlines(True),
            fromfile=baseline.name,
            tofile="current API",
        )
    )
    print("Review the diff. For an intentional change, rerun with --update.", file=sys.stderr)
    return 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--update", action="store_true", help="replace the reviewed inventory")
    parser.add_argument(
        "--install-tools",
        action="store_true",
        help="install the pinned development tools before running",
    )
    args = parser.parse_args()
    try:
        if args.install_tools:
            subprocess.run(
                ["rustup", "toolchain", "install", NIGHTLY, "--profile", "minimal"], check=True
            )
            subprocess.run(
                [
                    "cargo",
                    "+stable",
                    "install",
                    "--locked",
                    "--version",
                    f"={RENDERER}",
                    "cargo-public-api",
                ],
                check=True,
            )
        return check(BASELINE, inventory(), args.update)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Compatibility check failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
