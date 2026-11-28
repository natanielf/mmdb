"""Download configured media archives and safely extract them under data/."""

import argparse
import re
import shutil
import stat
import sys
import tarfile
import urllib.request
import zipfile
from collections.abc import Iterable
from pathlib import Path, PurePosixPath
from urllib.parse import unquote, urlparse

PROJECT_ROOT = Path(__file__).resolve().parents[1]
DATA_DIR = PROJECT_ROOT / "data"
DOWNLOAD_DIR = DATA_DIR / ".downloads"

SOURCES: list[tuple[str, str]] = [
    (
        "caltech-101",
        "https://data.caltech.edu/records/mzrjq-6wc02/files/caltech-101.zip?download=1",
    ),
    (
        "esc-50",
        "https://github.com/karoldvl/ESC-50/archive/master.zip",
    ),
    (
        "ucf101-ds",
        "https://www.crcv.ucf.edu/data1/UCF101-DS/UCF101-DS.zip",
    ),
]


def _safe_destination(root: Path, member_name: str) -> Path:
    normalized_name = member_name.replace("\\", "/")
    member_path = PurePosixPath(normalized_name)
    if not member_path.parts and normalized_name in ("", ".", "./"):
        return root.resolve()
    if (
        member_path.is_absolute()
        or not member_path.parts
        or any(part in ("", ".", "..") for part in member_path.parts)
        or member_path.parts[0].endswith(":")
    ):
        raise ValueError(f"archive contains an unsafe path: {member_name!r}")

    root = root.resolve()
    destination = root.joinpath(*member_path.parts).resolve()
    try:
        destination.relative_to(root)
    except ValueError as error:
        raise ValueError(
            f"archive path escapes extraction directory: {member_name!r}"
        ) from error
    return destination


def _extract_zip(archive_path: Path, destination: Path) -> None:
    with zipfile.ZipFile(str(archive_path)) as archive:
        for member in archive.infolist():
            path = _safe_destination(destination, member.filename)
            mode = member.external_attr >> 16
            if stat.S_ISLNK(mode):
                raise ValueError(
                    f"archive contains a symbolic link: {member.filename!r}"
                )
            if member.is_dir():
                path.mkdir(parents=True, exist_ok=True)
                continue
            path.parent.mkdir(parents=True, exist_ok=True)
            with archive.open(member) as source, path.open("wb") as target:
                shutil.copyfileobj(source, target)


def _extract_tar(archive_path: Path, destination: Path) -> None:
    with tarfile.open(str(archive_path), mode="r:*") as archive:
        for member in archive.getmembers():
            path = _safe_destination(destination, member.name)
            if member.isdir():
                path.mkdir(parents=True, exist_ok=True)
                continue
            if not member.isfile():
                raise ValueError(
                    f"archive contains an unsupported link or special file: {member.name!r}"
                )
            source = archive.extractfile(member)
            if source is None:
                raise ValueError(
                    f"cannot read archive member: {member.name!r}"
                )
            path.parent.mkdir(parents=True, exist_ok=True)
            with source, path.open("wb") as target:
                shutil.copyfileobj(source, target)


def extract_archive(archive_path: Path, destination: Path) -> None:
    if destination.is_symlink():
        raise ValueError(
            f"refusing to extract through a symbolic link: {destination}"
        )
    destination.mkdir(parents=True, exist_ok=True)
    if zipfile.is_zipfile(str(archive_path)):
        _extract_zip(archive_path, destination)
    elif tarfile.is_tarfile(str(archive_path)):
        _extract_tar(archive_path, destination)
    else:
        raise ValueError(f"unsupported archive format: {archive_path}")


def _extract_embedded_archives(destination: Path) -> None:
    archive_suffixes = (
        ".zip",
        ".tar",
        ".tar.gz",
        ".tgz",
        ".tar.bz2",
        ".tbz2",
        ".tar.xz",
        ".txz",
    )
    archives = sorted(
        path
        for path in destination.rglob("*")
        if path.is_file()
        and path.name.lower().endswith(archive_suffixes)
        and (zipfile.is_zipfile(str(path)) or tarfile.is_tarfile(str(path)))
    )
    for archive_path in archives:
        print(f"Extracting nested archive {archive_path}...", flush=True)
        extract_archive(archive_path, archive_path.parent)


def _validate_source_name(name: str) -> str:
    safe_name = re.sub(r"[^A-Za-z0-9._-]+", "-", name).strip(".-")
    if not safe_name or safe_name in (".", ".."):
        raise ValueError("source name must contain letters or digits")
    return safe_name


def _archive_filename(url: str, index: int) -> str:
    path = unquote(urlparse(url).path)
    filename = Path(path).name
    if filename:
        return filename
    return f"source-{index}.archive"


def download_archive(url: str, destination: Path) -> None:
    request = urllib.request.Request(
        url, headers={"User-Agent": "mmdb-benchmark/1.0"}
    )
    with urllib.request.urlopen(request, timeout=60) as response, destination.open(
        "wb"
    ) as target:
        shutil.copyfileobj(response, target)


def download_sources(sources: Iterable[tuple[str, str]]) -> int:
    configured_sources = list(sources)
    if not configured_sources:
        print(
            "No sources configured. Add (dataset_name, archive_url) entries to "
            "SOURCES in benchmark/download.py.",
            file=sys.stderr,
        )
        return 2

    if DOWNLOAD_DIR.is_symlink():
        raise ValueError(f"refusing to download through a symbolic link: {DOWNLOAD_DIR}")
    DOWNLOAD_DIR.mkdir(parents=True, exist_ok=True)
    DATA_DIR.mkdir(parents=True, exist_ok=True)
    for index, (name, url) in enumerate(configured_sources, start=1):
        source_name = _validate_source_name(name)
        if not url.startswith(("https://", "http://")):
            raise ValueError(f"source URL must use HTTP or HTTPS: {url!r}")
        archive_path = DOWNLOAD_DIR / f"{source_name}-{_archive_filename(url, index)}"
        if archive_path.is_symlink():
            raise ValueError(
                f"refusing to overwrite a symbolic link: {archive_path}"
            )
        extraction_dir = DATA_DIR / source_name
        print(f"Downloading {source_name}...", flush=True)
        download_archive(url, archive_path)
        print(
            f"Extracting {source_name} to {extraction_dir}...",
            flush=True,
        )
        extract_archive(archive_path, extraction_dir)
        _extract_embedded_archives(extraction_dir)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.parse_args()
    return download_sources(SOURCES)


if __name__ == "__main__":
    sys.exit(main())
