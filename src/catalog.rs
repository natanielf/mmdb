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

