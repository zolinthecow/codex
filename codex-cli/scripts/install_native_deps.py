#!/usr/bin/env python3
"""Install Codex native binaries (Rust CLI plus ripgrep helpers)."""

import argparse
import json
import os
import shutil
import subprocess
import tarfile
import tempfile
import zipfile
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from typing import Iterable, Sequence
from urllib.parse import urlparse
from urllib.request import urlopen

SCRIPT_DIR = Path(__file__).resolve().parent
CODEX_CLI_ROOT = SCRIPT_DIR.parent
DEFAULT_WORKFLOW_URL = "https://github.com/openai/codex/actions/runs/17952349351"  # rust-v0.40.0
VENDOR_DIR_NAME = "vendor"
RG_MANIFEST = CODEX_CLI_ROOT / "bin" / "rg"
CODEX_TARGETS = (
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
)

RG_TARGET_PLATFORM_PAIRS: list[tuple[str, str]] = [
    ("x86_64-unknown-linux-musl", "linux-x86_64"),
    ("aarch64-unknown-linux-musl", "linux-aarch64"),
    ("x86_64-apple-darwin", "macos-x86_64"),
    ("aarch64-apple-darwin", "macos-aarch64"),
    ("x86_64-pc-windows-msvc", "windows-x86_64"),
    ("aarch64-pc-windows-msvc", "windows-aarch64"),
]
RG_TARGET_TO_PLATFORM = {target: platform for target, platform in RG_TARGET_PLATFORM_PAIRS}
DEFAULT_RG_TARGETS = [target for target, _ in RG_TARGET_PLATFORM_PAIRS]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Install native Codex binaries.")
    parser.add_argument(
        "--workflow-url",
        help=(
            "GitHub Actions workflow URL that produced the artifacts. Defaults to a "
            "known good run when omitted."
        ),
    )
    parser.add_argument(
        "root",
        nargs="?",
        type=Path,
        help=(
            "Directory containing package.json for the staged package. If omitted, the "
            "repository checkout is used."
        ),
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()

    codex_cli_root = (args.root or CODEX_CLI_ROOT).resolve()
    vendor_dir = codex_cli_root / VENDOR_DIR_NAME
    vendor_dir.mkdir(parents=True, exist_ok=True)

    workflow_url = (args.workflow_url or DEFAULT_WORKFLOW_URL).strip()
    if not workflow_url:
        workflow_url = DEFAULT_WORKFLOW_URL

    workflow_id = workflow_url.rstrip("/").split("/")[-1]

    with tempfile.TemporaryDirectory(prefix="codex-native-artifacts-") as artifacts_dir_str:
        artifacts_dir = Path(artifacts_dir_str)
        _download_artifacts(workflow_id, artifacts_dir)
        install_codex_binaries(artifacts_dir, vendor_dir, CODEX_TARGETS)

    fetch_rg(vendor_dir, DEFAULT_RG_TARGETS, manifest_path=RG_MANIFEST)

    print(f"Installed native dependencies into {vendor_dir}")
    return 0


def fetch_rg(
    vendor_dir: Path,
    targets: Sequence[str] | None = None,
    *,
    manifest_path: Path,
) -> list[Path]:
    """Download ripgrep binaries described by the DotSlash manifest."""

    if targets is None:
        targets = DEFAULT_RG_TARGETS

    if not manifest_path.exists():
        raise FileNotFoundError(f"DotSlash manifest not found: {manifest_path}")

    manifest = _load_manifest(manifest_path)
    platforms = manifest.get("platforms", {})

    vendor_dir.mkdir(parents=True, exist_ok=True)

    targets = list(targets)
    if not targets:
        return []

    task_configs: list[tuple[str, str, dict]] = []
    for target in targets:
        platform_key = RG_TARGET_TO_PLATFORM.get(target)
        if platform_key is None:
            raise ValueError(f"Unsupported ripgrep target '{target}'.")

        platform_info = platforms.get(platform_key)
        if platform_info is None:
            raise RuntimeError(f"Platform '{platform_key}' not found in manifest {manifest_path}.")

        task_configs.append((target, platform_key, platform_info))

    results: dict[str, Path] = {}
    max_workers = min(len(task_configs), max(1, (os.cpu_count() or 1)))

    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        future_map = {
            executor.submit(
                _fetch_single_rg,
                vendor_dir,
                target,
                platform_key,
                platform_info,
                manifest_path,
            ): target
            for target, platform_key, platform_info in task_configs
        }

        for future in as_completed(future_map):
            target = future_map[future]
            results[target] = future.result()

    return [results[target] for target in targets]


def _download_artifacts(workflow_id: str, dest_dir: Path) -> None:
    cmd = [
        "gh",
        "run",
        "download",
        "--dir",
        str(dest_dir),
        "--repo",
        "openai/codex",
        workflow_id,
    ]
    subprocess.check_call(cmd)


def install_codex_binaries(
    artifacts_dir: Path, vendor_dir: Path, targets: Iterable[str]
) -> list[Path]:
    targets = list(targets)
    if not targets:
        return []

    results: dict[str, Path] = {}
    max_workers = min(len(targets), max(1, (os.cpu_count() or 1)))

    with ThreadPoolExecutor(max_workers=max_workers) as executor:
        future_map = {
            executor.submit(_install_single_codex_binary, artifacts_dir, vendor_dir, target): target
            for target in targets
        }

        for future in as_completed(future_map):
            target = future_map[future]
            results[target] = future.result()

    return [results[target] for target in targets]


def _install_single_codex_binary(artifacts_dir: Path, vendor_dir: Path, target: str) -> Path:
    artifact_subdir = artifacts_dir / target
    archive_name = _archive_name_for_target(target)
    archive_path = artifact_subdir / archive_name
    if not archive_path.exists():
        raise FileNotFoundError(f"Expected artifact not found: {archive_path}")

    dest_dir = vendor_dir / target / "codex"
    dest_dir.mkdir(parents=True, exist_ok=True)

    binary_name = "codex.exe" if "windows" in target else "codex"
    dest = dest_dir / binary_name
    dest.unlink(missing_ok=True)
    extract_archive(archive_path, "zst", None, dest)
    if "windows" not in target:
        dest.chmod(0o755)
    return dest


def _archive_name_for_target(target: str) -> str:
    if "windows" in target:
        return f"codex-{target}.exe.zst"
    return f"codex-{target}.zst"


def _fetch_single_rg(
    vendor_dir: Path,
    target: str,
    platform_key: str,
    platform_info: dict,
    manifest_path: Path,
) -> Path:
    providers = platform_info.get("providers", [])
    if not providers:
        raise RuntimeError(f"No providers listed for platform '{platform_key}' in {manifest_path}.")

    url = providers[0]["url"]
    archive_format = platform_info.get("format", "zst")
    archive_member = platform_info.get("path")

    dest_dir = vendor_dir / target / "path"
    dest_dir.mkdir(parents=True, exist_ok=True)

    is_windows = platform_key.startswith("win")
    binary_name = "rg.exe" if is_windows else "rg"
    dest = dest_dir / binary_name

    with tempfile.TemporaryDirectory() as tmp_dir_str:
        tmp_dir = Path(tmp_dir_str)
        archive_filename = os.path.basename(urlparse(url).path)
        download_path = tmp_dir / archive_filename
        _download_file(url, download_path)

        dest.unlink(missing_ok=True)
        extract_archive(download_path, archive_format, archive_member, dest)

    if not is_windows:
        dest.chmod(0o755)

    return dest


def _download_file(url: str, dest: Path) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)
    with urlopen(url) as response, open(dest, "wb") as out:
        shutil.copyfileobj(response, out)


