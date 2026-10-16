# MMDB Implementation Plan

## 1. Purpose and scope

This plan describes delivery of the Rust multimodal database documented in
[README.md](./README.md). MMDB stores image and audio files, catalogs their
metadata, and supports indexed filtering, similarity retrieval, hybrid queries,
and command-line evaluation.

Video, a graphical interface, learned semantic embeddings, distributed
deployment, and production-scale operation are out of scope. MMDB computes
lightweight fixed-size image-color and audio-signal descriptors itself; these
are not semantic model embeddings. The image and audio codec libraries decode
the supported media formats.

### Delivered behavior

- Persist the JSON catalog and copied media payloads under the selected database
  root.
- Ingest image and audio files with user attributes. Parse valid JSON attribute
  values as typed values and store other values as strings. Treat `timestamp` as
  the RFC 3339 capture-time attribute.
- Retrieve records by ID and query `IMAGE`, `AUDIO`, or `OBJECT` records using
  supported metadata predicates and same-modality similarity queries.
- Provide `scan`, `indexed`, `exact`, `ann`, `filter-first`, and `vector-first`
  plans, plus `auto` plan selection.
- Offer one-shot CLI commands and an interactive prompt, and report storage,
  index, latency, candidates-examined, and Recall@10 measurements.

The LSH index is in-memory and rebuilt on process startup; the catalog and
payloads persist across runs.

## 2. Implementation contract

### Storage and ingestion

- Use the Rust stable toolchain.
- Store the catalog at `DIR/data/catalog.json` and payloads at
  `DIR/data/objects/`; `--db DIR` selects `DIR`, with the current directory as
  the default database root.
- Support the formats decoded by the configured `image` and Symphonia
  dependencies, as documented in the README.
- Keep object metadata, payload location, and computed modality-specific feature
  vector consistent when ingesting and reopening a database.

### Query contract

Supported statements:

```text
FIND IMAGE|AUDIO|OBJECT
[WHERE field (=|!=|<|<=|>|>=) value [AND ...]]
[SIMILAR TO 'query-media-path']
[LIMIT positive-integer]

GET object-id
```

`OBJECT` searches both supported modalities. A similarity query must use a
query file with the same modality as the searched media. Query paths may be
absolute, relative to the working directory, or relative to the database root.
Only the operators shown above are in scope; `CONTAINS`, `BETWEEN`, video
queries, and similarity from a raw vector are not part of this contract.

### Execution and evaluation contract

- `auto` chooses indexed execution for metadata-only queries, ANN for
  similarity-only queries, and filter-first for hybrid queries.
- Metadata evaluation compares a full scan with the hash-indexed plan;
  point lookups use the object-ID hash index.
- Similarity evaluation compares exact, ANN, filter-first, and vector-first
  plans. Report latency, candidates examined, and Recall@10 against exact
  search.
- `stats` reports object, embedding, payload, catalog, and estimated in-memory
  index sizes, as well as index-build times.

## 3. Work allocation

For a two-person team, use the following ownership as a starting point and
review the shared interfaces at each gate:

- **Person A — storage and query:** catalog schema, ingestion, persistence,
  parser, metadata filtering, ID retrieval, CLI integration.
- **Person B — features and retrieval:** feature extraction, vector storage,
  exact and ANN search, hybrid execution, benchmark metrics.
- **Shared:** agree on record and query contracts, integrate end-to-end behavior,
  review test results, and confirm each phase's acceptance criteria.

The work may proceed in parallel only where the dependency gates below allow
it.

## 4. Timeline, dependencies, and phase gates

Timeline is a five-week estimate relative to project start; adjust calendar
dates to the course schedule without changing the dependency order. A phase is
complete only when its acceptance criteria pass. Downstream work must not treat
an unfinished phase as a stable interface.

| Phase | Target | Dependencies | Lead | Deliverables |
|---|---|---|---|---|
| 1. Contract and project baseline | Week 1 | None | Shared | Rust project baseline; agreed object, attribute, query, and result contracts; representative fixtures |
| 2. Persistent catalog and ingest | Weeks 1–2 | Phase 1 | Person A; Person B agrees feature-vector interface | Image/audio ingest; typed attributes; payload and catalog persistence; ID lookup |
| 3. Metadata queries and parser | Weeks 2–3 | Phase 1; catalog record interface from Phase 2 | Person A | FIND/GET parsing; supported predicates; scan and indexed metadata execution |
| 4. Feature extraction and similarity | Weeks 2–4 | Phase 1; ingest interface from Phase 2 | Person B | Modality-specific descriptors; exact and ANN retrieval; same-modality validation |
| 5. Hybrid execution and plan selection | Week 4 | Phases 3 and 4 | Shared; Person B leads retrieval integration | Filter-first and vector-first plans; `auto`; top-K ordering |
| 6. CLI, stats, benchmarks, and release check | Week 5 | Phases 2–5 | Shared | One-shot and interactive CLI; stats; benchmark output; end-to-end verification |

### Phase 1 — Contract and project baseline (Week 1)

**Work**

- Confirm that image and audio are the only supported modalities.
- Agree on the catalog record, attribute typing, timestamp behavior, feature
  vector interfaces, result ordering, and supported query grammar above.
- Establish representative image/audio fixtures and a small repeatable query
  set; identify expected results for correctness and Recall@10 checks.
- Confirm the Rust stable build and command invocation conventions.

**Acceptance gate to Phase 2:** the project builds with the selected toolchain;
the object and query contracts are written down and shared by both workstreams;
the fixture set and expected metadata-query and retrieval results are available.

### Phase 2 — Persistent catalog and ingest (Weeks 1–2)

**Work**

