# MMDB Final Project Plan

This document translates the project requirements in the final project PDF into an implementation plan for the minimal multimodal database (MMDB) system.

## 1. Project Overview

The project is a minimal multimodal database designed to store and query different media types such as images, audio, and video. The system must support both structured metadata filtering and similarity-based retrieval using embeddings.

The core concept is a single object catalog with metadata and a retrieval engine that can answer questions such as:

- "Find all drone images"
- "Find audio clips shorter than 30 seconds"
- "Find visually similar images to a provided reference"
- "Retrieve a specific media item by object ID"
- "Combine metadata filters with similarity search and return top K results"

---

## 2. Project Goals

### Primary goals

1. Store multimodal media records in a structured catalog.
2. Support metadata-driven filtering with SQL-like predicates.
3. Support semantic similarity retrieval using embeddings.
4. Support retrieval by object ID.
5. Return ranked results with configurable limits.
6. Provide a simple command-line interface consistent with the project examples.

### Secondary goals

- Support object-level metadata and embedding storage.
- Enable multi-modal indexing and retrieval across image, audio, and video objects.
- Maintain a clear retrieval pipeline that combines filtering and similarity logic.
- Define evaluation criteria for retrieval quality.

---

## 3. Functional Requirements

### 3.1 Data model

Each stored item should include at least the following fields:

- `object_id`: unique identifier
- `media_type`: image, audio, or video
- `physical_location`: file path or storage location
- `size`: size of the media object
- `timestamp`: creation or ingestion timestamp
- `structured_metadata`: additional metadata properties
- `embedding`: global semantic representation for the object
- `segment_embeddings`: one or more sub-region or segment embeddings for richer retrieval

The system should support metadata fields such as:

- `camera`
- `duration`
- `speaker`
- `tag`
- custom user-defined attributes

### 3.2 Required object behavior

The database must be able to:

- ingest a new media object
- store its metadata and embedding(s)
- retrieve it directly by `object_id`
- match it against structured conditions
- rank it against similarity queries

A single object can have:

- one catalog entry
- one global embedding
- multiple segment embeddings

This supports both general object-level retrieval and specific substructure understanding.

---

## 4. Query Language Requirements

The project requirements define a minimal query grammar.

### 4.1 Core grammar

```text
FIND <media-type>
[WHERE <structured-predicate>]
[SIMILAR TO <query-object-or-vector>]
[LIMIT <k>]
```

Examples from the PDF include:

```text
FIND IMAGE
WHERE camera = 'drone'
LIMIT 20
```

```text
FIND AUDIO
WHERE duration < 30
LIMIT 10
```

```text
FIND IMAGE
SIMILAR TO 'query.jpg'
LIMIT 10
```

```text
FIND VIDEO
WHERE duration < 60
SIMILAR TO 'example.mp4'
LIMIT 20
```

```text
FIND AUDIO
WHERE speaker = 'unknown'
AND timestamp >= '2026-01-01'
SIMILAR TO 'sample.wav'
LIMIT 5
```

### 4.2 Supported predicate operators

The system should support structured predicates including:

- equality: `=`
- inequality: `!=`
- less than: `<`
- less than or equal: `<=`
- greater than: `>`
- greater than or equal: `>=`
- conjunction: `AND`

Additional examples included in the project spec:

```text
tag CONTAINS 'vehicle'
timestamp BETWEEN t1 AND t2
```

### 4.3 Retrieval-by-ID behavior

The database must support a direct retrieval pattern:

```text
GET 513
```

and a query pattern:

```text
FIND OBJECT
WHERE object_id = 513
```

This indicates that the system should support both direct lookup and filtering-based object selection.

---

## 5. Search Semantics

### 5.1 Metadata filtering

The system must filter objects based on structured metadata before or alongside similarity evaluation. Examples include:

- `camera = 'drone'`
- `duration < 30`
- `speaker = 'unknown'`
- `timestamp >= '2026-01-01'`
- `tag CONTAINS 'vehicle'`

### 5.2 Similarity retrieval

The project requires support for searching by similarity to a query image, audio object, or vector. This is expressed as:

```text
SIMILAR TO 'query.jpg'
SIMILAR TO 'example.mp4'
SIMILAR TO 'sample.wav'
SIMILAR TO 'car.mp4'
```

The system must support a query object or embedding-based similarity comparison and return the closest matches sorted by relevance.

### 5.3 Combined search

The database should support hybrid queries that combine:

- metadata filtering
- semantic similarity
- ranking by relevance
- top K result selection

This is the most important multi-modal requirement and should be designed as a primary execution path.

---

## 6. Example CLI / Shell Behavior

The project spec shows a simple database shell interface:

```text
db> FIND IMAGE WHERE camera='drone' LIMIT 10
db> FIND IMAGE SIMILAR TO 'query.jpg' LIMIT 5
db> FIND VIDEO WHERE duration < 60 SIMILAR TO 'car.mp4' LIMIT 10
```

This suggests a minimal but user-friendly interface with:

- a command prompt (`db>`)
- a simple query language
- straightforward output of matching objects

---

## 7. Retrieval Architecture Plan

### 7.1 Proposed system components

1. Catalog store
   - Stores object metadata and file references.
   - Keeps `object_id` and `media_type` as the primary keys.

