# MMDB system design

## Goals and scope

MMDB is a local database access layer for image and audio collections. It implements a persistent media catalog, structured metadata queries, exact vector search, approximate vector search, and query-plan comparisons. It has no video modality; the parser and ingestion path accept only images, audio, and object-ID lookup.

The design keeps the educational database mechanisms visible in Rust. Image and audio codecs come from existing crates, while the metadata hash index and approximate-neighbor structure are implemented in this project.

## Architecture

~~~mermaid
flowchart TD
    CLI[CLI and FIND parser] --> Planner[Query planner]
    Planner --> MIndex[Metadata hash index]
    Planner --> Exact[Exact cosine scan]
    Planner --> LSH[Random-projection LSH]
    MIndex --> Catalog[JSON media catalog]
    Exact --> Catalog
    LSH --> Catalog
    Catalog --> Payload[Image/audio files in data/objects]
    Ingest[Ingest and feature extraction] --> Catalog
    Ingest --> Payload
~~~

At process startup, MMDB loads data/catalog.json and reconstructs its indexes. Media payloads are copied to data/objects with an object-ID filename. A catalog record stores the relative path, byte size, UTC timestamp, modality, structured attributes, and the fixed-size vector.

The record has the following logical shape:

~~~text
object_id: unsigned integer
media_type: image | audio
physical_location: relative path
size: bytes
timestamp: RFC 3339 UTC string
metadata: JSON scalar map
embedding: array of 32 floating-point values
~~~

Image records add format, width, height, channels, and source name. Audio records add format, duration_seconds, sample_rate, channels, and source name. duration is also accepted as a query alias for duration_seconds. Ingest accepts extra key=value attributes; values that parse as JSON keep their number or boolean type, while other values are strings.

## Storage and metadata access

Catalog records and vectors are stored together in a human-readable JSON file. The original bytes stay in separate payload files, so metadata scans and similarity search do not need to read full media files. This file-based storage keeps setup simple and makes each record's physical location explicit. Each ingest rewrites the catalog; this is appropriate for a course-scale collection, but it is not a transaction log or a large-scale page store.

The traditional index is a secondary hash index built at startup. Its value directory uses FNV-1a hashing, separate chaining for collision handling, and bucket growth above a 0.75 load factor. It maps a field and a typed JSON value to record positions. MMDB indexes the mandatory fields and every metadata attribute. This makes equality predicates such as camera = 'drone' and object-ID lookup direct. Range predicates such as duration < 30 and timestamp >= '2026-01-01' are evaluated while scanning either all records or the equality-index candidate list. The hash index does not accelerate ranges.

There is no application-level payload cache. Ingestion computes and persists the feature vector once, so catalog and vector queries do not reopen stored payload files. A similarity query decodes its query file and computes a query vector for each request; reported query latency includes that work. The operating system may still cache filesystem reads.

## Feature vectors and similarity

MMDB stores one 32-dimensional vector per media object:

- Image: resize to 128 by 128, then concatenate 8-bin normalized histograms for red, green, blue, and luminance.
- Audio: decode and downsample to about 4,000 mono samples per second, retain at most 65,536 time-stamped samples using deterministic reservoir sampling across the recording, then concatenate 16 time-segment RMS-energy values and 16 segment zero-crossing rates.

Both features are inexpensive to compute and have a fixed dimension. They serve as a reproducible baseline, not learned semantic embeddings. Similarity is restricted to the query's modality because color histograms and acoustic statistics do not share a meaningful cross-modal distance. The distance score is cosine similarity.

The indexing unit is one whole source file, including long-form audio. Each audio file has one catalog object and one global embedding, rather than a vector for every fixed-length window. This keeps catalog and index size low and makes point lookup simple; it also means a query cannot identify a particular moment inside a recording, and whole-recording features can smooth out short sounds.

The exact vector path computes cosine similarity against each eligible vector and sorts by score. This is the reference result set for recall measurements. Its work grows linearly with the eligible vector count.

