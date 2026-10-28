mod catalog;
mod index;
mod media;
mod query;

use catalog::{Catalog, MediaObject, Store};
use index::{LshIndex, MetadataIndex};
use query::{Command, ExecutionResult, Plan};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::time::Instant;

struct Runtime {
    store: Store,
    catalog: Catalog,
    metadata_index: MetadataIndex,
    lsh_index: LshIndex,
    metadata_build_micros: u128,
    ann_build_micros: u128,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("mmdb: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let mut command_args = Vec::new();
    let mut database_root = PathBuf::from(".");
    let mut requested_plan = Plan::Auto;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--db" => {
                database_root = PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--db requires a directory path".to_owned())?,
                );
            }
            "--plan" => {
                requested_plan = Plan::parse(
                    &args
                        .next()
                        .ok_or_else(|| "--plan requires a plan name".to_owned())?,
                )?;
            }
            _ => command_args.push(argument),
        }
    }

    if command_args.is_empty()
        || command_args[0] == "repl"
        || command_args[0] == "--help"
        || command_args[0] == "-h"
    {
        if command_args.first().is_some_and(|arg| arg == "--help" || arg == "-h") {
            print_usage();
            return Ok(());
        }
        let mut runtime = Runtime::open(database_root)?;
        return repl(&mut runtime, requested_plan);
    }

    let command_name = command_args[0].to_ascii_lowercase();
    let rest = &command_args[1..];
    let store = Store::new(database_root);

    match command_name.as_str() {
        "init" => {
            let catalog = store.load()?;
            store.save(&catalog)?;
            println!("Initialized MMDB at {}", store.root().display());
            println!("Catalog: {}", store.catalog_path().display());
        }
        "ingest" => {
            let source = rest
                .first()
                .ok_or_else(|| "ingest requires a media file path".to_owned())?;
            let metadata = parse_metadata(&rest[1..])?;
            let mut catalog = store.load()?;
            let object = store.ingest(&mut catalog, &PathBuf::from(source.as_str()), metadata)?;
            println!(
                "Ingested {} {} bytes as object {} ({})",
                object.media_type.as_str(),
                object.size,
                object.object_id,
                object.physical_location
            );
            println!("Embedding dimensions: {}", object.embedding.len());
        }
        "query" | "find" => {
            if rest.is_empty() {
                return Err("query requires a quoted FIND or GET statement".to_owned());
            }
            let statement = rest.join(" ");
            let parsed = query::parse(&statement)?;
            let runtime = Runtime::open(store.root().to_path_buf())?;
            match parsed {
                Command::Get(id) => {
                    print_object_by_id(&runtime.catalog, &runtime.metadata_index, id)
                }
                Command::Find(find) => {
                    let result = query::execute(
                        &runtime.catalog,
                        &runtime.metadata_index,
                        &runtime.lsh_index,
                        &runtime.store,
                        &find,
                        requested_plan,
                    )?;
                    print_result(&result);
                }
            }
        }
        "get" => {
            let id = rest
                .first()
                .ok_or_else(|| "get requires an object identifier".to_owned())?
                .parse::<u64>()
                .map_err(|_| "object identifier must be an unsigned integer".to_owned())?;
            let runtime = Runtime::open(store.root().to_path_buf())?;
            print_object_by_id(&runtime.catalog, &runtime.metadata_index, id);
        }
        "stats" | "status" => {
            let runtime = Runtime::open(store.root().to_path_buf())?;
            print_stats(&runtime);
        }
        "bench" => {
            if rest.is_empty() {
                return Err(
                    "bench requires one or more quoted GET or FIND queries"
                        .to_owned(),
                );
            }
            let runtime = Runtime::open(store.root().to_path_buf())?;
            benchmark(&runtime, rest)?;
        }
        _ => {
            print_usage();
            return Err(format!("unknown command '{}'", command_args[0]));
        }
    }
    Ok(())
}

