# MMDB

A small Rust multimodal database engine for **image, audio, and video files**. It stores media objects in a catalog, indexes metadata for filtering, builds lightweight feature vectors for similarity search, and exposes a CLI for ingesting and querying the collection.

## Build and run

Install the Rust stable toolchain, then run commands from this directory:

~~~text
cargo build --release
cargo run --release -- init
cargo run --release -- ingest ./media/drone-01.jpg camera=drone timestamp='2026-02-18T10:30:00Z'
cargo run --release -- ingest ./media/field-recording.wav speaker=unknown
cargo run --release -- ingest ./media/clip.mp4 camera=drone duration_seconds=12.5
cargo run --release -- stats
~~~

The --db DIR option selects the database root. By default, MMDB uses the current directory. It stores its catalog at DIR/data/catalog.json and copies payloads to DIR/data/objects/.

The image crate decodes BMP, GIF, JPEG, PNG, TIFF, and WebP. Symphonia decodes audio formats enabled by its all feature, including WAV, MP3, FLAC, OGG/Opus, AAC/M4A, and AIFF. Video support is implemented with a lightweight byte- and frame-histogram approach for common extension-based video inputs such as MP4, MOV, AVI, MKV, WEBM, WMV, FLV, MPEG, and MPG. MMDB computes small fixed-size feature vectors itself; the project is not using a learned embedding model.

## Object model

Each catalog object includes:

- object_id
- media_type
- physical_location
- size
- timestamp
- metadata
- embedding
- segment_embeddings

Segment embeddings are stored as a list of per-segment feature vectors for richer object-level metadata and future granular retrieval work. They are empty for image and audio objects unless a media source provides them.

## Ingest and query

Ingested fields are typed when the value is valid JSON; otherwise they are stored as strings. For example, camera=drone is a string and rating=4 is a number. The special timestamp attribute sets the catalog's RFC 3339 capture time.

~~~text
cargo run --release -- ingest ./media/drone-01.jpg camera=drone location=roof
cargo run --release -- query "FIND IMAGE WHERE camera = 'drone' AND timestamp >= '2026-01-01' LIMIT 20"
cargo run --release -- query "FIND AUDIO WHERE duration < 30 LIMIT 10"
cargo run --release -- query "FIND VIDEO WHERE duration_seconds < 60 LIMIT 10"
cargo run --release -- query "FIND IMAGE SIMILAR TO './media/query.jpg' LIMIT 10" --plan exact
cargo run --release -- query "FIND AUDIO SIMILAR TO './media/query.wav' LIMIT 10" --plan ann
cargo run --release -- query "FIND VIDEO SIMILAR TO './media/query.mp4' LIMIT 10" --plan ann
cargo run --release -- query "FIND IMAGE WHERE camera = 'drone' SIMILAR TO './media/query.jpg' LIMIT 10" --plan vector-first
cargo run --release -- get 1
~~~

The query parser accepts:

~~~text
FIND IMAGE|AUDIO|VIDEO|OBJECT
[WHERE field (=|!=|<|<=|>|>=|CONTAINS|BETWEEN) value [AND ...]]
[SIMILAR TO 'query-media-path']
[LIMIT positive-integer]

GET object-id
~~~

`CONTAINS` matches substring values in string fields, and `BETWEEN low AND high` evaluates inclusive numeric or timestamp ranges.

OBJECT searches all supported media types. A similarity query must use a query file with the same modality. Query paths may be absolute, relative to the current directory, or relative to the database root.

Use --plan to select indexed, scan, exact, ann, filter-first, or vector-first. auto uses an indexed query for metadata-only queries, ANN for similarity-only queries, and filter-first for hybrid queries. The interactive prompt supports the same FIND and GET statements:

~~~text
cargo run --release
db> FIND IMAGE WHERE camera = 'drone' LIMIT 10
db> FIND VIDEO WHERE duration_seconds < 60 LIMIT 5
db> GET 1
db> exit
~~~

## Evaluation

Compare a group of queries with bench; quote each query as a separate argument. Point lookups use the object-ID hash index. Similarity workloads report exact, ANN, filter-first, and vector-first latency, candidates examined, and Recall@10 relative to exact search. Metadata workloads compare a full scan with the hash-indexed plan.

~~~text
cargo run --release -- bench "GET 1" "GET 2" "FIND IMAGE WHERE camera = 'drone' LIMIT 100" "FIND IMAGE SIMILAR TO './queries/image-1.jpg' LIMIT 10" "FIND VIDEO WHERE duration_seconds < 60 LIMIT 10" "FIND AUDIO WHERE speaker = 'unknown' SIMILAR TO './queries/audio-1.wav' LIMIT 10"
~~~

stats reports the object, embedding, payload, catalog, and estimated in-memory index sizes, plus index build times.

## Design and limits

See [DESIGN.md](DESIGN.md) for the storage model, index and execution choices, feature-vector definitions, and tradeoffs. The vectors are lightweight image-color, audio-signal, and coarse video-descriptor features, not learned semantic embeddings: similarity is most useful for visually or acoustically similar media in the same modality. The LSH index is rebuilt in memory when the process starts; payload files and the JSON catalog persist across runs.
