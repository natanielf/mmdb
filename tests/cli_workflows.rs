use image::{Rgb, RgbImage};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after UNIX epoch")
            .as_nanos();
        let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mmdb-integration-{}-{timestamp}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create unique temporary directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run<I, S>(database: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_from(database, Path::new("."), args)
}

fn run_from<I, S>(database: &Path, working_directory: &Path, args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_mmdb"))
        .args(args)
        .arg("--db")
        .arg(database)
        .current_dir(working_directory)
        .output()
        .expect("run mmdb CLI")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "expected command to succeed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
    stdout(output)
}

fn assert_failure(output: &Output, expected_message: &str) {
    assert!(
        !output.status.success(),
        "expected command to fail\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
    assert!(
        stderr(output).contains(expected_message),
        "expected stderr to contain {expected_message:?}, got:\n{}",
        stderr(output)
    );
}

fn assert_find_ids(database: &Path, query: &str, expected_ids: &[u64]) {
    assert_find(database, query, expected_ids.len(), expected_ids);
}

fn assert_find(database: &Path, query: &str, expected_matches: usize, expected_ids: &[u64]) {
    let output = assert_success(&run(database, ["query", query]));
    assert!(
        output.contains(&format!("matches={expected_matches}")),
        "unexpected match count for {query:?}:\n{output}"
    );
    for id in 1..=4 {
        assert_eq!(
            output.contains(&format!("id={id} type=")),
            expected_ids.contains(&id),
            "unexpected object {id} membership for {query:?}:\n{output}"
        );
    }
}

fn write_png(path: &Path, color: [u8; 3]) {
    let image = RgbImage::from_pixel(8, 8, Rgb(color));
    image.save(path).expect("write PNG fixture");
}

fn write_wav(path: &Path) {
    const SAMPLE_RATE: u32 = 8_000;
    const SAMPLE_COUNT: u32 = SAMPLE_RATE;
    const DATA_SIZE: u32 = SAMPLE_COUNT * 2;

    let mut wav = Vec::with_capacity((44 + DATA_SIZE) as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + DATA_SIZE).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&DATA_SIZE.to_le_bytes());
    for sample_index in 0..SAMPLE_COUNT {
        let sample = if sample_index % 2 == 0 {
            8_000_i16
        } else {
            -8_000_i16
        };
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    fs::write(path, wav).expect("write WAV fixture");
}

fn write_video(path: &Path, contents: &[u8]) {
    fs::write(path, contents).expect("write video fixture");
}

#[test]
fn init_help_and_empty_stats_work() {
    let temp = TempDir::new();
    let database = temp.path().join("db");

    let help = Command::new(env!("CARGO_BIN_EXE_mmdb"))
        .arg("--help")
        .output()
        .expect("run help");
    assert!(assert_success(&help).contains("Commands:"));

    let initialized = assert_success(&run(&database, ["init"]));
    assert!(initialized.contains("Initialized MMDB"));
    let catalog_path = database.join("data/catalog.json");
    assert!(catalog_path.is_file());

    let catalog: Value =
        serde_json::from_slice(&fs::read(catalog_path).expect("read catalog")).unwrap();
    assert_eq!(catalog["schema_version"], 1);
    assert_eq!(catalog["next_id"], 1);
    assert_eq!(catalog["objects"].as_array().unwrap().len(), 0);

    let stats = assert_success(&run(&database, ["stats"]));
    assert!(stats.contains("objects: 0"));
    assert!(stats.contains("image objects: 0"));
    assert!(stats.contains("audio objects: 0"));
    assert!(stats.contains("LSH index: 0 vectors"));
}

#[test]
fn ingest_query_and_reopen_image_and_audio_database() {
    let temp = TempDir::new();
    let database = temp.path().join("db");
    let image_path = temp.path().join("blue-image.png");
    let audio_path = temp.path().join("tone.wav");
    write_png(&image_path, [20, 80, 220]);
    write_wav(&audio_path);

    let image_path_arg = image_path.to_str().unwrap();
    let image_ingest = assert_success(&run(
        &database,
        [
            "ingest",
            image_path_arg,
            "camera=drone",
            "rating=4",
            "timestamp=2026-02-18T10:30:00Z",
        ],
    ));
    assert!(image_ingest.contains("Ingested image"));
    assert!(image_ingest.contains("as object 1"));
    assert!(image_ingest.contains("Embedding dimensions: 32"));

    let audio_ingest = assert_success(&run(
        &database,
        ["ingest", audio_path.to_str().unwrap(), "speaker=field"],
    ));
    assert!(audio_ingest.contains("Ingested audio"));
    assert!(audio_ingest.contains("as object 2"));

    let catalog_path = database.join("data/catalog.json");
    let catalog: Value =
        serde_json::from_slice(&fs::read(&catalog_path).expect("read saved catalog")).unwrap();
    let objects = catalog["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 2);
    assert_eq!(objects[0]["metadata"]["camera"], "drone");
    assert_eq!(objects[0]["metadata"]["rating"], 4);
    assert_eq!(objects[0]["timestamp"], "2026-02-18T10:30:00.000Z");
    assert_eq!(objects[1]["metadata"]["speaker"], "field");
    assert_eq!(objects[0]["embedding"].as_array().unwrap().len(), 32);
    assert_eq!(objects[1]["embedding"].as_array().unwrap().len(), 32);
    assert!(database
        .join(objects[0]["physical_location"].as_str().unwrap())
        .is_file());
    assert!(database
        .join(objects[1]["physical_location"].as_str().unwrap())
        .is_file());

    let get = assert_success(&run(&database, ["get", "2"]));
    assert!(get.contains("object_id: 2"));
    assert!(get.contains("media_type: audio"));
    assert!(get.contains("\"speaker\": \"field\""));

    let indexed_query = assert_success(&run(
        &database,
        [
            "query",
            "FIND IMAGE WHERE camera = 'drone' AND rating >= 4 LIMIT 5",
        ],
    ));
    assert!(indexed_query.contains("plan=indexed"));
    assert!(indexed_query.contains("matches=1"));
    assert!(indexed_query.contains("id=1 type=image"));

    let scan_query = assert_success(&run(
        &database,
        [
            "--plan",
            "scan",
            "query",
            "FIND IMAGE WHERE camera = 'drone' AND rating >= 4 LIMIT 5",
        ],
    ));
    assert!(scan_query.contains("plan=full-scan"));
    assert!(scan_query.contains("id=1 type=image"));

    let contains_query = assert_success(&run(
        &database,
        ["query", "FIND IMAGE WHERE name CONTAINS 'blue' LIMIT 5"],
    ));
    assert!(contains_query.contains("matches=1"));
    assert!(contains_query.contains("id=1 type=image"));

    let between_query = assert_success(&run(
        &database,
        ["query", "FIND AUDIO WHERE duration BETWEEN 0 AND 2 LIMIT 5"],
    ));
    assert!(between_query.contains("matches=1"));
    assert!(between_query.contains("id=2 type=audio"));

    let object_query = assert_success(&run(&database, ["query", "FIND OBJECT LIMIT 5"]));
    assert!(object_query.contains("matches=2"));
    assert!(object_query.contains("id=1 type=image"));
    assert!(object_query.contains("id=2 type=audio"));

    let stats = assert_success(&run(&database, ["stats"]));
    assert!(stats.contains("objects: 2"));
    assert!(stats.contains("image objects: 1"));
    assert!(stats.contains("audio objects: 1"));
    assert!(stats.contains("LSH index: 2 vectors"));
}

#[test]
fn similarity_plans_hybrid_search_and_benchmark_work() {
    let temp = TempDir::new();
    let database = temp.path().join("db");
    let image_path = temp.path().join("query-image.png");
    let audio_path = temp.path().join("query-audio.wav");
    write_png(&image_path, [220, 30, 80]);
    write_wav(&audio_path);

    assert_success(&run(
        &database,
        ["ingest", image_path.to_str().unwrap(), "camera=drone"],
    ));
    assert_success(&run(
        &database,
        ["ingest", audio_path.to_str().unwrap(), "speaker=field"],
    ));

    let image_exact = assert_success(&run(
        &database,
        [
            "--plan",
            "exact",
            "query",
            &format!("FIND IMAGE SIMILAR TO '{}' LIMIT 1", image_path.display()),
        ],
    ));
    assert!(image_exact.contains("plan=exact"));
    assert!(image_exact.contains("id=1 type=image"));
    assert!(image_exact.contains("similarity=1.000000"));

    let image_ann = assert_success(&run(
        &database,
        [
            "query",
            &format!("FIND IMAGE SIMILAR TO '{}' LIMIT 1", image_path.display()),
        ],
    ));
    assert!(image_ann.contains("plan=ann"));
    assert!(image_ann.contains("id=1 type=image"));

    let hybrid = assert_success(&run(
        &database,
        [
            "query",
            &format!(
                "FIND IMAGE WHERE camera = 'drone' SIMILAR TO '{}' LIMIT 1",
                image_path.display()
            ),
        ],
    ));
    assert!(hybrid.contains("plan=filter-first"));
    assert!(hybrid.contains("id=1 type=image"));

    let audio_exact = assert_success(&run(
        &database,
        [
            "--plan",
            "exact",
            "query",
            &format!("FIND AUDIO SIMILAR TO '{}' LIMIT 1", audio_path.display()),
        ],
    ));
    assert!(audio_exact.contains("id=2 type=audio"));
    assert!(audio_exact.contains("similarity=1.000000"));

    let mismatch = run(
        &database,
        [
            "query",
            &format!("FIND AUDIO SIMILAR TO '{}' LIMIT 1", image_path.display()),
        ],
    );
    assert_failure(&mismatch, "query file is image, but FIND specifies audio");

    let benchmark = assert_success(&run(
        &database,
        [
            "bench",
            "GET 1",
            "FIND IMAGE WHERE camera = 'drone' LIMIT 5",
            &format!("FIND IMAGE SIMILAR TO '{}' LIMIT 1", image_path.display()),
        ],
    ));
    assert!(benchmark.contains("method"));
    assert!(benchmark.contains("indexed"));
    assert!(benchmark.contains("exact"));
    assert!(benchmark.contains("ann"));
}

#[test]
fn video_ingest_metadata_similarity_and_cross_modality_work() {
    let temp = TempDir::new();
    let database = temp.path().join("db");
    let video_path = temp.path().join("clip.mp4");
    let image_path = temp.path().join("query.png");
    write_video(
        &video_path,
        b"deterministic video bytes with multiple histogram bins",
    );
    write_png(&image_path, [20, 80, 220]);

    let ingest = assert_success(&run(
        &database,
        [
            "ingest",
            video_path.to_str().unwrap(),
            "camera=drone",
            "duration_seconds=12.5",
        ],
    ));
    assert!(ingest.contains("Ingested video"));
    assert!(ingest.contains("as object 1"));
    assert!(ingest.contains("Embedding dimensions: 32"));

    let catalog: Value = serde_json::from_slice(
        &fs::read(database.join("data/catalog.json")).expect("read catalog"),
    )
    .unwrap();
    let video = &catalog["objects"][0];
    assert_eq!(video["media_type"], "video");
    assert_eq!(video["metadata"]["camera"], "drone");
    assert_eq!(video["metadata"]["duration_seconds"], 12.5);
    assert_eq!(video["embedding"].as_array().unwrap().len(), 32);
    assert_eq!(video["segment_embeddings"].as_array().unwrap().len(), 4);
    assert!(database
        .join(video["physical_location"].as_str().unwrap())
        .is_file());

    let metadata_query = assert_success(&run(
        &database,
        [
            "query",
            "FIND VIDEO WHERE camera = 'drone' AND duration_seconds < 60 LIMIT 5",
        ],
    ));
    assert!(metadata_query.contains("matches=1"));
    assert!(metadata_query.contains("id=1 type=video"));

    let similarity_query = assert_success(&run(
        &database,
        [
            "--plan",
            "exact",
            "query",
            &format!("FIND VIDEO SIMILAR TO '{}' LIMIT 1", video_path.display()),
        ],
    ));
    assert!(similarity_query.contains("plan=exact"));
    assert!(similarity_query.contains("id=1 type=video"));
    assert!(similarity_query.contains("similarity=1.000000"));

    assert_failure(
        &run(
            &database,
            [
                "query",
                &format!("FIND VIDEO SIMILAR TO '{}' LIMIT 1", image_path.display()),
            ],
        ),
        "query file is image, but FIND specifies video",
    );

    let stats = assert_success(&run(&database, ["stats"]));
    assert!(stats.contains("video objects: 1"));
    assert!(stats.contains("LSH index: 1 vectors"));
}

#[test]
fn larger_collection_exercises_ann_and_vector_first_plans() {
    let temp = TempDir::new();
    let database = temp.path().join("db");
    let query_path = temp.path().join("query.png");
    write_png(&query_path, [12, 72, 180]);

    for index in 0..72 {
        let image_path = temp.path().join(format!("image-{index:03}.png"));
        let is_match = index < 4;
        let color = if is_match {
            [12, 72, 180]
        } else {
            let shade = ((index * 37) % 256) as u8;
            [shade, shade.wrapping_add(53), shade.wrapping_add(109)]
        };
        write_png(&image_path, color);
        let group = if is_match { "target" } else { "other" };
        let ingest = run(
            &database,
            [
                "ingest",
                image_path.to_str().unwrap(),
                &format!("group={group}"),
            ],
        );
        assert_success(&ingest);
    }

    let catalog: Value = serde_json::from_slice(
        &fs::read(database.join("data/catalog.json")).expect("read catalog"),
    )
    .unwrap();
    assert_eq!(catalog["objects"].as_array().unwrap().len(), 72);

    let exact = assert_success(&run(
        &database,
        [
            "--plan",
            "exact",
            "query",
            &format!(
                "FIND IMAGE WHERE group = 'target' SIMILAR TO '{}' LIMIT 4",
                query_path.display()
            ),
        ],
    ));
    assert!(exact.contains("plan=exact"));
    for id in 1..=4 {
        assert!(exact.contains(&format!("id={id} type=image")));
    }

    let ann = assert_success(&run(
        &database,
        [
            "query",
            &format!("FIND IMAGE SIMILAR TO '{}' LIMIT 4", query_path.display()),
        ],
    ));
    assert!(ann.contains("plan=ann"));
    assert!(ann.contains("id=1 type=image"));

    let vector_first = assert_success(&run(
        &database,
        [
            "--plan",
            "vector-first",
            "query",
            &format!(
                "FIND IMAGE WHERE group = 'target' SIMILAR TO '{}' LIMIT 4",
                query_path.display()
            ),
        ],
    ));
    assert!(vector_first.contains("plan=vector-first"));
    for id in 1..=4 {
        assert!(vector_first.contains(&format!("id={id} type=image")));
    }
}

#[test]
fn similarity_query_paths_resolve_from_working_directory_and_database() {
    let temp = TempDir::new();
    let database = temp.path().join("db");
    let working_directory = temp.path().join("work");
    let query_directory = working_directory.join("queries");
    fs::create_dir_all(&query_directory).unwrap();
    let source_path = temp.path().join("source.png");
    let local_query_path = query_directory.join("query.png");
    let corrupt_query_path = query_directory.join("corrupt.png");
    write_png(&source_path, [12, 72, 180]);
    write_png(&local_query_path, [12, 72, 180]);
    fs::write(&corrupt_query_path, b"not a PNG").unwrap();

    assert_success(&run(&database, ["ingest", source_path.to_str().unwrap()]));

    let working_directory_query = assert_success(&run_from(
        &database,
        &working_directory,
        ["query", "FIND IMAGE SIMILAR TO 'queries/query.png' LIMIT 1"],
    ));
    assert!(working_directory_query.contains("id=1 type=image"));
    assert!(working_directory_query.contains("similarity=1.000000"));

    let database_relative_query = assert_success(&run(
        &database,
        [
            "query",
            "FIND IMAGE SIMILAR TO 'data/objects/00000001.png' LIMIT 1",
        ],
    ));
    assert!(database_relative_query.contains("id=1 type=image"));
    assert!(database_relative_query.contains("similarity=1.000000"));

    assert_failure(
        &run_from(
            &database,
            &working_directory,
            [
                "query",
                "FIND IMAGE SIMILAR TO 'queries/missing.png' LIMIT 1",
            ],
        ),
        "similarity query file not found",
    );
    assert_failure(
        &run_from(
            &database,
            &working_directory,
            [
                "query",
                "FIND IMAGE SIMILAR TO 'queries/corrupt.png' LIMIT 1",
            ],
        ),
        "cannot decode image",
    );
}

#[test]
fn cli_errors_preserve_existing_database_on_failed_ingest() {
    let temp = TempDir::new();
    let database = temp.path().join("db");
    let valid_image = temp.path().join("valid.png");
    let corrupt_image = temp.path().join("corrupt.png");
    write_png(&valid_image, [20, 80, 220]);
    fs::write(&corrupt_image, b"not a PNG").unwrap();

    assert_success(&run(&database, ["init"]));
    assert_success(&run(
        &database,
        ["ingest", valid_image.to_str().unwrap(), "camera=drone"],
    ));
    let catalog_path = database.join("data/catalog.json");
    let catalog_before = fs::read(&catalog_path).expect("read catalog before failures");
    let payloads_before: Vec<_> = fs::read_dir(database.join("data/objects"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(payloads_before.len(), 1);

    assert_failure(
        &run(&database, ["ingest"]),
        "ingest requires a media file path",
    );
    assert_failure(
        &run(
            &database,
            ["ingest", temp.path().join("missing.png").to_str().unwrap()],
        ),
        "media file does not exist",
    );
    assert_failure(
        &run(
            &database,
            [
                "ingest",
                valid_image.to_str().unwrap(),
                "timestamp=not-a-timestamp",
            ],
        ),
        "timestamp must be RFC 3339",
    );
    assert_failure(
        &run(
            &database,
            ["ingest", valid_image.to_str().unwrap(), "object_id=99"],
        ),
        "reserved catalog field",
    );
    assert_failure(
        &run(
            &database,
            [
                "ingest",
                valid_image.to_str().unwrap(),
                "Camera=drone",
                "camera=air",
            ],
        ),
        "supplied more than once",
    );
    assert_failure(
        &run(&database, ["ingest", corrupt_image.to_str().unwrap()]),
        "cannot decode image",
    );

    assert_eq!(
        fs::read(&catalog_path).expect("read catalog after failures"),
        catalog_before,
        "failed ingests must not modify the catalog",
    );
    let payloads_after: Vec<_> = fs::read_dir(database.join("data/objects"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(payloads_after, payloads_before);

    fs::write(&catalog_path, b"{invalid catalog").unwrap();
    assert_failure(&run(&database, ["stats"]), "invalid catalog");
}

#[test]
fn metadata_operators_timestamp_ranges_limits_and_modality_are_consistent() {
    let temp = TempDir::new();
    let database = temp.path().join("db");

    for id in 1..=3 {
        let image_path = temp.path().join(format!("image-{id}.png"));
        write_png(&image_path, [id as u8 * 40, 80, 180]);
        assert_success(&run(
            &database,
            [
                "ingest",
                image_path.to_str().unwrap(),
                &format!("rating={id}"),
                "camera=drone",
                &format!("timestamp=2026-02-0{id}T00:00:00Z"),
                &format!("label=sky-{id}"),
            ],
        ));
    }
    let audio_path = temp.path().join("audio.wav");
    write_wav(&audio_path);
    assert_success(&run(
        &database,
        [
            "ingest",
            audio_path.to_str().unwrap(),
            "rating=2",
            "camera=drone",
            "speaker=field",
        ],
    ));

    assert_find_ids(&database, "FIND IMAGE WHERE rating = 2", &[2]);
    assert_find_ids(&database, "FIND IMAGE WHERE rating != 2", &[1, 3]);
    assert_find_ids(&database, "FIND IMAGE WHERE rating < 2", &[1]);
    assert_find_ids(&database, "FIND IMAGE WHERE rating <= 2", &[1, 2]);
    assert_find_ids(&database, "FIND IMAGE WHERE rating > 2", &[3]);
    assert_find_ids(&database, "FIND IMAGE WHERE rating >= 2", &[2, 3]);
    assert_find_ids(
        &database,
        "FIND IMAGE WHERE timestamp BETWEEN '2026-02-02T00:00:00.000Z' AND '2026-02-03T00:00:00.000Z'",
        &[2, 3],
    );
    assert_find_ids(
        &database,
        "FIND IMAGE WHERE label CONTAINS 'sky-'",
        &[1, 2, 3],
    );
    assert_find(
        &database,
        "FIND IMAGE WHERE camera = 'drone' LIMIT 1",
        3,
        &[1],
    );
    assert_find_ids(&database, "FIND AUDIO WHERE rating = 2", &[4]);
    assert_find_ids(&database, "FIND IMAGE WHERE speaker = 'field'", &[]);
}

#[test]
fn cli_aliases_explicit_filter_first_and_repl_errors_work() {
    let temp = TempDir::new();
    let database = temp.path().join("db");
    let image_path = temp.path().join("image.png");
    write_png(&image_path, [20, 80, 220]);
    assert_success(&run(
        &database,
        ["ingest", image_path.to_str().unwrap(), "camera=drone"],
    ));

    let status_alias = assert_success(&run(&database, ["status"]));
    assert!(status_alias.contains("objects: 1"));

    let find_alias = assert_success(&run(
        &database,
        ["find", "FIND IMAGE WHERE camera = 'drone' LIMIT 1"],
    ));
    assert!(find_alias.contains("id=1 type=image"));

    let query_get = assert_success(&run(&database, ["query", "GET 1"]));
    assert!(query_get.contains("object_id: 1"));
    assert!(query_get.contains("media_type: image"));

    let filter_first = assert_success(&run(
        &database,
        [
            "--plan",
            "filter-first",
            "query",
            &format!(
                "FIND IMAGE WHERE camera = 'drone' SIMILAR TO '{}' LIMIT 1",
                image_path.display()
            ),
        ],
    ));
    assert!(filter_first.contains("plan=filter-first"));
    assert!(filter_first.contains("id=1 type=image"));

    let mut repl = Command::new(env!("CARGO_BIN_EXE_mmdb"))
        .arg("--db")
        .arg(&database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start interactive CLI");
    repl.stdin
        .take()
        .expect("interactive stdin")
        .write_all(b"not a query\nFIND IMAGE SIMILAR TO 'missing.png' LIMIT 1\nquit\n")
        .expect("send invalid interactive commands");
    let repl_output = repl.wait_with_output().expect("wait for interactive CLI");
    assert_success(&repl_output);
    let repl_stderr = stderr(&repl_output);
    assert!(repl_stderr.contains("parse error: expected FIND or GET"));
    assert!(repl_stderr.contains("query error: similarity query file not found"));

    let mut eof_repl = Command::new(env!("CARGO_BIN_EXE_mmdb"))
        .arg("--db")
        .arg(&database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start REPL for EOF test");
    drop(eof_repl.stdin.take().expect("interactive stdin"));
    let eof_output = eof_repl
        .wait_with_output()
        .expect("wait for REPL EOF handling");
    let eof_stdout = assert_success(&eof_output);
    assert!(eof_stdout.contains("MMDB image/audio catalog"));
}

#[test]
fn invalid_inputs_report_errors_and_repl_accepts_commands() {
    let temp = TempDir::new();
    let database = temp.path().join("db");
    let unsupported_path = temp.path().join("notes.txt");
    fs::write(&unsupported_path, "not media").unwrap();

    let invalid_metadata = run(
        &database,
        [
            "ingest",
            unsupported_path.to_str().unwrap(),
            "missing-equals",
        ],
    );
    assert_failure(&invalid_metadata, "metadata must use key=value syntax");

    let unsupported_media = run(&database, ["ingest", unsupported_path.to_str().unwrap()]);
    assert_failure(&unsupported_media, "unsupported media file");
    assert!(!database.join("data/catalog.json").exists());

    assert_failure(
        &run(&database, ["query", "not a query"]),
        "expected FIND or GET",
    );
    assert_failure(
        &run(&database, ["--plan", "unknown", "stats"]),
        "unknown plan",
    );

    let mut repl = Command::new(env!("CARGO_BIN_EXE_mmdb"))
        .arg("--db")
        .arg(&database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start interactive CLI");
    repl.stdin
        .take()
        .expect("interactive stdin")
        .write_all(b"FIND OBJECT LIMIT 3\nGET 99\nexit\n")
        .expect("send interactive commands");
    let repl_output = repl.wait_with_output().expect("wait for interactive CLI");
    let repl_stdout = assert_success(&repl_output);
    assert!(repl_stdout.contains("MMDB image/audio catalog"));
    assert!(repl_stdout.contains("matches=0"));
    assert!(repl_stdout.contains("No object with id 99"));
}