## Approximate vector index

The ANN baseline is random-hyperplane locality-sensitive hashing. MMDB maintains a separate index for images and audio. Each modality uses 8 hash tables with 12 deterministic random projections per table. A vector's sign against each projection produces a 12-bit bucket key.

For a query, MMDB probes exact and nearby buckets, then rescales the candidate vectors with cosine similarity. If the near buckets do not produce enough candidates, it expands to occupied buckets in increasing Hamming distance until it reaches the candidate target or exhausts that modality's buckets. The candidate target is max(12 x LIMIT, 64), capped by the selected modality's vector count. This trades some recall for fewer vector comparisons on larger collections. Small collections can return exact results because most or all vectors are inspected.

The projection seed and configuration are fixed, which makes index construction repeatable. Indexes remain in memory and are rebuilt from catalog vectors at startup. The stats and bench commands report build time and an approximate index-size estimate.

## Query language and execution

The hand-written parser supports:

~~~text
FIND IMAGE|AUDIO|OBJECT
[WHERE field (=|!=|<|<=|>|>=) value [AND ...]]
[SIMILAR TO 'query-file']
[LIMIT k]

GET object-id
~~~

Metadata comparisons use typed numeric comparison for numbers and lexicographic comparison for strings. ISO-style timestamps therefore sort chronologically when their representation is consistent. Predicates are combined with AND.

For a metadata-only request, the planner selects the smallest available equality-index posting list and evaluates remaining predicates on those records. If there is no usable equality predicate, the indexed plan still applies the modality index; scan bypasses the hash index and evaluates the entire catalog.

For a hybrid metadata and similarity request, two orders are available:

1. Filter-first: use the metadata index to reduce the records, evaluate remaining predicates, and run exact cosine ranking over that set.
2. Vector-first: use LSH to generate a candidate set, then apply metadata predicates to those candidates and rank the survivors.

The first plan is useful when metadata is selective. The second is useful when the ANN candidate set is small. The exact plan is the accuracy reference, and the ANN plan performs approximate retrieval without an additional metadata predicate. Auto selects indexed for metadata-only, ANN for similarity-only, and filter-first for hybrid requests.

## Evaluation support

Run stats to report object and vector counts, modality counts, payload and catalog bytes, estimated metadata and LSH index sizes, and index build times.

Run bench with multiple GET and FIND statements to report p50/p95 query latency and average candidates examined. GET uses the object-ID hash posting and then fetches the catalog row. Metadata-only queries compare a full scan with the equality-index plan. Similarity queries compare exact, ANN, filter-first, and vector-first. Recall@10 is measured against the exact top-k IDs for each query.

For representative measurements, use several query files per modality and include both selective and broad metadata predicates. The expected trends are:

- Equality indexing examines fewer records than a full metadata scan when the selected value is uncommon.
- Filter-first benefits from a selective equality predicate because fewer vectors need exact scoring.
- Vector-first can be faster when LSH returns a small candidate set, but filtering can remove relevant neighbors and reduce recall.
- Exact scoring cost grows with the number of eligible vectors; ANN trades that work for index-build and memory overhead.

## Tradeoffs

The JSON catalog and filesystem payloads are easy to inspect, copy, and reset, but saving the catalog rewrites the whole file. Metadata and ANN indexes are volatile and must be rebuilt on each run. The CLI is append-only; it has no update or delete operation, so it avoids the additional work of keeping postings and ANN buckets consistent with changed records. These choices prioritize clarity and reproducibility over write throughput, concurrent writers, transactions, and very large catalogs.

The custom hash index implements equality lookup; range queries use scans. The custom LSH index keeps its mechanism visible and avoids an external ANN dependency, but it needs measured tuning as collection size and vector distribution change. The image and audio descriptors are intentionally simple; a semantic embedding model could improve relevance but would add a larger runtime model and hardware dependency.