impl Runtime {
    fn open(root: PathBuf) -> Result<Self, String> {
        let store = Store::new(root);
        let catalog = store.load()?;
        let metadata_started = Instant::now();
        let metadata_index = MetadataIndex::build(&catalog);
        let metadata_build_micros = metadata_started.elapsed().as_micros();
        let ann_started = Instant::now();
        let lsh_index = LshIndex::build(&catalog);
        let ann_build_micros = ann_started.elapsed().as_micros();
        Ok(Self {
            store,
            catalog,
            metadata_index,
            lsh_index,
            metadata_build_micros,
            ann_build_micros,
        })
    }
}

fn parse_metadata(args: &[String]) -> Result<BTreeMap<String, Value>, String> {
    let reserved = [
        "id",
        "object_id",
        "media_type",
        "physical_location",
        "size",
        "embedding",
    ];
    let mut metadata = BTreeMap::new();
    for argument in args {
        let (key, value) = argument
            .split_once('=')
            .ok_or_else(|| format!("metadata must use key=value syntax: '{argument}'"))?;
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty() {
            return Err("metadata field names cannot be empty".to_owned());
        }
        if !key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(format!(
                "metadata field '{key}' must use only letters, digits, or underscores"
            ));
        }
        if reserved.contains(&key.as_str()) {
            return Err(format!("'{key}' is a reserved catalog field"));
        }
        if metadata.contains_key(&key) {
            return Err(format!("metadata field '{key}' was supplied more than once"));
        }
        let value = serde_json::from_str(value)
            .unwrap_or_else(|_| Value::String(value.to_owned()));
        if value.is_array() || value.is_object() {
            return Err(format!("metadata field '{key}' must have a scalar value"));
        }
        metadata.insert(key, value);
    }
    Ok(metadata)
}

fn lookup_by_id<'a>(
    catalog: &'a Catalog,
    metadata_index: &MetadataIndex,
    id: u64,
) -> Option<&'a MediaObject> {
    let positions = metadata_index.equality("object_id", &Value::from(id))?;
    let position = *positions.first()?;
    catalog
        .objects
        .get(position)
        .filter(|object| object.object_id == id)
}

fn print_object_by_id(catalog: &Catalog, metadata_index: &MetadataIndex, id: u64) {
    let start = Instant::now();
    let object = lookup_by_id(catalog, metadata_index, id);
    println!(
        "lookup_plan=hash-index candidates_examined={} latency_us={}",
        usize::from(object.is_some()),
        start.elapsed().as_micros()
    );
    match object {
        Some(object) => print_object(object, None),
        None => println!("No object with id {id}"),
    }
}

fn print_object(object: &MediaObject, similarity: Option<f32>) {
    println!("object_id: {}", object.object_id);
    println!("media_type: {}", object.media_type.as_str());
    println!("physical_location: {}", object.physical_location);
    println!("size: {}", object.size);
    println!("timestamp: {}", object.timestamp);
    if let Some(score) = similarity {
        println!("cosine_similarity: {score:.6}");
    }
    println!("embedding_dimensions: {}", object.embedding.len());
    println!("segment_embedding_count: {}", object.segment_embeddings.len());
    println!(
        "metadata: {}",
        serde_json::to_string_pretty(&object.metadata).unwrap_or_else(|_| "{}".to_owned())
    );
}

fn print_result(result: &ExecutionResult) {
    println!(
        "plan={} candidates_examined={} matches={} latency_us={}",
        result.plan.name(),
        result.candidates_examined,
        result.total_matches_before_limit,
        result.elapsed_micros
    );
    for (rank, hit) in result.hits.iter().enumerate() {
        println!(
            "\n[{}] id={} type={} size={} timestamp={} similarity={} location={}",
            rank + 1,
            hit.object.object_id,
            hit.object.media_type.as_str(),
            hit.object.size,
            hit.object.timestamp,
            hit.similarity
                .map(|value| format!("{value:.6}"))
                .unwrap_or_else(|| "-".to_owned()),
            hit.object.physical_location
        );
        println!(
            "metadata={}",
            serde_json::to_string(&hit.object.metadata).unwrap_or_else(|_| "{}".to_owned())
        );
    }
}

