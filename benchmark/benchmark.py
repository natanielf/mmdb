"""Run repeatable query benchmarks against an ingested MMDB database."""

from __future__ import annotations

import argparse
import csv
import json
import os
import random
import re
import subprocess
import sys
from collections.abc import Sequence
from pathlib import Path
from typing import Any

PROJECT_ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DB_DIR = Path(__file__).resolve().parent / "db"
DEFAULT_OUTPUT_DIR = Path(__file__).resolve().parent / "results"
MEDIA_TYPES = ("image", "audio", "video")
PLOT_RED = "#a50034"
PLOT_BLUE = "#2878b5"
PLOT_GRAY = "#666666"


def _positive_int(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return parsed


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


def run_mmdb(binary: Path, db_dir: Path, command: Sequence[str]) -> str:
    result = subprocess.run(
        [str(binary), "--db", str(db_dir)] + list(command),
        cwd=str(PROJECT_ROOT),
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise RuntimeError(
            f"MMDB command failed ({' '.join(command[:1])}): "
            f"{detail or 'unknown error'}"
        )
    return result.stdout


def load_catalog(db_dir: Path) -> dict[str, Any]:
    catalog_path = db_dir / "data" / "catalog.json"
    if not catalog_path.is_file():
        raise FileNotFoundError(
            f"MMDB catalog not found: {catalog_path}. "
            "Run benchmark/ingest.py first."
        )
    with catalog_path.open("r", encoding="utf-8") as catalog_file:
        catalog = json.load(catalog_file)
    if not isinstance(catalog, dict) or not isinstance(catalog.get("objects"), list):
        raise TypeError(f"invalid MMDB catalog structure: {catalog_path}")
    if not catalog["objects"]:
        raise ValueError("MMDB catalog contains no ingested media")
    return catalog


def _sql_string(value: str) -> str:
    return "'" + value.replace("\\", "\\\\").replace("'", "\\'") + "'"


def build_queries(
    catalog: dict[str, Any], db_dir: Path, queries_per_modality: int, seed: int
) -> tuple[list[str], dict[str, int]]:
    objects_by_type: dict[str, list[dict[str, Any]]] = {
        media_type: [] for media_type in MEDIA_TYPES
    }
    for obj in catalog["objects"]:
        if not isinstance(obj, dict):
            raise TypeError("invalid object in MMDB catalog")
        media_type = obj.get("media_type")
        location = obj.get("physical_location")
        if isinstance(media_type, str) and media_type in objects_by_type and isinstance(
            location, str
        ):
            query_path = db_dir / location
            if query_path.is_file():
                objects_by_type[media_type].append(obj)

    randomizer = random.Random(seed)
    statements: list[str] = []
    counts: dict[str, int] = {}
    for media_type in MEDIA_TYPES:
        candidates = objects_by_type[media_type]
        if not candidates:
            continue
        selected = randomizer.sample(
            candidates, min(queries_per_modality, len(candidates))
        )
        category_counts: dict[str, int] = {}
        for obj in candidates:
            metadata = obj.get("metadata")
            category = metadata.get("category") if isinstance(metadata, dict) else None
            if isinstance(category, str):
                category_counts[category] = category_counts.get(category, 0) + 1
        selective_category = (
            min(category_counts, key=lambda category: category_counts[category])
            if category_counts
            else None
        )
        counts[media_type] = len(selected)
        for obj in selected:
            query_path = (db_dir / obj["physical_location"]).resolve()
            statements.append(
                f"FIND {media_type.upper()} SIMILAR TO "
                f"{_sql_string(str(query_path))} LIMIT 10"
            )
            metadata = obj.get("metadata")
            value = metadata.get("category") if isinstance(metadata, dict) else None
            if isinstance(value, str):
                statements.append(
                    f"FIND {media_type.upper()} WHERE category = "
                    f"{_sql_string(value)} SIMILAR TO "
                    f"{_sql_string(str(query_path))} LIMIT 10"
                )

        if selective_category is not None:
            statements.append(
                f"FIND {media_type.upper()} WHERE category = "
                f"{_sql_string(selective_category)} LIMIT 100"
            )
    return statements, counts


def parse_bench_output(
    output: str,
) -> tuple[dict[str, int], list[dict[str, Any]]]:
    build_match = re.search(
        r"queries=(\d+)\s+metadata_index_build_us=(\d+)\s+"
        r"ann_index_build_us=(\d+)\s+ann_index_bytes=(\d+)",
        output,
    )
    if build_match is None:
        raise ValueError("could not parse benchmark build metrics from MMDB output")
    build_metrics = {
        "query_statements": int(build_match.group(1)),
        "metadata_index_build_us": int(build_match.group(2)),
        "ann_index_build_us": int(build_match.group(3)),
        "ann_index_bytes": int(build_match.group(4)),
    }

    rows: list[dict[str, Any]] = []
    for line in output.splitlines():
        fields = line.split()
        if len(fields) != 6 or fields[0] in ("method", "queries="):
            continue
        try:
            p50_us = int(fields[1])
            p95_us = int(fields[2])
            avg_examined = float(fields[3])
            recall_at_10: float | None = (
                None if fields[4] == "-" else float(fields[4])
            )
            runs = int(fields[5])
        except ValueError:
            continue
        rows.append(
            {
                "method": fields[0],
                "p50_us": p50_us,
                "p95_us": p95_us,
                "avg_candidates_examined": avg_examined,
                "recall_at_10": recall_at_10,
                "runs": runs,
            }
        )
    if not rows:
        raise ValueError("MMDB benchmark output contained no method result rows")
    return build_metrics, rows


def parse_stats(output: str) -> dict[str, Any]:
    patterns = {
        "objects": r"^objects: (\d+)$",
        "image_objects": r"^image objects: (\d+)$",
        "audio_objects": r"^audio objects: (\d+)$",
        "video_objects": r"^video objects: (\d+)$",
        "embedding_values": r"^embedding values: (\d+)$",
        "estimated_metadata_bytes": r"^estimated structured metadata bytes: (\d+)$",
        "payload_storage_bytes": r"^payload storage bytes: (\d+)$",
        "catalog_bytes": r"^catalog bytes: (\d+)$",
        "approximate_total_storage_bytes": r"^approximate total storage bytes: (\d+)$",
        "metadata_index_bytes": (
            r"^metadata index: \d+ postings, approximately (\d+) bytes"
        ),
        "metadata_index_build_us": r"^metadata index: .*built in (\d+) us$",
        "lsh_index_vectors": r"^LSH index: (\d+) vectors",
        "lsh_index_buckets": r"^LSH index: \d+ vectors, (\d+) buckets",
        "lsh_index_bytes": r"^LSH index: .*approximately (\d+) bytes",
        "lsh_index_build_us": r"^LSH index: .*built in (\d+) us",
    }
    parsed: dict[str, Any] = {}
    for key, pattern in patterns.items():
        match = re.search(pattern, output, re.MULTILINE)
        if match:
            parsed[key] = int(match.group(1))
    missing = sorted(set(patterns) - set(parsed))
    if missing:
        raise ValueError(
            f"could not parse MMDB stats fields: {', '.join(missing)}"
        )
    parsed["approximate_in_memory_index_bytes"] = (
        parsed["metadata_index_bytes"] + parsed["lsh_index_bytes"]
    )
    return parsed


def _method_rows(
    query_rows: list[dict[str, Any]], method_names: Sequence[str]
) -> list[dict[str, Any]]:
    by_method = {row["method"]: row for row in query_rows}
    labels = {
        "full-scan": "Full scan",
        "indexed": "Indexed",
        "exact": "Exact",
        "ann": "ANN",
        "filter-first": "Filter-first",
        "vector-first": "Vector-first",
    }
    return [
        {"label": labels[name], **by_method[name]}
        for name in method_names
        if name in by_method
    ]


def _load_pyplot() -> Any:
    try:
        import matplotlib

        matplotlib.use("Agg")
        from matplotlib import pyplot
    except ImportError as error:
        raise RuntimeError(
            "Matplotlib is required to create benchmark plots. Install it with "
            "'python3 -m pip install -r benchmark/requirements.txt'."
        ) from error
    return pyplot


def _style_horizontal_axis(axis: Any, xlabel: str) -> None:
    axis.set_xlabel(xlabel)
    axis.set_axisbelow(True)
    axis.grid(axis="x", linestyle="--", linewidth=0.7, alpha=0.35)
    axis.spines["top"].set_visible(False)
    axis.spines["right"].set_visible(False)


def _draw_horizontal_bars(
    axis: Any,
    title: str,
    labels: Sequence[str],
    values: Sequence[float],
    xlabel: str,
    colors: Sequence[str],
    formatter: Any = None,
) -> None:
    positions = list(range(len(labels)))
    bars = axis.barh(positions, values, color=colors, height=0.62)
    axis.set_yticks(positions)
    axis.set_yticklabels(labels)
    axis.invert_yaxis()
    axis.set_title(title, fontsize=11, fontweight="bold")
    _style_horizontal_axis(axis, xlabel)
    maximum = max(values, default=0)
    axis.set_xlim(0, maximum * 1.25 if maximum else 1)
    for bar, value in zip(bars, values):
        label = formatter(value) if formatter else f"{value:,.0f}"
        axis.annotate(
            label,
            xy=(bar.get_width(), bar.get_y() + bar.get_height() / 2),
            xytext=(4, 0),
            textcoords="offset points",
            va="center",
            fontsize=8,
        )


def _save_figure(
    pyplot: Any, figure: Any, output_dir: Path, basename: str
) -> list[Path]:
    path = output_dir / f"{basename}.png"
    figure.savefig(path, bbox_inches="tight", dpi=300)
    pyplot.close(figure)
    return [path]


def _write_latency_chart(
    pyplot: Any, output_dir: Path, query_rows: list[dict[str, Any]]
) -> list[Path]:
    figure, axes = pyplot.subplots(
        1, 2, figsize=(13, 5.3), constrained_layout=True
    )
    groups = [
        ("Metadata-only queries", ("full-scan", "indexed")),
        (
            "Similarity and hybrid queries",
            ("exact", "ann", "filter-first", "vector-first"),
        ),
    ]
    for axis, (title, methods) in zip(axes, groups):
        rows = _method_rows(query_rows, methods)
        labels = [row["label"] for row in rows]
        positions = list(range(len(rows)))
        p50 = [row["p50_us"] for row in rows]
        p95 = [row["p95_us"] for row in rows]
        axis.barh(
            [position - 0.18 for position in positions],
            p50,
            height=0.34,
            label="p50",
            color=PLOT_RED,
        )
        axis.barh(
            [position + 0.18 for position in positions],
            p95,
            height=0.34,
            label="p95",
            color=PLOT_BLUE,
        )
        axis.set_yticks(positions)
        axis.set_yticklabels(labels)
        axis.invert_yaxis()
        axis.set_title(title, fontsize=11, fontweight="bold")
        _style_horizontal_axis(axis, "Latency (us)")
        maximum = max(p50 + p95, default=0)
        axis.set_xlim(0, maximum * 1.3 if maximum else 1)
        axis.legend(frameon=False)
        for values, offsets in ((p50, -0.18), (p95, 0.18)):
            for position, value in zip(positions, values):
                axis.annotate(
                    f"{value:,.0f}",
                    xy=(value, position + offsets),
                    xytext=(4, 0),
                    textcoords="offset points",
                    va="center",
                    fontsize=8,
                )
    figure.suptitle("MMDB query latency", fontsize=15, fontweight="bold")
    figure.text(
        0.5,
        0.01,
        "p50 and p95 latency in microseconds; workloads are separated.",
        ha="center",
        fontsize=9,
        color=PLOT_GRAY,
    )
    return _save_figure(pyplot, figure, output_dir, "latency")


def _write_candidates_chart(
    pyplot: Any, output_dir: Path, query_rows: list[dict[str, Any]]
) -> list[Path]:
    figure, axes = pyplot.subplots(
        1, 2, figsize=(13, 5.3), constrained_layout=True
    )
    groups = [
        ("Metadata-only queries", ("full-scan", "indexed")),
        (
            "Similarity and hybrid queries",
            ("exact", "ann", "filter-first", "vector-first"),
        ),
    ]
    for axis, (title, methods) in zip(axes, groups):
        rows = _method_rows(query_rows, methods)
        _draw_horizontal_bars(
            axis,
            title,
            [row["label"] for row in rows],
            [row["avg_candidates_examined"] for row in rows],
            "Average candidates examined",
            [PLOT_BLUE] * len(rows),
        )
    figure.suptitle("MMDB candidate objects examined", fontsize=15, fontweight="bold")
    return _save_figure(pyplot, figure, output_dir, "candidates_examined")


def _write_recall_chart(
    pyplot: Any, output_dir: Path, query_rows: list[dict[str, Any]]
) -> list[Path]:
    rows = _method_rows(
        query_rows, ("exact", "ann", "filter-first", "vector-first")
    )
    labels = [row["label"] for row in rows]
    values = [
        1.0 if row["method"] == "exact" else row["recall_at_10"] or 0.0
        for row in rows
    ]
    colors = [PLOT_GRAY if row["method"] == "exact" else PLOT_RED for row in rows]
    figure, axis = pyplot.subplots(figsize=(8, 4.8), constrained_layout=True)
    _draw_horizontal_bars(
        axis,
        "Similarity-search Recall@10",
        labels,
        values,
        "Recall@10",
        colors,
        formatter=lambda value: f"{value:.2f}",
    )
    axis.set_xlim(0, 1.12)
    axis.set_xticks([tick / 5 for tick in range(6)])
    figure.suptitle("Recall against exact top-10 results", fontsize=15, fontweight="bold")
    return _save_figure(pyplot, figure, output_dir, "recall_at_10")


def _write_index_storage_chart(
    pyplot: Any, output_dir: Path, stats: dict[str, Any]
) -> list[Path]:
    figure, axes = pyplot.subplots(
        1, 3, figsize=(14, 4.8), constrained_layout=True
    )
    panels = [
        (
            "Index size (bytes)",
            ["Metadata", "ANN / LSH"],
            [stats["metadata_index_bytes"], stats["lsh_index_bytes"]],
        ),
        (
            "Index build time (us)",
            ["Metadata", "ANN / LSH"],
            [stats["metadata_index_build_us"], stats["lsh_index_build_us"]],
        ),
        (
            "Storage size (bytes)",
            ["Payloads", "Catalog"],
            [stats["payload_storage_bytes"], stats["catalog_bytes"]],
        ),
    ]
    for axis, (title, labels, values) in zip(axes, panels):
        _draw_horizontal_bars(
            axis,
            title,
            labels,
            values,
            title,
            [PLOT_RED, PLOT_BLUE],
        )
    figure.suptitle(
        "MMDB index and storage metrics\nApproximate in-memory index total: "
        f"{stats['approximate_in_memory_index_bytes']:,} bytes",
        fontsize=14,
        fontweight="bold",
    )
    return _save_figure(pyplot, figure, output_dir, "index_and_storage")


def write_graphs(
    output_dir: Path, query_rows: list[dict[str, Any]], stats: dict[str, Any]
) -> list[Path]:
    output_dir.mkdir(parents=True, exist_ok=True)
    pyplot = _load_pyplot()
    paths: list[Path] = []
    paths.extend(_write_latency_chart(pyplot, output_dir, query_rows))
    paths.extend(_write_recall_chart(pyplot, output_dir, query_rows))
    paths.extend(_write_candidates_chart(pyplot, output_dir, query_rows))
    paths.extend(_write_index_storage_chart(pyplot, output_dir, stats))
    return paths


def write_results(
    output_dir: Path,
    database: Path,
    seed: int,
    sample_counts: dict[str, int],
    build_metrics: dict[str, int],
    query_rows: list[dict[str, Any]],
    stats: dict[str, Any],
    raw_bench: str,
    raw_stats: str,
    graph_paths: list[Path],
) -> tuple[Path, Path]:
    output_dir.mkdir(parents=True, exist_ok=True)
    csv_path = output_dir / "benchmark_results.csv"
    json_path = output_dir / "benchmark_results.json"
    with csv_path.open("w", encoding="utf-8", newline="") as csv_file:
        fields = [
            "method",
            "p50_us",
            "p95_us",
            "avg_candidates_examined",
            "recall_at_10",
            "runs",
            "index_size_bytes",
            "index_build_time_us",
            "storage_bytes",
            "approximate_memory_bytes",
        ]
        writer = csv.DictWriter(csv_file, fieldnames=fields)
        writer.writeheader()
        writer.writerows(query_rows)
    report = {
        "database": str(database),
        "seed": seed,
        "sample_queries_per_modality": sample_counts,
        "build_metrics": build_metrics,
        "stats": stats,
        "methods": query_rows,
        "graphs": [path.name for path in graph_paths],
        "raw_mmdb_output": {"bench": raw_bench, "stats": raw_stats},
    }
    with json_path.open("w", encoding="utf-8") as json_file:
        json.dump(report, json_file, indent=2)
        json_file.write("\n")
    return csv_path, json_path


def run_benchmark(
    db_dir: Path,
    output_dir: Path,
    queries_per_modality: int,
    seed: int,
) -> int:
    db_dir = db_dir.resolve()
    catalog = load_catalog(db_dir)
    statements, sample_counts = build_queries(
        catalog, db_dir, queries_per_modality, seed
    )
    if not statements:
        raise ValueError("no queryable media payloads found in the MMDB catalog")
    binary = ensure_binary()
    raw_bench = run_mmdb(binary, db_dir, ["bench"] + statements)
    build_metrics, query_rows = parse_bench_output(raw_bench)
    raw_stats = run_mmdb(binary, db_dir, ["stats"])
    stats = parse_stats(raw_stats)
    index_size_bytes = stats["metadata_index_bytes"] + stats["lsh_index_bytes"]
    index_build_time_us = (
        stats["metadata_index_build_us"] + stats["lsh_index_build_us"]
    )
    for row in query_rows:
        row["index_size_bytes"] = index_size_bytes
        row["index_build_time_us"] = index_build_time_us
        row["storage_bytes"] = stats["approximate_total_storage_bytes"]
        row["approximate_memory_bytes"] = stats["approximate_in_memory_index_bytes"]
    graph_paths = write_graphs(output_dir, query_rows, stats)
    csv_path, json_path = write_results(
        output_dir,
        db_dir,
        seed,
        sample_counts,
        build_metrics,
        query_rows,
        stats,
        raw_bench,
        raw_stats,
        graph_paths,
    )

    print(
        f"{'method':<15} {'p50_us':>9} {'p95_us':>9} "
        f"{'avg_examined':>14} {'Recall@10':>12} {'runs':>8} "
        f"{'index_bytes':>13} {'build_us':>12}"
    )
    for row in query_rows:
        recall = (
            "-"
            if row["recall_at_10"] is None
            else f"{row['recall_at_10']:.3f}"
        )
        print(
            f"{row['method']:<15} {row['p50_us']:>9} {row['p95_us']:>9} "
            f"{row['avg_candidates_examined']:>14.1f} {recall:>12} "
            f"{row['runs']:>8} {row['index_size_bytes']:>13} "
            f"{row['index_build_time_us']:>12}"
        )
    print(
        f"Index build: metadata={stats['metadata_index_build_us']} us, "
        f"ANN={stats['lsh_index_build_us']} us; index bytes="
        f"{stats['metadata_index_bytes'] + stats['lsh_index_bytes']} "
        f"(estimated in-memory indexes="
        f"{stats['approximate_in_memory_index_bytes']})"
    )
    print(
        f"Storage: payload={stats['payload_storage_bytes']} bytes, "
        f"catalog={stats['catalog_bytes']} bytes, "
        f"total={stats['approximate_total_storage_bytes']} bytes"
    )
    print(f"CSV: {csv_path}")
    print(f"JSON: {json_path}")
    print("Report plots (PNG):")
    for path in graph_paths:
        print(f"  {path}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--db",
        type=Path,
        default=DEFAULT_DB_DIR,
        help="MMDB database directory (default: %(default)s)",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=DEFAULT_OUTPUT_DIR,
        help="directory for CSV/JSON results (default: %(default)s)",
    )
    parser.add_argument(
        "--queries-per-modality",
        type=_positive_int,
        default=5,
        help="number of similarity query files sampled per modality",
    )
    parser.add_argument("--seed", type=int, default=0, help="deterministic sample seed")
    args = parser.parse_args()
    return run_benchmark(
        args.db, args.output_dir, args.queries_per_modality, args.seed
    )


if __name__ == "__main__":
    sys.exit(main())
