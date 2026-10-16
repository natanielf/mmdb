use crate::catalog::{Catalog, MediaObject};
use crate::media::MediaType;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

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

impl Default for MetadataIndex {
    fn default() -> Self {
        Self {
            fields: Vec::new(),
            entries: 0,
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
    vector_count: usize,
    image_count: usize,
    audio_count: usize,
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
        let image = build_modality(catalog, &image_positions, 0x1a2b_3c4d_55aa_7721);
        let audio = build_modality(catalog, &audio_positions, 0x7f4a_7c15_9e37_79b9);
        Self {
            image,
            audio,
            vector_count: image_positions.len() + audio_positions.len(),
            image_count: image_positions.len(),
            audio_count: audio_positions.len(),
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
        }
    }

    pub fn bucket_count(&self) -> usize {
        self.image
            .iter()
            .chain(self.audio.iter())
            .flat_map(|modality| modality.tables.iter())
            .map(|table| table.buckets.len())
            .sum()
    }

    pub fn size_estimate_bytes(&self) -> usize {
        self.image
            .iter()
            .chain(self.audio.iter())
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