def extract_archive(
    archive_path: Path,
    archive_format: str,
    archive_member: str | None,
    dest: Path,
) -> None:
    dest.parent.mkdir(parents=True, exist_ok=True)

    if archive_format == "zst":
        output_path = archive_path.parent / dest.name
        subprocess.check_call(
            ["zstd", "-f", "-d", str(archive_path), "-o", str(output_path)]
        )
        shutil.move(str(output_path), dest)
        return

    if archive_format == "tar.gz":
        if not archive_member:
            raise RuntimeError("Missing 'path' for tar.gz archive in DotSlash manifest.")
        with tarfile.open(archive_path, "r:gz") as tar:
            try:
                member = tar.getmember(archive_member)
            except KeyError as exc:
                raise RuntimeError(
                    f"Entry '{archive_member}' not found in archive {archive_path}."
                ) from exc
            tar.extract(member, path=archive_path.parent, filter="data")
        extracted = archive_path.parent / archive_member
        shutil.move(str(extracted), dest)
        return

    if archive_format == "zip":
        if not archive_member:
            raise RuntimeError("Missing 'path' for zip archive in DotSlash manifest.")
        with zipfile.ZipFile(archive_path) as archive:
            try:
                with archive.open(archive_member) as src, open(dest, "wb") as out:
                    shutil.copyfileobj(src, out)
            except KeyError as exc:
                raise RuntimeError(
                    f"Entry '{archive_member}' not found in archive {archive_path}."
                ) from exc
        return

    raise RuntimeError(f"Unsupported archive format '{archive_format}'.")


def _load_manifest(manifest_path: Path) -> dict:
    cmd = ["dotslash", "--", "parse", str(manifest_path)]
    stdout = subprocess.check_output(cmd, text=True)
    try:
        manifest = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"Invalid DotSlash manifest output from {manifest_path}.") from exc

    if not isinstance(manifest, dict):
        raise RuntimeError(
            f"Unexpected DotSlash manifest structure for {manifest_path}: {type(manifest)!r}"
        )

    return manifest


if __name__ == "__main__":
    import sys

    sys.exit(main())