fn print_stats(runtime: &Runtime) {
    let image_count = runtime
        .catalog
        .objects
        .iter()
        .filter(|object| object.media_type == media::MediaType::Image)
        .count();
    let audio_count = runtime
        .catalog
        .objects
        .iter()
        .filter(|object| object.media_type == media::MediaType::Audio)
        .count();
    let video_count = runtime
        .catalog
        .objects
        .iter()
        .filter(|object| object.media_type == media::MediaType::Video)
        .count();
    let vector_count: usize = runtime
        .catalog
        .objects
        .iter()
        .map(|object| object.embedding.len())
        .sum();
    let metadata_bytes: usize = runtime
        .catalog
        .objects
        .iter()
        .map(|object| {
            object.metadata.iter().map(|(key, value)| {
                key.len() + serde_json::to_vec(value).map(|bytes| bytes.len()).unwrap_or(0)
            }).sum::<usize>()
                + object.timestamp.len()
                + object.physical_location.len()
                + 24
        })
        .sum();
    let payload_bytes: u64 = runtime.catalog.objects.iter().map(|object| object.size).sum();
    let catalog_bytes = std::fs::metadata(runtime.store.catalog_path())
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    println!("database: {}", runtime.store.root().display());
    println!("objects: {}", runtime.catalog.objects.len());
    println!("image objects: {image_count}");
    println!("audio objects: {audio_count}");
    println!("video objects: {video_count}");
    println!("vector embeddings: {}", runtime.catalog.objects.len());
    println!("embedding values: {vector_count}");
    println!("estimated structured metadata bytes: {metadata_bytes}");
    println!("payload storage bytes: {payload_bytes}");
    println!("catalog bytes: {catalog_bytes}");
    println!(
        "approximate total storage bytes: {}",
        payload_bytes + catalog_bytes
    );
    println!(
        "metadata index: {} postings, approximately {} bytes, built in {} us",
        runtime.metadata_index.entry_count(),
        runtime.metadata_index.size_estimate_bytes(),
        runtime.metadata_build_micros
    );
    println!(
        "LSH index: {} vectors, {} buckets, approximately {} bytes, built in {} us ({} tables x {} bits)",
        runtime.lsh_index.vector_count(),
        runtime.lsh_index.bucket_count(),
        runtime.lsh_index.size_estimate_bytes(),
        runtime.ann_build_micros,
        LshIndex::TABLE_COUNT,
        LshIndex::BITS_PER_TABLE
    );
}

struct MethodStats {
    latencies: Vec<u128>,
    candidates: Vec<usize>,
    recalls: Vec<f64>,
}

