use crate::catalog::{Catalog, MediaObject};
use crate::media::MediaType;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct MetadataIndex {
    fields: Vec<(String, ValueHashIndex)>,
    entries: usize,
}

struct ValueHashIndex {
    buckets: Vec<Vec<(String, Vec<usize>)>>,
    distinct_values: usize,
}

impl ValueHashIndex {
    fn new() -> Self {
        Self {
            buckets: (0..32).map(|_| Vec::new()).collect(),
            distinct_values: 0,
        }
    }

    fn insert(&mut self, key: String, position: usize) {
        let slot = hash_key(&key) as usize % self.buckets.len();
        if let Some(existing_position) = self.buckets[slot]
            .iter()
            .position(|(existing, _)| existing == &key)
        {
            self.buckets[slot][existing_position].1.push(position);
            return;
        }
        self.buckets[slot].push((key, vec![position]));
        self.distinct_values += 1;
        if self.distinct_values * 4 > self.buckets.len() * 3 {
            self.grow();
        }
    }

    fn get(&self, key: &str) -> Option<&[usize]> {
        let slot = hash_key(key) as usize % self.buckets.len();
        self.buckets[slot]
            .iter()
            .find(|(existing, _)| existing.as_str() == key)
            .map(|(_, positions)| positions.as_slice())
    }

    fn grow(&mut self) {
        let new_capacity = self.buckets.len() * 2;
        let old_buckets = std::mem::replace(
            &mut self.buckets,
            (0..new_capacity).map(|_| Vec::new()).collect(),
        );
        for (key, positions) in old_buckets.into_iter().flatten() {
            let slot = hash_key(&key) as usize % new_capacity;
            self.buckets[slot].push((key, positions));
        }
    }
}

impl MetadataIndex {
    pub fn build(catalog: &Catalog) -> Self {
        let mut index = Self::default();
        for (position, object) in catalog.objects.iter().enumerate() {
            let mut fields = object.metadata.clone();
            if let Some(duration) = object.metadata.get("duration_seconds").cloned() {
                fields.entry("duration".to_owned()).or_insert(duration);
            }
            fields.insert("object_id".to_owned(), Value::from(object.object_id));
            fields.insert("id".to_owned(), Value::from(object.object_id));
            fields.insert(
                "media_type".to_owned(),
                Value::String(object.media_type.as_str().to_owned()),
            );
            fields.insert(
                "physical_location".to_owned(),
                Value::String(object.physical_location.clone()),
            );
            fields.insert("size".to_owned(), Value::from(object.size));
            fields.insert(
                "timestamp".to_owned(),
                Value::String(object.timestamp.clone()),
            );
            for (field, value) in fields {
                index.insert(&field, value_key(&value), position);
                index.entries += 1;
            }
        }
        index
    }

    pub fn equality(&self, field: &str, value: &Value) -> Option<&[usize]> {
        let (_, values) = self
            .fields
            .iter()
            .find(|(name, _)| name.as_str() == field)?;
        values.get(&value_key(value))
    }

    pub fn entry_count(&self) -> usize {
        self.entries
    }

    pub fn size_estimate_bytes(&self) -> usize {
        let key_bytes: usize = self
            .fields
            .iter()
            .map(|(field, values)| {
                field.len()
                    + values
                        .buckets
                        .iter()
                        .map(|bucket| {
                            bucket
                                .iter()
                                .map(|(value, positions)| value.len() + positions.len() * 8)
                                .sum::<usize>()
                                + 24
                        })
                        .sum::<usize>()
            })
            .sum();
        key_bytes + self.fields.len() * 64
    }

    fn insert(&mut self, field: &str, key: String, position: usize) {
        let field_position = self
            .fields
            .iter()
            .position(|(name, _)| name.as_str() == field);
        let index = if let Some(field_position) = field_position {
            &mut self.fields[field_position].1
        } else {
            self.fields
                .push((field.to_owned(), ValueHashIndex::new()));
            &mut self.fields.last_mut().expect("just inserted").1
        };
        index.insert(key, position);
    }
}

fn value_key(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
}

