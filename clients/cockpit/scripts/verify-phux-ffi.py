#!/usr/bin/env python3
"""Verify and attest Cockpit's same-checkout Phux client FFI."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
REPO_ROOT = ROOT.parent.parent
INCLUDE_DIR = REPO_ROOT / "crates/phux-client-ffi/include"
LIB_DIR = REPO_ROOT / "target/ffi-release"
PROVENANCE_KEYS = {
    "schema",
    "repository",
    "commit",
    "workspace_version",
    "client_abi_version",
    "cargo_profile",
}


class VerificationError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise VerificationError(message)


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        raise VerificationError(f"cannot read {path}: {error}") from error


def git_output(tree: Path, *args: str) -> str:
    result = subprocess.run(
        ("git", "-C", str(tree), *args),
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    require(result.returncode == 0, f"cannot inspect Phux checkout with git {' '.join(args)}")
    return result.stdout.strip()


def canonical_remote(value: str) -> str:
    value = value.removesuffix(".git")
    ssh = re.fullmatch(r"git@github\.com:(.+)", value)
    if ssh:
        return ssh.group(1)
    https = re.fullmatch(r"https?://github\.com/(.+)", value)
    return https.group(1) if https else value


def cargo_section(text: str, heading: str, label: str) -> str:
    match = re.search(rf"(?ms)^\[{re.escape(heading)}\]\s*$\n(.*?)(?=^\[|\Z)", text)
    require(match is not None, f"{label} is missing [{heading}]")
    return match.group(1)


def cargo_string(section: str, key: str, label: str) -> str:
    match = re.search(rf'(?m)^{re.escape(key)}\s*=\s*"([^"]+)"\s*$', section)
    require(match is not None, f"{label} is missing {key}")
    return match.group(1)


def source_metadata(tree: Path = REPO_ROOT) -> dict[str, Any]:
    root_manifest = read_text(tree / "Cargo.toml")
    workspace = cargo_section(root_manifest, "workspace.package", "Phux Cargo.toml")
    version = cargo_string(workspace, "version", "Phux [workspace.package]")

    header = read_text(tree / "crates/phux-client-ffi/include/phux/client.h")
    abi_match = re.search(r"(?m)^#define PHUX_CLIENT_ABI_VERSION ([0-9]+)u$", header)
    profile_match = re.search(r'^#define PHUX_CLIENT_RELEASE_CARGO_PROFILE "([^"]+)"$', header, re.MULTILINE)
    require(abi_match is not None, "Phux header lacks PHUX_CLIENT_ABI_VERSION")
    require(profile_match is not None, "Phux header lacks PHUX_CLIENT_RELEASE_CARGO_PROFILE")

    repository = canonical_remote(git_output(tree, "remote", "get-url", "origin"))
    require(
        re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is not None,
        "Phux origin is not a GitHub owner/repository",
    )
    return {
        "schema": 2,
        "repository": repository,
        "commit": git_output(tree, "rev-parse", "HEAD"),
        "workspace_version": version,
        "client_abi_version": int(abi_match.group(1)),
        "cargo_profile": profile_match.group(1),
    }


def verify_source_tree(tree: Path) -> dict[str, Any]:
    require(tree.resolve() == REPO_ROOT.resolve(), "Cockpit must consume the enclosing Phux checkout")
    metadata = source_metadata(tree)
    profile = metadata["cargo_profile"]
    cargo_section(read_text(tree / "Cargo.toml"), f"profile.{profile}", "Phux Cargo.toml")

    ffi_manifest = read_text(tree / "crates/phux-client-ffi/Cargo.toml")
    package = cargo_section(ffi_manifest, "package", "phux-client-ffi Cargo.toml")
    require(
        re.search(r"(?m)^version\.workspace\s*=\s*true\s*$", package) is not None,
        "phux-client-ffi must inherit the workspace version",
    )
    lockfile = read_text(tree / "Cargo.lock")
    locked_ffi = re.search(
        r'(?m)^\[\[package\]\]\s*$\nname = "phux-client-ffi"\s*$\nversion = "([^"]+)"\s*$',
        lockfile,
    )
    require(locked_ffi is not None, "Phux Cargo.lock lacks phux-client-ffi")
    require(
        locked_ffi.group(1) == metadata["workspace_version"],
        "Phux Cargo.lock phux-client-ffi version is skewed",
    )
    return metadata


def verify_ffi_paths(tree: Path, include_dir: Path, library_dir: Path, profile: str) -> None:
    require(include_dir.resolve() == (tree / "crates/phux-client-ffi/include").resolve(),
            "Phux FFI include directory is not from the enclosing checkout")
    require(library_dir.resolve() == (tree / "target" / profile).resolve(),
            "Phux FFI library directory is not from the enclosing checkout/profile")
    require((include_dir / "phux/client.h").is_file(), "Phux FFI header is missing")
    require((library_dir / "libphux_client_ffi.a").is_file(), "Phux FFI static library is missing")


def verify_repository_contract() -> None:
    require(not (ROOT / "phux-ffi.lock.json").exists(), "the retired cross-repository FFI lock still exists")
    readme = read_text(ROOT / "README.md")
    require("same Phux checkout" in readme, "README does not describe same-checkout FFI composition")
    notices = " ".join(read_text(ROOT / "THIRD_PARTY_NOTICES.md").split())
    require("same source checkout" in notices, "third-party notices do not describe same-checkout provenance")
    inventory = read_text(ROOT / "assets/licenses/Phux-FFI-THIRD-PARTY.html")
    for package in ("phux-client-core", "phux-client-ffi", "phux-protocol"):
        require(f">{package} " in inventory, f"license inventory lacks {package}")
    require("https://github.com/no-phux/phux" in inventory, "license inventory lacks the canonical Phux URL")

    package = read_text(ROOT / "scripts/package-macos.sh")
    for fragment in (
        'PHUX_SOURCE_DIR="${PHUX_SOURCE_DIR:-${REPO_ROOT}}"',
        '--write-provenance "${RESOURCES}/Phux-FFI-Provenance.json"',
        '--phux-tree "${PHUX_SOURCE_DIR}"',
        '--ffi-include-dir "${PHUX_CLIENT_FFI_INCLUDE_DIR}"',
        '--ffi-lib-dir "${PHUX_CLIENT_FFI_LIB_DIR}"',
    ):
        require(fragment in package, f"package-macos.sh lacks {fragment}")


def load_provenance(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(read_text(path))
    except json.JSONDecodeError as error:
        raise VerificationError(f"packaged provenance is invalid JSON: {error}") from error
    require(isinstance(value, dict), "packaged provenance must be a JSON object")
    require(set(value) == PROVENANCE_KEYS, "packaged provenance has unexpected or missing keys")
    return value


def write_provenance(path: Path, metadata: dict[str, Any]) -> None:
    require(
        not git_output(REPO_ROOT, "status", "--porcelain", "--untracked-files=no"),
        "refusing to attest a dirty source checkout",
    )
    path.write_text(json.dumps(metadata, separators=(",", ":"), sort_keys=True) + "\n", encoding="utf-8")


def emit_github_output(path: Path, metadata: dict[str, Any]) -> None:
    values = {
        "repository": metadata["repository"],
        "ref": metadata["commit"],
        "version": metadata["workspace_version"],
        "profile": metadata["cargo_profile"],
        "abi": str(metadata["client_abi_version"]),
    }
    with path.open("a", encoding="utf-8") as output:
        for key, value in values.items():
            output.write(f"{key}={value}\n")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--github-output", type=Path, help="append source values as GitHub outputs")
    parser.add_argument("--phux-tree", type=Path, help="verify the enclosing Phux source tree")
    parser.add_argument("--provenance-file", type=Path, help="verify packaged provenance")
    parser.add_argument("--write-provenance", type=Path, help="write packaged provenance")
    parser.add_argument("--ffi-include-dir", type=Path, help="verify the consumed FFI include directory")
    parser.add_argument("--ffi-lib-dir", type=Path, help="verify the consumed FFI library directory")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        verify_repository_contract()
        require(bool(args.ffi_include_dir) == bool(args.ffi_lib_dir),
                "--ffi-include-dir and --ffi-lib-dir must be supplied together")
        tree = args.phux_tree or REPO_ROOT
        metadata = verify_source_tree(tree)
        if args.ffi_include_dir:
            verify_ffi_paths(tree, args.ffi_include_dir, args.ffi_lib_dir, metadata["cargo_profile"])
        if args.github_output:
            emit_github_output(args.github_output, metadata)
        if args.write_provenance:
            write_provenance(args.write_provenance, metadata)
        if args.provenance_file:
            require(load_provenance(args.provenance_file) == metadata,
                    "packaged Phux FFI provenance does not match this source checkout")
    except (OSError, VerificationError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
