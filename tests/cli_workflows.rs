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
    Command::new(env!("CARGO_BIN_EXE_mmdb"))
        .args(args)
        .arg("--db")
        .arg(database)
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