fn benchmark(runtime: &Runtime, statements: &[String]) -> Result<(), String> {
    let mut methods: BTreeMap<&'static str, MethodStats> = BTreeMap::new();
    for statement in statements {
        let command = query::parse(statement)?;
        let find = match command {
            Command::Get(id) => {
                let start = Instant::now();
                let found = lookup_by_id(&runtime.catalog, &runtime.metadata_index, id);
                let elapsed = start.elapsed().as_micros();
                let stats = methods.entry("point-lookup").or_insert_with(|| MethodStats {
                    latencies: Vec::new(),
                    candidates: Vec::new(),
                    recalls: Vec::new(),
                });
                stats.latencies.push(elapsed);
                stats.candidates.push(if found.is_some() { 1 } else { 0 });
                continue;
            }
            Command::Find(find) => find,
        };
        let mut find = find;
        if find.similar_to.is_some() {
            find.limit = 10;
        }
        let plans: &[Plan] = if find.similar_to.is_some() {
            &[
                Plan::Exact,
                Plan::Ann,
                Plan::FilterFirst,
                Plan::VectorFirst,
            ]
        } else {
            &[Plan::Scan, Plan::Indexed]
        };
        let mut exact_ids = Vec::new();
        for plan in plans {
            let result = query::execute(
                &runtime.catalog,
                &runtime.metadata_index,
                &runtime.lsh_index,
                &runtime.store,
                &find,
                *plan,
            )?;
            if *plan == Plan::Exact {
                exact_ids = result
                    .hits
                    .iter()
                    .map(|hit| hit.object.object_id)
                    .collect();
            }
            let stats = methods.entry(plan.name()).or_insert_with(|| MethodStats {
                latencies: Vec::new(),
                candidates: Vec::new(),
                recalls: Vec::new(),
            });
            stats.latencies.push(result.elapsed_micros);
            stats.candidates.push(result.candidates_examined);
            if find.similar_to.is_some() && *plan != Plan::Exact {
                let returned: Vec<u64> = result
                    .hits
                    .iter()
                    .map(|hit| hit.object.object_id)
                    .collect();
                let relevant = exact_ids
                    .iter()
                    .filter(|id| returned.contains(id))
                    .count();
                stats.recalls.push(if exact_ids.is_empty() {
                    1.0
                } else {
                    relevant as f64 / exact_ids.len() as f64
                });
            }
        }
    }

    println!(
        "queries={} metadata_index_build_us={} ann_index_build_us={} ann_index_bytes={}",
        statements.len(),
        runtime.metadata_build_micros,
        runtime.ann_build_micros,
        runtime.lsh_index.size_estimate_bytes()
    );
    println!(
        "{:<15} {:>9} {:>9} {:>12} {:>12} {:>12}",
        "method", "p50_us", "p95_us", "avg_examined", "Recall@10", "runs"
    );
    for (method, stats) in methods {
        let recall = if stats.recalls.is_empty() {
            "-".to_owned()
        } else {
            format!("{:.3}", mean_f64(&stats.recalls))
        };
        println!(
            "{:<15} {:>9} {:>9} {:>12.1} {:>12} {:>12}",
            method,
            percentile(&stats.latencies, 0.50),
            percentile(&stats.latencies, 0.95),
            mean_usize(&stats.candidates),
            recall,
            stats.latencies.len()
        );
    }
    Ok(())
}

fn percentile(values: &[u128], percentile: f64) -> u128 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = ((sorted.len() as f64 * percentile).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    sorted[index]
}

fn mean_usize(values: &[usize]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<usize>() as f64 / values.len() as f64
    }
}

