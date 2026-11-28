"""Recursively ingest supported media files into a separate MMDB database."""

import argparse
import json
import os
import subprocess
import sys
from collections.abc import Iterable
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_SOURCE_DIR = PROJECT_ROOT / "data"
DEFAULT_DB_DIR = Path(__file__).resolve().parent / "db"

IMAGE_EXTENSIONS = {".bmp", ".gif", ".jpg", ".jpeg", ".png", ".tif", ".tiff", ".webp"}
AUDIO_EXTENSIONS = {
    ".wav",
    ".wave",
    ".mp3",
    ".flac",
    ".ogg",
    ".opus",
    ".aac",
    ".m4a",
    ".aiff",
    ".aif",
}
VIDEO_EXTENSIONS = {
    ".mp4",
    ".mov",
    ".m4v",
    ".avi",
    ".mkv",
    ".webm",
    ".wmv",
    ".flv",
    ".mpeg",
    ".mpg",
}
SUPPORTED_EXTENSIONS = IMAGE_EXTENSIONS | AUDIO_EXTENSIONS | VIDEO_EXTENSIONS


def _is_within(path: Path, parent: Path) -> bool:
    try:
        path.relative_to(parent)
        return True
    except ValueError:
        return False


def discover_media(source_dir: Path, db_dir: Path) -> list[Path]:
    source_dir = source_dir.resolve()
    db_dir = db_dir.resolve()
    if _is_within(source_dir, db_dir):
        raise ValueError("source directory cannot be inside the MMDB database")
    if not source_dir.is_dir():
        raise ValueError(f"source directory does not exist: {source_dir}")

    media_files: list[Path] = []
    for current, directory_names, filenames in os.walk(
        str(source_dir), followlinks=False
    ):
        current_path = Path(current)
        directory_names[:] = sorted(
            name
            for name in directory_names
            if not (current_path / name).is_symlink()
            and not _is_within((current_path / name).resolve(), db_dir)
        )
        for filename in sorted(filenames):
            path = current_path / filename
            if (
                path.is_symlink()
                or path.suffix.lower() not in SUPPORTED_EXTENSIONS
                or not path.is_file()
            ):
                continue
            media_files.append(path)
    return media_files


def load_ingested_sources(db_dir: Path) -> set[str]:
    catalog_path = db_dir / "data" / "catalog.json"
    if not catalog_path.exists():
        return set()
    with catalog_path.open("r", encoding="utf-8") as catalog_file:
        catalog = json.load(catalog_file)
    if not isinstance(catalog, dict) or not isinstance(catalog.get("objects"), list):
        raise TypeError(f"invalid MMDB catalog structure: {catalog_path}")
    sources: set[str] = set()
    for obj in catalog["objects"]:
        if not isinstance(obj, dict):
            raise TypeError(f"invalid object in MMDB catalog: {catalog_path}")
        metadata = obj.get("metadata", {})
        if isinstance(metadata, dict) and isinstance(metadata.get("source_path"), str):
            sources.add(metadata["source_path"])
    return sources


def _binary_path() -> Path:
    executable = "mmdb.exe" if os.name == "nt" else "mmdb"
    return PROJECT_ROOT / "target" / "release" / executable


def ensure_binary() -> Path:
    binary = _binary_path()
    if binary.is_file():
        return binary
    subprocess.run(["cargo", "build", "--release"], cwd=str(PROJECT_ROOT), check=True)
    if not binary.is_file():
        raise FileNotFoundError(f"MMDB binary was not produced at {binary}")
    return binary


def _metadata_args(source_dir: Path, media_path: Path) -> Iterable[str]:
    relative = media_path.relative_to(source_dir)
    parts = relative.parts
    dataset = parts[0] if len(parts) > 1 else "data"
    category = relative.parent.name if len(parts) > 2 else "uncategorized"
    metadata: dict[str, str] = {
        "dataset": dataset,
        "category": category,
        "source_path": relative.as_posix(),
    }
    return [
        f"{key}={json.dumps(value, ensure_ascii=True)}"
        for key, value in metadata.items()
    ]


def _positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return parsed


def ingest_media(
    source_dir: Path,
    db_dir: Path,
    max_files: int = 0,
    dry_run: bool = False,
) -> int:
    source_dir = source_dir.resolve()
    db_dir = db_dir.resolve()
    discovered = discover_media(source_dir, db_dir)
    existing_sources = set() if dry_run else load_ingested_sources(db_dir)
    new_files = [
        path
        for path in discovered
        if path.relative_to(source_dir).as_posix() not in existing_sources
    ]
    already_ingested = 0 if dry_run else len(discovered) - len(new_files)
    pending = new_files
    if max_files:
        pending = pending[:max_files]

    print(
        f"Discovered {len(discovered)} supported files; {already_ingested} already ingested; {len(pending)} to process."
    )
    if dry_run or not pending:
        return 0

    binary = ensure_binary()
    failures = 0
    for index, path in enumerate(pending, start=1):
        if index == 1 or index % 100 == 0 or index == len(pending):
            print(f"[{index}/{len(pending)}] {path}", flush=True)
        command = [
            str(binary),
            "--db",
            str(db_dir),
            "ingest",
            str(path),
        ] + list(_metadata_args(source_dir, path))
        result = subprocess.run(
            command,
            cwd=str(PROJECT_ROOT),
            capture_output=True,
            text=True,
            check=False,
        )
        if result.returncode != 0:
            failures += 1
            detail = (result.stderr or result.stdout).strip()
            print(
                "Failed to ingest {}: {}".format(path, detail or "unknown error"),
                file=sys.stderr,
                flush=True,
            )

    succeeded = len(pending) - failures
    print(
        f"Ingestion complete: {succeeded} succeeded, {failures} failed."
    )
    return 1 if failures else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source-dir",
        type=Path,
        default=DEFAULT_SOURCE_DIR,
        help="directory to scan recursively (default: %(default)s)",
    )
    parser.add_argument(
        "--db",
        type=Path,
        default=DEFAULT_DB_DIR,
        help="MMDB database directory (default: %(default)s)",
    )
    parser.add_argument(
        "--max-files",
        type=_positive_int,
        default=0,
        help="ingest at most this many new files; useful for smoke tests",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="list supported files without building MMDB or ingesting",
    )
    args = parser.parse_args()
    return ingest_media(args.source_dir, args.db, args.max_files, args.dry_run)


if __name__ == "__main__":
    sys.exit(main())