fn hash_key(key: &str) -> u64 {
    // FNV-1a over canonical JSON value text; collisions are resolved by bucket chaining.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

struct LshTable {
    projections: Vec<Vec<f32>>,
    buckets: HashMap<u64, Vec<usize>>,
}

struct ModalityLsh {
    bits: usize,
    dimensions: usize,
    tables: Vec<LshTable>,
}

pub struct LshIndex {
    image: Option<ModalityLsh>,
    audio: Option<ModalityLsh>,
    video: Option<ModalityLsh>,
    vector_count: usize,
    image_count: usize,
    audio_count: usize,
    video_count: usize,
}

impl LshIndex {
    pub const TABLE_COUNT: usize = 8;
    pub const BITS_PER_TABLE: usize = 12;

    pub fn build(catalog: &Catalog) -> Self {
        let image_positions: Vec<usize> = catalog
            .objects
            .iter()
            .enumerate()
            .filter_map(|(position, object)| {
                (object.media_type == MediaType::Image).then_some(position)
            })
            .collect();
        let audio_positions: Vec<usize> = catalog
            .objects
            .iter()
            .enumerate()
            .filter_map(|(position, object)| {
                (object.media_type == MediaType::Audio).then_some(position)
            })
            .collect();
        let video_positions: Vec<usize> = catalog
            .objects
            .iter()
            .enumerate()
            .filter_map(|(position, object)| {
                (object.media_type == MediaType::Video).then_some(position)
            })
            .collect();
        let image = build_modality(catalog, &image_positions, 0x1a2b_3c4d_55aa_7721);
        let audio = build_modality(catalog, &audio_positions, 0x7f4a_7c15_9e37_79b9);
        let video = build_modality(catalog, &video_positions, 0x9b7c_1d44_3b9f_88c2);
        Self {
            image,
            audio,
            video,
            vector_count: image_positions.len() + audio_positions.len() + video_positions.len(),
            image_count: image_positions.len(),
            audio_count: audio_positions.len(),
            video_count: video_positions.len(),
        }
    }

    pub fn candidate_positions(
        &self,
        query: &[f32],
        media_type: MediaType,
        wanted: usize,
    ) -> Vec<usize> {
        let Some(index) = self.modality(media_type) else {
            return Vec::new();
        };
        if index.tables.is_empty() || index.dimensions != query.len() {
            return Vec::new();
        }

        let mut candidates = HashSet::new();
        let masks: Vec<u64> = (0..(1_u64 << index.bits))
            .filter(|mask| mask.count_ones() <= 2)
            .collect();
        for radius in 0..=2 {
            for table in &index.tables {
                let signature = signature(query, &table.projections);
                for mask in masks
                    .iter()
                    .filter(|mask| (**mask).count_ones() == radius)
                {
                    if let Some(positions) = table.buckets.get(&(signature ^ *mask)) {
                        candidates.extend(positions.iter().copied());
                    }
                }
            }
            if candidates.len() >= wanted {
                break;
            }
        }

        // If the close buckets are sparse, inspect bucket keys by Hamming distance. This
        // remains an approximate search: only vectors in selected buckets are rescored.
        if candidates.len() < wanted {
            for table in &index.tables {
                let query_signature = signature(query, &table.projections);
                let mut nearby: Vec<(u32, u64)> = table
                    .buckets
                    .keys()
                    .map(|key| ((query_signature ^ *key).count_ones(), *key))
                    .collect();
                nearby.sort_unstable();
                for (_, key) in nearby {
                    if let Some(positions) = table.buckets.get(&key) {
                        candidates.extend(positions.iter().copied());
                    }
                    if candidates.len() >= wanted {
                        break;
                    }
                }
                if candidates.len() >= wanted {
                    break;
                }
            }
        }
        let mut result: Vec<usize> = candidates.into_iter().collect();
        result.sort_unstable();
        result
    }

    pub fn vector_count(&self) -> usize {
        self.vector_count
    }

    pub fn vector_count_for(&self, media_type: MediaType) -> usize {
        match media_type {
            MediaType::Image => self.image_count,
            MediaType::Audio => self.audio_count,
            MediaType::Video => self.video_count,
        }
    }

    pub fn bucket_count(&self) -> usize {
        self.image
            .iter()
            .chain(self.audio.iter())
            .chain(self.video.iter())
            .flat_map(|modality| modality.tables.iter())
            .map(|table| table.buckets.len())
            .sum()
    }

    pub fn size_estimate_bytes(&self) -> usize {
        self.image
            .iter()
            .chain(self.audio.iter())
            .chain(self.video.iter())
            .flat_map(|modality| modality.tables.iter())
            .map(|table| {
                table.projections.iter().map(|p| p.len() * 4).sum::<usize>()
                    + table
                        .buckets
                        .values()
                        .map(|ids| 8 + ids.len() * 8)
                        .sum::<usize>()
            })
            .sum()
    }

    fn modality(&self, media_type: MediaType) -> Option<&ModalityLsh> {
        match media_type {
            MediaType::Image => self.image.as_ref(),
            MediaType::Audio => self.audio.as_ref(),
            MediaType::Video => self.video.as_ref(),
        }
    }
}

fn build_modality(catalog: &Catalog, positions: &[usize], seed: u64) -> Option<ModalityLsh> {
    let first = catalog.objects.get(*positions.first()?)?;
    let dimensions = first.embedding.len();
    if dimensions == 0 {
        return None;
    }
    let mut state = seed;
    let mut tables = Vec::with_capacity(LshIndex::TABLE_COUNT);
    for _ in 0..LshIndex::TABLE_COUNT {
        let mut projections = Vec::with_capacity(LshIndex::BITS_PER_TABLE);
        for _ in 0..LshIndex::BITS_PER_TABLE {
            let mut projection: Vec<f32> = (0..dimensions)
                .map(|_| next_random(&mut state))
                .collect();
            let norm = projection.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
            for value in &mut projection {
                *value /= norm;
            }
            projections.push(projection);
        }
        let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
        for position in positions {
            let object = &catalog.objects[*position];
            if object.embedding.len() == dimensions {
                let key = signature(&object.embedding, &projections);
                buckets.entry(key).or_default().push(*position);
            }
        }
        tables.push(LshTable {
            projections,
            buckets,
        });
    }
    Some(ModalityLsh {
        bits: LshIndex::BITS_PER_TABLE,
        dimensions,
        tables,
    })
}

fn next_random(state: &mut u64) -> f32 {
    // Deterministic xorshift generator keeps index construction reproducible without a
    // dependency or a system RNG. The vectors are projection coefficients, not secrets.
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    let unit = ((*state >> 40) as f32) / ((1_u32 << 24) as f32);
    unit * 2.0 - 1.0
}

fn signature(vector: &[f32], projections: &[Vec<f32>]) -> u64 {
    projections
        .iter()
        .enumerate()
        .fold(0_u64, |signature, (bit, projection)| {
            let dot = vector
                .iter()
                .zip(projection.iter())
                .map(|(a, b)| a * b)
                .sum::<f32>();
            if dot >= 0.0 {
                signature | (1_u64 << bit)
            } else {
                signature
            }
        })
}

pub fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut left_norm = 0.0;
    let mut right_norm = 0.0;
    for (a, b) in left.iter().zip(right.iter()) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    let denominator = left_norm.sqrt() * right_norm.sqrt();
    if denominator <= f32::EPSILON {
        0.0
    } else {
        dot / denominator
    }
}