2. Metadata index
   - Supports fast filtering on fields like `camera`, `duration`, `speaker`, and timestamp.
   - Can be implemented with an in-memory dictionary, SQLite tables, or a lightweight indexed store.

3. Embedding store
   - Stores global embeddings and segment embeddings.
   - Supports nearest-neighbor lookup for semantic similarity search.

4. Query parser
   - Parses the command grammar and converts it into structured conditions.
   - Supports `FIND`, `WHERE`, `SIMILAR TO`, and `LIMIT` clauses.

5. Execution engine
   - Applies metadata filtering.
   - Executes similarity search over the relevant candidate set.
   - Merges results and ranks them.
   - Limits output to the requested number of results.

6. Response formatter
   - Returns the top K objects with enough metadata to inspect the match.

### 7.2 Recommended execution flow

```text
Parse query
  -> Validate media type and predicates
  -> Evaluate metadata filters
  -> Retrieve candidate set
  -> Compute similarity (if needed)
  -> Merge and rank results
  -> Apply LIMIT
  -> Return top K objects
```

---

## 8. Data Storage Strategy

### 8.1 Object catalog

Store each object in a record structured like:

```text
object_id
media_type
physical_location
size
timestamp
structured_metadata
embedding
segment_embeddings
```

### 8.2 Metadata representation

The metadata can be represented as:

- a typed JSON object
- a relational schema with common fields and optional extension fields
- a hybrid of both, where common fields are first-class and custom metadata is stored as JSON

For this project, a hybrid model is recommended to keep the system simple and flexible.

### 8.3 Embedding design

The system should hold:

- one global embedding per object for coarse retrieval
- optional segment embeddings for sub-part retrieval or more granular matching

This is explicitly required by the project text: "1 catalog object, 1 global embedding, N segment embeddings."

---

## 9. Evaluation and Validation

The PDF includes a Recall@K metric example:

```text
Recall@10 = 8/10 = 0.80
```

This suggests the final system must provide a measurable retrieval quality metric for search tasks.

### Evaluation goals

- Measure whether the top K returned results include the relevant objects.
- Benchmark metadata filtering quality.
- Benchmark semantic retrieval quality.
- Validate combined metadata + similarity searches.

### Minimum acceptable behavior

- Metadata filters work correctly.
- Similarity queries return relevant objects.
- Hybrid queries combine both semantics correctly.
- Top K results are deterministic and ranked sensibly.
- The system responds with a consistent and clean CLI output.

---

## 10. Non-Functional Requirements

### Performance

- Search should be efficient enough for small to medium multimodal datasets.
- Candidate filtering should avoid expensive full-dataset scans for straightforward metadata queries.
- Similarity retrieval should use a fast vector search mechanism or equivalent approximate nearest-neighbor method.

### Correctness

- Query parsing must be strict enough to catch malformed clauses.
- Returned results should obey `LIMIT` precisely.
- Retrieval should match object type and query-specific constraints.

### Maintainability

- Keep the code modular so each subsystem can be tested independently.
- Separate parsing, indexing, retrieval, and formatting concerns.
- Keep the database shell and query engine simple and predictable.

---

## 11. Proposed Implementation Plan

### Phase 1: System design and data schema

- Define the object model and storage format.
- Implement metadata schema for common fields.
- Design the query grammar and parser.
- Decide how embeddings and segment embeddings will be stored.

### Phase 2: Ingestion pipeline

- Implement object ingest for image/audio/video files.
- Store object metadata and references.
- Generate and save the global embedding.
- Optionally generate segment embeddings.

### Phase 3: Query engine

- Implement `FIND` queries with metadata filters.
- Implement `GET` by `object_id`.
- Implement `SIMILAR TO` semantics.
- Support combined metadata + similarity queries.
- Add ranked results and `LIMIT` handling.

### Phase 4: Shell / user interface

- Implement the `db>` query shell.
- Accept user queries in the project-defined syntax.
- Return readable matches and metadata.

### Phase 5: Evaluation and validation

- Build sample datasets for image/audio/video retrieval.
- Verify metadata filtering.
- Measure recall on representative retrieval tasks.
- Validate results against the expected `Recall@10` style metric.

---

## 12. Risks and Open Questions

1. How should custom metadata be normalized across different media types?
2. What is the expected similarity metric for embeddings?
3. Should segment embeddings be required for all objects or only some?
4. Will the system use a simple in-memory vector index or a more scalable approximate nearest-neighbor approach?
5. What constitutes a valid result object format for CLI output?

These can be answered during design review, but the minimal project should assume that the database can store metadata, embeddings, and query results at a practical scale.

---

## 13. Acceptance Criteria

The project is successful if the implementation can:

- store multimodal objects and metadata
- answer `GET` and `FIND` queries
- apply structured filters using common comparison operators
- perform similarity-based search using a reference object or vector
- combine metadata and similarity in the same query
- return top K results consistently
- produce results with a clear CLI or API interface
- report retrieval performance using a metric such as `Recall@10`

---

## 14. Recommended Next Step

The next action is to convert this plan into a concrete implementation backlog with:

- data model definitions
- parser and query grammar
- ingestion pipeline specification
- retrieval engine design
- evaluation script and sample dataset
- initial CLI commands for demonstration

This plan provides the foundation for that implementation work.
