use crate::media::{self, MediaType};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MediaObject {
    pub object_id: u64,
    pub media_type: MediaType,
    pub physical_location: String,
    pub size: u64,
    pub timestamp: String,
    pub metadata: BTreeMap<String, Value>,
    pub embedding: Vec<f32>,
    pub segment_embeddings: Vec<Vec<f32>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default = "schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub next_id: u64,
    #[serde(default)]
    pub objects: Vec<MediaObject>,
}

fn schema_version() -> u32 {
    1
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            schema_version: schema_version(),
            next_id: 1,
            objects: Vec::new(),
        }
    }
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn catalog_path(&self) -> PathBuf {
        self.root.join("data").join("catalog.json")
    }

    pub fn load(&self) -> Result<Catalog, String> {
        let path = self.catalog_path();
        if !path.exists() {
            return Ok(Catalog::default());
        }
        let bytes = fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| format!("invalid catalog {}: {e}", path.display()))
    }

    pub fn save(&self, catalog: &Catalog) -> Result<(), String> {
        let path = self.catalog_path();
        let parent = path.parent().ok_or("catalog path has no parent directory")?;
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        let bytes = serde_json::to_vec_pretty(catalog)
            .map_err(|e| format!("cannot serialize catalog: {e}"))?;
        fs::write(&path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    pub fn ingest(
        &self,
        catalog: &mut Catalog,
        source: &Path,
        mut user_metadata: BTreeMap<String, Value>,
    ) -> Result<MediaObject, String> {
        if !source.is_file() {
            return Err(format!("media file does not exist: {}", source.display()));
        }
        let analysis = media::analyze(source)?;
        let size = fs::metadata(source)
            .map_err(|e| format!("cannot inspect {}: {e}", source.display()))?
            .len();
        let id = catalog.next_id.max(1);
        let timestamp = match user_metadata.remove("timestamp") {
            Some(Value::String(value)) => chrono::DateTime::parse_from_rfc3339(&value)
                .map(|timestamp| {
                    timestamp
                        .with_timezone(&chrono::Utc)
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                })
                .map_err(|e| format!("timestamp must be RFC 3339: {e}"))?,
            Some(_) => return Err("timestamp must be an RFC 3339 string".to_owned()),
            None => chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };

        let extension = source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let relative = PathBuf::from("data")
            .join("objects")
            .join(format!("{id:08}.{extension}"));
        let destination = self.root.join(&relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        fs::copy(source, &destination).map_err(|e| {
            format!(
                "cannot copy {} to {}: {e}",
                source.display(),
                destination.display()
            )
        })?;

        let mut metadata = analysis.metadata;
        metadata.insert(
            "name".to_owned(),
            Value::String(
                source
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("media")
                    .to_owned(),
            ),
        );
        metadata.extend(user_metadata);

        let object = MediaObject {
            object_id: id,
            media_type: analysis.media_type,
            physical_location: relative.to_string_lossy().replace('\\', "/"),
            size,
            timestamp,
            metadata,
            embedding: analysis.embedding,
            segment_embeddings: analysis.segment_embeddings,
        };
        catalog.objects.push(object.clone());
        catalog.next_id = id.saturating_add(1);
        self.save(catalog)?;
        Ok(object)
    }

    pub fn resolve_location(&self, location: &str) -> PathBuf {
        self.root.join(location)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::MediaType;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("mmdb-catalog-tests-{}-{id}", std::process::id()));
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

    fn write_png(path: &Path) {
        let mut image = image::RgbImage::new(2, 2);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgb([30, 120, 220]);
        }
        image
            .save_with_format(path, image::ImageFormat::Png)
            .unwrap();
    }

    #[test]
    fn catalog_default_starts_empty_at_id_one() {
        let catalog = Catalog::default();

        assert_eq!(catalog.schema_version, 1);
        assert_eq!(catalog.next_id, 1);
        assert!(catalog.objects.is_empty());
    }

    #[test]
    fn store_paths_are_rooted_under_data() {
        let store = Store::new("db-root");

        assert_eq!(store.root(), Path::new("db-root"));
        assert_eq!(
            store.catalog_path(),
            PathBuf::from("db-root").join("data").join("catalog.json")
        );
        assert_eq!(
            store.resolve_location("data/objects/one.png"),
            PathBuf::from("db-root")
                .join("data")
                .join("objects")
                .join("one.png")
        );
    }

    #[test]
    fn load_missing_catalog_returns_default_without_creating_files() {
        let directory = TempDir::new();
        let store = Store::new(directory.path());

        let catalog = store.load().unwrap();

        assert_eq!(catalog.schema_version, 1);
        assert_eq!(catalog.next_id, 1);
        assert!(catalog.objects.is_empty());
        assert!(!store.catalog_path().exists());
    }

    #[test]
    fn save_and_load_round_trip_catalog_data() {
        let directory = TempDir::new();
        let store = Store::new(directory.path());
        let catalog = Catalog {
            schema_version: 3,
            next_id: 9,
            objects: vec![MediaObject {
                object_id: 8,
                media_type: MediaType::Image,
                physical_location: "data/objects/00000008.png".to_owned(),
                size: 23,
                timestamp: "2026-01-02T03:04:05Z".to_owned(),
                metadata: BTreeMap::from([("camera".to_owned(), json!("drone"))]),
                embedding: vec![0.25, 0.75],
                segment_embeddings: vec![],
            }],
        };

        store.save(&catalog).unwrap();
        let loaded = store.load().unwrap();

        assert_eq!(loaded.schema_version, catalog.schema_version);
        assert_eq!(loaded.next_id, catalog.next_id);
        assert_eq!(loaded.objects.len(), 1);
        assert_eq!(loaded.objects[0].object_id, 8);
        assert_eq!(loaded.objects[0].media_type, MediaType::Image);
        assert_eq!(loaded.objects[0].size, 23);
        assert_eq!(loaded.objects[0].metadata["camera"], json!("drone"));
        assert_eq!(loaded.objects[0].embedding, vec![0.25, 0.75]);
    }

    #[test]
    fn load_applies_defaults_for_older_catalog_fields() {
        let directory = TempDir::new();
        let store = Store::new(directory.path());
        fs::create_dir_all(store.catalog_path().parent().unwrap()).unwrap();
        fs::write(store.catalog_path(), br#"{"objects":[]}"#).unwrap();

        let catalog = store.load().unwrap();

        assert_eq!(catalog.schema_version, 1);
        assert_eq!(catalog.next_id, 0);
        assert!(catalog.objects.is_empty());
    }

    #[test]
    fn load_reports_invalid_catalog_json() {
        let directory = TempDir::new();
        let store = Store::new(directory.path());
        fs::create_dir_all(store.catalog_path().parent().unwrap()).unwrap();
        fs::write(store.catalog_path(), b"{invalid").unwrap();

        let error = store.load().unwrap_err();

        assert!(error.contains("invalid catalog"));
        assert!(error.contains("catalog.json"));
    }

    #[test]
    fn ingest_copies_image_assigns_id_metadata_and_persists() {
        let directory = TempDir::new();
        let source = directory.path().join("source.PNG");
        write_png(&source);
        let database = directory.path().join("database");
        let store = Store::new(&database);
        let mut catalog = Catalog::default();
        let metadata = BTreeMap::from([
            ("camera".to_owned(), json!("drone")),
            ("timestamp".to_owned(), json!("2025-02-03T04:05:06+02:00")),
        ]);

        let object = store.ingest(&mut catalog, &source, metadata).unwrap();

        assert_eq!(object.object_id, 1);
        assert_eq!(object.media_type, MediaType::Image);
        assert_eq!(object.size, fs::metadata(&source).unwrap().len());
        assert_eq!(object.physical_location, "data/objects/00000001.png");
        assert_eq!(object.timestamp, "2025-02-03T02:05:06.000Z");
        assert_eq!(object.metadata["camera"], json!("drone"));
        assert_eq!(object.metadata["name"], json!("source.PNG"));
        assert_eq!(object.embedding.len(), 32);
        assert_eq!(
            fs::read(store.resolve_location(&object.physical_location)).unwrap(),
            fs::read(&source).unwrap()
        );
        assert_eq!(catalog.next_id, 2);
        assert_eq!(catalog.objects.len(), 1);
        assert_eq!(store.load().unwrap().objects[0].object_id, 1);
    }

    #[test]
    fn ingest_advances_ids_and_uses_current_time_when_absent() {
        let directory = TempDir::new();
        let source = directory.path().join("image.png");
        write_png(&source);
        let store = Store::new(directory.path().join("database"));
        let mut catalog = Catalog {
            next_id: 5,
            ..Catalog::default()
        };

        let object = store
            .ingest(&mut catalog, &source, BTreeMap::new())
            .unwrap();

        assert_eq!(object.object_id, 5);
        assert_eq!(catalog.next_id, 6);
        assert!(chrono::DateTime::parse_from_rfc3339(&object.timestamp).is_ok());
    }

    #[test]
    fn ingest_rejects_missing_source_and_does_not_mutate_catalog() {
        let directory = TempDir::new();
        let store = Store::new(directory.path().join("database"));
        let mut catalog = Catalog::default();

        let error = store
            .ingest(
                &mut catalog,
                &directory.path().join("missing.png"),
                BTreeMap::new(),
            )
            .unwrap_err();

        assert!(error.contains("media file does not exist"));
        assert!(catalog.objects.is_empty());
        assert_eq!(catalog.next_id, 1);
    }

    #[test]
    fn ingest_rejects_invalid_timestamp_without_mutating_catalog() {
        let directory = TempDir::new();
        let source = directory.path().join("image.png");
        write_png(&source);
        let store = Store::new(directory.path().join("database"));
        let mut catalog = Catalog::default();
        let metadata = BTreeMap::from([("timestamp".to_owned(), json!("not a timestamp"))]);

        let error = store.ingest(&mut catalog, &source, metadata).unwrap_err();

        assert!(error.contains("timestamp must be RFC 3339"));
        assert!(catalog.objects.is_empty());
        assert_eq!(catalog.next_id, 1);
        assert!(!store.catalog_path().exists());
    }

    #[test]
    fn ingest_rejects_non_string_timestamp() {
        let directory = TempDir::new();
        let source = directory.path().join("image.png");
        write_png(&source);
        let store = Store::new(directory.path().join("database"));
        let mut catalog = Catalog::default();

        let error = store
            .ingest(
                &mut catalog,
                &source,
                BTreeMap::from([("timestamp".to_owned(), json!(42))]),
            )
            .unwrap_err();

        assert!(error.contains("timestamp must be an RFC 3339 string"));
        assert!(catalog.objects.is_empty());
    }

    #[test]
    fn ingest_rejects_unsupported_media() {
        let directory = TempDir::new();
        let source = directory.path().join("clip.bin");
        fs::write(&source, b"not a supported media file").unwrap();
        let store = Store::new(directory.path().join("database"));
        let mut catalog = Catalog::default();

        let error = store
            .ingest(&mut catalog, &source, BTreeMap::new())
            .unwrap_err();

        assert!(error.contains("unsupported media file"));
        assert!(catalog.objects.is_empty());
    }
}