pub fn object_value(object: &MediaObject, field: &str) -> Option<Value> {
    match field.to_ascii_lowercase().as_str() {
        "id" | "object_id" => Some(Value::from(object.object_id)),
        "media_type" => Some(Value::String(object.media_type.as_str().to_owned())),
        "physical_location" => {
            Some(Value::String(object.physical_location.clone()))
        }
        "size" => Some(Value::from(object.size)),
        "timestamp" => Some(Value::String(object.timestamp.clone())),
        "duration" => object.metadata.get("duration_seconds").cloned(),
        metadata_field => object.metadata.get(metadata_field).cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn object(
        id: u64,
        media_type: MediaType,
        metadata: BTreeMap<String, Value>,
        embedding: Vec<f32>,
    ) -> MediaObject {
        MediaObject {
            object_id: id,
            media_type,
            physical_location: format!("data/{id}.media"),
            size: id * 10,
            timestamp: format!("2026-01-{id:02}T00:00:00Z"),
            metadata,
            embedding,
            segment_embeddings: vec![],
        }
    }

    fn catalog() -> Catalog {
        Catalog {
            objects: vec![
                object(
                    1,
                    MediaType::Image,
                    BTreeMap::from([
                        ("camera".to_owned(), json!("drone")),
                        ("duration_seconds".to_owned(), json!(12.5)),
                    ]),
                    vec![1.0, 0.0, 0.0],
                ),
                object(
                    2,
                    MediaType::Image,
                    BTreeMap::from([
                        ("camera".to_owned(), json!("drone")),
                        ("duration_seconds".to_owned(), json!(30.0)),
                    ]),
                    vec![0.8, 0.2, 0.0],
                ),
                object(
                    3,
                    MediaType::Audio,
                    BTreeMap::from([("speaker".to_owned(), json!("field"))]),
                    vec![0.0, 1.0],
                ),
            ],
            next_id: 4,
            ..Catalog::default()
        }
    }

    #[test]
    fn value_hash_index_resolves_bucket_collisions() {
        let mut keys_by_slot = HashMap::<usize, String>::new();
        let (first, second) = (0..1000)
            .map(|number| number.to_string())
            .find_map(|key| {
                let slot = hash_key(&key) as usize % 32;
                keys_by_slot
                    .insert(slot, key.clone())
                    .map(|first| (first, key))
            })
            .expect("find two keys with a matching bucket");
        let mut index = ValueHashIndex::new();
        index.insert(first.clone(), 7);
        index.insert(second.clone(), 11);

        assert_eq!(index.get(&first), Some(&[7][..]));
        assert_eq!(index.get(&second), Some(&[11][..]));
        assert_eq!(index.get("not present"), None);
    }

    #[test]
    fn value_hash_index_appends_duplicate_positions_and_grows() {
        let mut index = ValueHashIndex::new();
        index.insert("same".to_owned(), 1);
        index.insert("same".to_owned(), 4);
        for value in 0..100 {
            index.insert(format!("distinct-{value}"), value + 10);
        }

        assert_eq!(index.get("same"), Some(&[1, 4][..]));
        for value in 0..100 {
            assert_eq!(
                index.get(&format!("distinct-{value}")),
                Some(&[value + 10][..])
            );
        }
        assert!(index.buckets.len() > 32);
    }

    #[test]
    fn metadata_index_matches_values_and_synthesizes_catalog_fields() {
        let catalog = catalog();
        let index = MetadataIndex::build(&catalog);

        assert_eq!(index.equality("camera", &json!("drone")), Some(&[0, 1][..]));
        assert_eq!(
            index.equality("duration", &json!(12.5)),
            Some(&[0][..])
        );
        assert_eq!(index.equality("duration_seconds", &json!(30.0)), Some(&[1][..]));
        assert_eq!(index.equality("object_id", &json!(2)), Some(&[1][..]));
        assert_eq!(index.equality("id", &json!(3)), Some(&[2][..]));
        assert_eq!(
            index.equality("media_type", &json!("audio")),
            Some(&[2][..])
        );
        assert_eq!(
            index.equality("physical_location", &json!("data/1.media")),
            Some(&[0][..])
        );
        assert_eq!(index.equality("size", &json!(20)), Some(&[1][..]));
        assert_eq!(
            index.equality("timestamp", &json!("2026-01-03T00:00:00Z")),
            Some(&[2][..])
        );
        assert_eq!(index.equality("camera", &json!("missing")), None);
        assert_eq!(index.equality("missing", &json!("value")), None);
    }

    #[test]
    fn metadata_index_preserves_explicit_duration_alias_and_counts_entries() {
        let catalog = Catalog {
            objects: vec![object(
                5,
                MediaType::Audio,
                BTreeMap::from([
                    ("duration".to_owned(), json!(99)),
                    ("duration_seconds".to_owned(), json!(12)),
                ]),
                vec![0.1],
            )],
            ..Catalog::default()
        };
        let index = MetadataIndex::build(&catalog);

        assert_eq!(index.equality("duration", &json!(99)), Some(&[0][..]));
        assert_eq!(index.equality("duration", &json!(12)), None);
        assert_eq!(index.entry_count(), 8);
        assert!(index.size_estimate_bytes() > 0);
    }

    #[test]
    fn empty_metadata_index_has_no_entries_or_estimated_storage() {
        let index = MetadataIndex::build(&Catalog::default());

        assert_eq!(index.entry_count(), 0);
        assert_eq!(index.size_estimate_bytes(), 0);
    }

    #[test]
    fn lsh_index_tracks_modalities_and_returns_matching_dimension_candidates() {
        let catalog = catalog();
        let index = LshIndex::build(&catalog);

        assert_eq!(index.vector_count(), 3);
        assert_eq!(index.vector_count_for(MediaType::Image), 2);
        assert_eq!(index.vector_count_for(MediaType::Audio), 1);
        assert!(index.bucket_count() > 0);
        assert!(index.size_estimate_bytes() > 0);
        let image_candidates = index.candidate_positions(&[1.0, 0.0, 0.0], MediaType::Image, 1);
        assert!(image_candidates.contains(&0));
        assert!(image_candidates.iter().all(|position| *position < 2));
        assert_eq!(
            index.candidate_positions(&[0.0, 1.0], MediaType::Audio, 1),
            vec![2]
        );
        assert!(index
            .candidate_positions(&[1.0], MediaType::Image, 1)
            .is_empty());
    }

    #[test]
    fn lsh_index_ignores_zero_and_mismatched_dimensions() {
        let catalog = Catalog {
            objects: vec![
                object(2, MediaType::Image, BTreeMap::new(), vec![1.0, 0.0]),
                object(1, MediaType::Image, BTreeMap::new(), Vec::new()),
                object(3, MediaType::Image, BTreeMap::new(), vec![1.0]),
            ],
            ..Catalog::default()
        };
        let index = LshIndex::build(&catalog);

        assert_eq!(index.vector_count_for(MediaType::Image), 3);
        assert!(index.bucket_count() > 0);
        assert_eq!(
            index.candidate_positions(&[1.0, 0.0], MediaType::Image, 10),
            vec![0]
        );
    }

    #[test]
    fn lsh_index_without_a_modality_returns_no_candidates() {
        let index = LshIndex::build(&Catalog::default());

        assert_eq!(index.vector_count(), 0);
        assert_eq!(index.bucket_count(), 0);
        assert!(index
            .candidate_positions(&[1.0], MediaType::Audio, 1)
            .is_empty());
    }

    #[test]
    fn cosine_similarity_handles_aligned_orthogonal_and_invalid_vectors() {
        assert!((cosine_similarity(&[1.0, 2.0], &[1.0, 2.0]) - 1.0).abs() < f32::EPSILON);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < f32::EPSILON);
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < f32::EPSILON);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn object_value_reads_builtins_case_insensitively_and_duration_alias() {
        let object = object(
            7,
            MediaType::Audio,
            BTreeMap::from([
                ("duration_seconds".to_owned(), json!(8.5)),
                ("speaker".to_owned(), json!("field")),
            ]),
            vec![0.5],
        );

        assert_eq!(object_value(&object, "ID"), Some(json!(7)));
        assert_eq!(object_value(&object, "OBJECT_ID"), Some(json!(7)));
        assert_eq!(object_value(&object, "Media_Type"), Some(json!("audio")));
        assert_eq!(
            object_value(&object, "PHYSICAL_LOCATION"),
            Some(json!("data/7.media"))
        );
        assert_eq!(object_value(&object, "size"), Some(json!(70)));
        assert_eq!(
            object_value(&object, "timestamp"),
            Some(json!("2026-01-07T00:00:00Z"))
        );
        assert_eq!(object_value(&object, "duration"), Some(json!(8.5)));
        assert_eq!(object_value(&object, "SPEAKER"), Some(json!("field")));
        assert_eq!(object_value(&object, "missing"), None);
    }
}