fn mean_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn repl(runtime: &mut Runtime, requested_plan: Plan) -> Result<(), String> {
    println!("MMDB image/audio catalog. Enter FIND or GET queries; use exit to quit.");
    let stdin = io::stdin();
    let mut input = String::new();
    loop {
        print!("db> ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        input.clear();
        if stdin.lock().read_line(&mut input).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let statement = input.trim();
        if statement.eq_ignore_ascii_case("exit") || statement.eq_ignore_ascii_case("quit") {
            break;
        }
        if statement.is_empty() {
            continue;
        }
        match query::parse(statement) {
            Ok(Command::Get(id)) => {
                print_object_by_id(&runtime.catalog, &runtime.metadata_index, id)
            }
            Ok(Command::Find(find)) => match query::execute(
                &runtime.catalog,
                &runtime.metadata_index,
                &runtime.lsh_index,
                &runtime.store,
                &find,
                requested_plan,
            ) {
                Ok(result) => print_result(&result),
                Err(error) => eprintln!("query error: {error}"),
            },
            Err(error) => eprintln!("parse error: {error}"),
        }
    }
    Ok(())
}

fn print_usage() {
    println!(
        "MMDB - image, audio, and video database\n\
         \n\
         Commands:\n\
           mmdb init [--db DIR]\n\
           mmdb ingest FILE [key=value ...] [--db DIR]\n\
           mmdb query \"FIND IMAGE WHERE camera='drone' LIMIT 10\" [--plan auto|indexed|scan|exact|ann|filter-first|vector-first] [--db DIR]\n\
           mmdb get ID [--db DIR]\n\
           mmdb stats [--db DIR]\n\
           mmdb bench \"FIND IMAGE SIMILAR TO 'query.jpg' LIMIT 10\" ... [--db DIR]\n\
           mmdb repl [--plan PLAN] [--db DIR]\n\
         \n\
         Similarity query paths may be absolute, relative to the current directory, or relative to DIR.\n\
         Supported media inputs are images, audio, and video."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaType;
    use serde_json::json;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mmdb-main-tests-{}-{id}", std::process::id()));
            fs::create_dir(&path).expect("create unique temporary test directory");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn object(
        object_id: u64,
        media_type: MediaType,
        metadata: BTreeMap<String, Value>,
        embedding: Vec<f32>,
    ) -> MediaObject {
        MediaObject {
            object_id,
            media_type,
            physical_location: format!("data/objects/{object_id:08}.bin"),
            size: 16,
            timestamp: "2026-01-01T00:00:00Z".to_owned(),
            metadata,
            embedding,
            segment_embeddings: vec![],
        }
    }

    fn sample_catalog() -> Catalog {
        Catalog {
            objects: vec![
                object(
                    1,
                    MediaType::Image,
                    BTreeMap::from([
                        ("camera".to_owned(), json!("drone")),
                        ("rating".to_owned(), json!(4)),
                    ]),
                    vec![1.0, 0.0],
                ),
                object(
                    2,
                    MediaType::Audio,
                    BTreeMap::from([("speaker".to_owned(), json!("field"))]),
                    vec![0.0, 1.0],
                ),
            ],
            next_id: 3,
            ..Catalog::default()
        }
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parse_metadata_accepts_empty_input() {
        assert!(parse_metadata(&[]).unwrap().is_empty());
    }

    #[test]
    fn parse_metadata_normalizes_keys_and_parses_json_scalars() {
        let metadata = parse_metadata(&args(&[
            " Camera =drone",
            "rating=4",
            "verified=true",
            "missing=null",
            "label=a=b",
        ]))
        .unwrap();

        assert_eq!(metadata["camera"], json!("drone"));
        assert_eq!(metadata["rating"], json!(4));
        assert_eq!(metadata["verified"], json!(true));
        assert_eq!(metadata["missing"], Value::Null);
        assert_eq!(metadata["label"], json!("a=b"));
    }

    #[test]
    fn parse_metadata_keeps_non_json_values_as_strings() {
        let metadata = parse_metadata(&args(&["name=not JSON", "empty="])).unwrap();

        assert_eq!(metadata["name"], json!("not JSON"));
        assert_eq!(metadata["empty"], json!(""));
    }

    #[test]
    fn parse_metadata_rejects_arguments_without_equals() {
        let error = parse_metadata(&args(&["camera"])).unwrap_err();
        assert!(error.contains("key=value"));
    }

    #[test]
    fn parse_metadata_rejects_empty_or_invalid_keys() {
        assert!(parse_metadata(&args(&["  =value"]))
            .unwrap_err()
            .contains("cannot be empty"));
        assert!(parse_metadata(&args(&["camera-name=value"]))
            .unwrap_err()
            .contains("only letters, digits, or underscores"));
    }

    #[test]
    fn parse_metadata_rejects_reserved_keys_case_insensitively() {
        let error = parse_metadata(&args(&["Object_ID=1"])).unwrap_err();
        assert!(error.contains("reserved catalog field"));
    }

    #[test]
    fn parse_metadata_rejects_duplicate_keys_after_normalization() {
        let error = parse_metadata(&args(&["Camera=drone", "camera=air"])).unwrap_err();
        assert!(error.contains("supplied more than once"));
    }

    #[test]
    fn parse_metadata_rejects_array_and_object_values() {
        assert!(parse_metadata(&args(&["tags=[1,2]"]))
            .unwrap_err()
            .contains("scalar value"));
        assert!(parse_metadata(&args(&["details={\"x\":1}"]))
            .unwrap_err()
            .contains("scalar value"));
    }

    #[test]
    fn lookup_by_id_returns_matching_object_and_ignores_missing_ids() {
        let catalog = sample_catalog();
        let index = MetadataIndex::build(&catalog);

        assert_eq!(lookup_by_id(&catalog, &index, 1).unwrap().object_id, 1);
        assert_eq!(lookup_by_id(&catalog, &index, 2).unwrap().object_id, 2);
        assert!(lookup_by_id(&catalog, &index, 0).is_none());
        assert!(lookup_by_id(&catalog, &index, u64::MAX).is_none());
    }

    #[test]
    fn lookup_by_id_handles_an_empty_catalog() {
        let catalog = Catalog::default();
        let index = MetadataIndex::build(&catalog);

        assert!(lookup_by_id(&catalog, &index, 1).is_none());
    }

    #[test]
    fn percentile_uses_nearest_rank_and_handles_boundaries() {
        let values = [40, 10, 30, 20];

        assert_eq!(percentile(&[], 0.5), 0);
        assert_eq!(percentile(&[7], 0.95), 7);
        assert_eq!(percentile(&values, 0.0), 10);
        assert_eq!(percentile(&values, 0.5), 20);
        assert_eq!(percentile(&values, 0.95), 40);
        assert_eq!(percentile(&values, 2.0), 40);
    }

    #[test]
    fn mean_helpers_handle_empty_and_populated_values() {
        assert_eq!(mean_usize(&[]), 0.0);
        assert_eq!(mean_usize(&[1, 2, 6]), 3.0);
        assert_eq!(mean_f64(&[]), 0.0);
        assert_eq!(mean_f64(&[0.25, 0.75, 1.0]), 2.0 / 3.0);
    }

    #[test]
    fn runtime_open_builds_indexes_from_a_saved_catalog() {
        let directory = TempDir::new();
        let store = Store::new(directory.path());
        store.save(&sample_catalog()).unwrap();

        let runtime = Runtime::open(directory.path().to_path_buf()).unwrap();

        assert_eq!(runtime.catalog.objects.len(), 2);
        assert_eq!(
            runtime.metadata_index.equality("camera", &json!("drone")),
            Some(&[0][..])
        );
        assert_eq!(runtime.lsh_index.vector_count(), 2);
        assert!(lookup_by_id(&runtime.catalog, &runtime.metadata_index, 2).is_some());
    }

    #[test]
    fn runtime_open_uses_an_empty_catalog_when_no_catalog_exists() {
        let directory = TempDir::new();

        let runtime = Runtime::open(directory.path().to_path_buf()).unwrap();

        assert!(runtime.catalog.objects.is_empty());
        assert_eq!(runtime.metadata_index.entry_count(), 0);
        assert_eq!(runtime.lsh_index.vector_count(), 0);
    }

    #[test]
    fn runtime_open_reports_invalid_catalog_json() {
        let directory = TempDir::new();
        let catalog_path = directory.path().join("data").join("catalog.json");
        fs::create_dir_all(catalog_path.parent().unwrap()).unwrap();
        fs::write(&catalog_path, b"{not json").unwrap();

        let error = match Runtime::open(directory.path().to_path_buf()) {
            Ok(_) => panic!("invalid catalog should fail to load"),
            Err(error) => error,
        };

        assert!(error.contains("invalid catalog"));
    }

    #[test]
    fn benchmark_accepts_get_and_metadata_find_queries() {
        let directory = TempDir::new();
        let store = Store::new(directory.path());
        store.save(&sample_catalog()).unwrap();
        let runtime = Runtime::open(directory.path().to_path_buf()).unwrap();
        let statements = args(&["GET 1", "FIND IMAGE WHERE camera = 'drone' LIMIT 5"]);

        assert!(benchmark(&runtime, &statements).is_ok());
    }

    #[test]
    fn benchmark_returns_query_parse_errors() {
        let directory = TempDir::new();
        let runtime = Runtime::open(directory.path().to_path_buf()).unwrap();

        let error = benchmark(&runtime, &args(&["not a query"])).unwrap_err();

        assert!(!error.is_empty());
    }
}