- Ingest supported image and audio files and capture user-supplied attributes.
- Interpret valid JSON attribute values as typed values; preserve non-JSON values
  as strings; parse the special timestamp attribute as RFC 3339 capture time.
- Copy media payloads into the database object directory and persist catalog
  entries under the selected database root.
- Implement lookup by object ID and reopen the catalog on a later process run.
- Agree and implement the interface by which the modality-specific feature
  vector is associated with each object.

**Acceptance gate to Phases 3 and 4:** ingest at least one fixture of each
modality; verify IDs, modality, attributes, timestamp, payload, and feature
association; restart the process and confirm catalog and payload persistence;
confirm `GET` returns the ingested record and invalid paths or unsupported
media produce an explicit error.

### Phase 3 — Metadata queries and parser (Weeks 2–3)

**Work**

- Parse `FIND IMAGE`, `FIND AUDIO`, `FIND OBJECT`, `GET`, optional `WHERE`,
  `SIMILAR TO`, and positive `LIMIT` clauses according to the contract.
- Evaluate `=`, `!=`, `<`, `<=`, `>`, and `>=`, including `AND`-combined
  predicates and typed metadata values.
- Implement scan and hash-indexed metadata plans and indexed object-ID lookup.
- Reject malformed statements, unsupported operators, and invalid limits
  explicitly.

**Acceptance gate to Phase 5:** parser and execution tests pass for each supported
operator, `AND`, media-type restriction, `OBJECT`, `GET`, and `LIMIT`; scan and
indexed plans return the same records in the same order for the fixture queries.

### Phase 4 — Feature extraction and similarity (Weeks 2–4)

**Work**

- Decode supported image/audio inputs using the selected codec libraries and
  compute the documented lightweight fixed-size descriptors.
- Implement exact similarity and the in-memory LSH/ANN path for each modality.
- Validate query-file existence and same-modality requirements; support the
  documented absolute, working-directory-relative, and database-root-relative
  path resolution.
- Rebuild the in-memory ANN index from persisted records at process startup.

**Acceptance gate to Phase 5:** image and audio similarity queries return
deterministic top-K results on the fixture set; cross-modality and unreadable
query files fail clearly; the index rebuilds after restart; ANN reports
Recall@10 against exact search and candidates examined.

### Phase 5 — Hybrid execution and plan selection (Week 4)

**Work**

- Combine metadata filters and similarity rankings in filter-first and
  vector-first execution.
- Implement `auto` selection: indexed for metadata-only, ANN for
  similarity-only, and filter-first for hybrid queries.
- Apply result limits after the plan's required filtering and ranking while
  preserving the agreed deterministic ordering.
- Expose explicit plan selection for `indexed`, `scan`, `exact`, `ann`,
  `filter-first`, and `vector-first`.

**Acceptance gate to Phase 6:** each explicit plan and `auto` produce correct
results for metadata-only, similarity-only, and hybrid fixtures; hybrid results
obey both metadata predicates and the top-K limit; filter-first and
vector-first results can be compared with exact-search ground truth.

### Phase 6 — CLI, stats, benchmarks, and release check (Week 5)

**Work**

- Verify `init`, `ingest`, `stats`, `query`, `get`, and `bench` command flows,
  including `--db` selection and the interactive FIND/GET prompt.
- Report the documented storage/index sizes and index build times through
  `stats`.
- Benchmark point lookups, metadata scan versus indexed plans, and exact, ANN,
  filter-first, and vector-first similarity workloads. Include latency,
  candidates examined, and Recall@10 where applicable.
- Run the README examples on the fixture dataset, check error paths, and update
  the README if implemented behavior differs from its documented contract.

**Release acceptance gate:** `cargo build --release` succeeds; README command
examples work with representative image and audio files; catalog and payload
data persist across runs; supported query forms and plan choices return
contract-correct results; benchmark and stats output include the stated
measurements; the README and implementation agree on scope and limitations.

## 5. Verification matrix

| Capability | Minimum verification |
|---|---|
| Ingest and persistence | Ingest image/audio, inspect attributes and payload locations, restart, and retrieve records |
| Metadata and ID lookup | Exercise each supported comparison, `AND`, `OBJECT`, `GET`, scan, and indexed results |
| Similarity | Run image and audio exact/ANN queries; verify modality, path resolution, top-K, Recall@10, and candidates |
| Hybrid retrieval | Compare filter-first and vector-first with exact ground truth; verify predicates and top-K |
| CLI and observability | Run one-shot and interactive commands; verify `stats` and benchmark fields |
| Error handling | Check malformed queries, invalid limits, missing files, unsupported media, and modality mismatch |

## 6. Risks and mitigations

| Risk | Mitigation |
|---|---|
| Query grammar or attribute typing differs between parser and executor | Freeze the contract in Phase 1 and use shared fixtures at the Phase 3 gate |
| Retrieval tests become misleading without ground truth | Establish relevant fixture results in Phase 1 and measure ANN Recall@10 against exact search |
| In-memory ANN index is confused with persistent storage | Rebuild it on startup from persisted catalog/features and verify restart behavior |
| Codec support differs by enabled dependency features | Test representative files for every advertised format; document any actual dependency limitation |
| Integration slips late in the schedule | Require phase gates and integrate the two workstreams during Weeks 2–4 rather than postponing all integration to Week 5 |

## 7. Completion criteria

The project is complete when the Phase 6 release gate passes and the
implementation matches [README.md](./README.md): image/audio ingestion and
persistence, typed metadata queries and ID lookup, modality-appropriate
similarity and hybrid retrieval, all documented plans and CLI entry points, and
the documented stats and evaluation measurements. No video or learned semantic
embedding support is required for completion.
