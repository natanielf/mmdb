# MMDB

A small Rust multimodal database engine for **image and audio files**. Video is intentionally unsupported. It focuses on media storage, a metadata catalog, traditional indexing, vector retrieval, query execution, and performance comparison rather than a graphical application.

## Build and run

Install the Rust stable toolchain, then run commands from this directory:

~~~text
cargo build --release
cargo run --release -- init
cargo run --release -- ingest C:\media\drone-01.jpg camera=drone timestamp='2026-02-18T10:30:00Z'
cargo run --release -- ingest C:\media\field-recording.wav speaker=unknown
cargo run --release -- stats
~~~

The --db DIR option selects the database root. By default, MMDB uses the current directory. It stores its catalog at DIR/data/catalog.json and copies payloads to DIR/data/objects/.

The image crate decodes BMP, GIF, JPEG, PNG, TIFF, and WebP. Symphonia decodes audio formats enabled by its all feature, including WAV, MP3, FLAC, OGG/Opus, AAC/M4A, and AIFF. The codec libraries do the file decoding; MMDB computes small fixed-size feature vectors itself. See the upstream [image documentation](https://docs.rs/image/0.25.10/image/) and [Symphonia 0.5.5 documentation](https://docs.rs/crate/symphonia/0.5.5).

## Ingest and query

Ingested fields are typed when the value is valid JSON; otherwise they are stored as strings. For example, camera=drone is a string and rating=4 is a number. The special timestamp attribute sets the catalog's RFC 3339 capture time.

~~~text
cargo run --release -- ingest C:\media\drone-01.jpg camera=drone location=roof
cargo run --release -- query "FIND IMAGE WHERE camera = 'drone' AND timestamp >= '2026-01-01' LIMIT 20"
cargo run --release -- query "FIND AUDIO WHERE duration < 30 LIMIT 10"
cargo run --release -- query "FIND IMAGE SIMILAR TO 'C:\media\query.jpg' LIMIT 10" --plan exact
cargo run --release -- query "FIND AUDIO SIMILAR TO 'C:\media\query.wav' LIMIT 10" --plan ann
cargo run --release -- query "FIND IMAGE WHERE camera = 'drone' SIMILAR TO 'C:\media\query.jpg' LIMIT 10" --plan vector-first
cargo run --release -- get 1
~~~

The query parser accepts:

~~~text
FIND IMAGE|AUDIO|OBJECT
[WHERE field (=|!=|<|<=|>|>=|CONTAINS|BETWEEN) value [AND ...]]
[SIMILAR TO 'query-media-path']
[LIMIT positive-integer]

GET object-id
~~~

`CONTAINS` matches substring values in string fields, and `BETWEEN low AND high` evaluates inclusive numeric or timestamp ranges.

OBJECT searches both supported media types. A similarity query must use a query file with the same modality. Query paths may be absolute, relative to the current directory, or relative to the database root.

Use --plan to select indexed, scan, exact, ann, filter-first, or vector-first. auto uses an indexed query for metadata-only queries, ANN for similarity-only queries, and filter-first for hybrid queries. The interactive prompt supports the same FIND and GET statements:

~~~text
cargo run --release
db> FIND IMAGE WHERE camera = 'drone' LIMIT 10
db> GET 1
db> exit
~~~

## Evaluation

Compare a group of queries with bench; quote each query as a separate argument. Point lookups use the object-ID hash index. Similarity workloads report exact, ANN, filter-first, and vector-first latency, candidates examined, and Recall@10 relative to exact search. Metadata workloads compare a full scan with the hash-indexed plan.

~~~text
cargo run --release -- bench "GET 1" "GET 2" "FIND IMAGE WHERE camera = 'drone' LIMIT 100" "FIND IMAGE SIMILAR TO 'C:\queries\image-1.jpg' LIMIT 10" "FIND IMAGE SIMILAR TO 'C:\queries\image-2.jpg' LIMIT 10" "FIND AUDIO WHERE speaker = 'unknown' SIMILAR TO 'C:\queries\audio-1.wav' LIMIT 10"
~~~

stats reports the object, embedding, payload, catalog, and estimated in-memory index sizes, plus index build times.

## Design and limits

See [DESIGN.md](DESIGN.md) for the storage model, index and execution choices, feature-vector definitions, and tradeoffs. The vectors are lightweight image-color and audio-signal descriptors, not learned semantic embeddings: similarity is most useful for visually or acoustically similar media in the same modality. The LSH index is rebuilt in memory when the process starts; payload files and the JSON catalog persist across runs.
