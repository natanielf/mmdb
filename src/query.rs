use crate::catalog::{Catalog, MediaObject, Store};
use crate::index::{cosine_similarity, object_value, LshIndex, MetadataIndex};
use crate::media::{self, MediaType};
use serde_json::Value;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Predicate {
    pub field: String,
    pub operator: Operator,
    pub value: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operator {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    Between,
}

#[derive(Clone, Debug)]
pub struct FindQuery {
    pub media_type: Option<MediaType>,
    pub predicates: Vec<Predicate>,
    pub similar_to: Option<PathBuf>,
    pub limit: usize,
}

#[derive(Clone, Debug)]
pub enum Command {
    Find(FindQuery),
    Get(u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Plan {
    Auto,
    Indexed,
    Scan,
    Exact,
    Ann,
    FilterFirst,
    VectorFirst,
}

impl Plan {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "index" | "indexed" => Ok(Self::Indexed),
            "scan" | "full-scan" => Ok(Self::Scan),
            "exact" => Ok(Self::Exact),
            "ann" | "approx" => Ok(Self::Ann),
            "filter-first" => Ok(Self::FilterFirst),
            "vector-first" => Ok(Self::VectorFirst),
            _ => Err(format!(
                "unknown plan '{value}'; use auto, indexed, scan, exact, ann, filter-first, or vector-first"
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Indexed => "indexed",
            Self::Scan => "full-scan",
            Self::Exact => "exact",
            Self::Ann => "ann",
            Self::FilterFirst => "filter-first",
            Self::VectorFirst => "vector-first",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub object: MediaObject,
    pub similarity: Option<f32>,
}

#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub hits: Vec<SearchHit>,
    pub plan: Plan,
    pub candidates_examined: usize,
    pub elapsed_micros: u128,
    pub total_matches_before_limit: usize,
}

pub fn parse(input: &str) -> Result<Command, String> {
    let tokens = tokenize(input)?;
    let mut parser = Parser { tokens, cursor: 0 };
    let command = parser
        .next_word()
        .ok_or_else(|| "expected FIND or GET".to_owned())?;
    if command.eq_ignore_ascii_case("GET") {
        let id = parser
            .next_literal()
            .ok_or_else(|| "GET requires an object identifier".to_owned())?;
        let id = id
            .as_u64()
            .or_else(|| id.as_str().and_then(|value| value.parse::<u64>().ok()))
            .ok_or_else(|| "GET identifier must be an unsigned integer".to_owned())?;
        parser.ensure_finished()?;
        return Ok(Command::Get(id));
    }
    if !command.eq_ignore_ascii_case("FIND") {
        return Err("expected FIND or GET".to_owned());
    }

    let media = parser
        .next_word()
        .ok_or_else(|| "FIND requires IMAGE, AUDIO, VIDEO, or OBJECT".to_owned())?;
    let media_type = match media.to_ascii_uppercase().as_str() {
        "IMAGE" => Some(MediaType::Image),
        "AUDIO" => Some(MediaType::Audio),
        "VIDEO" => Some(MediaType::Video),
        "OBJECT" => None,
        _ => return Err(format!("unsupported media type '{media}'; use IMAGE, AUDIO, VIDEO, or OBJECT")),
    };
    let mut query = FindQuery {
        media_type,
        predicates: Vec::new(),
        similar_to: None,
        limit: 10,
    };

    while parser.has_next() {
        if parser.consume_word("WHERE") {
            loop {
                let field = parser
                    .next_word()
                    .ok_or_else(|| "expected a metadata field after WHERE".to_owned())?
                    .to_ascii_lowercase();
                let operator = if let Some(keyword) = parser.next_word_keyword() {
                    match keyword.to_ascii_uppercase().as_str() {
                        "CONTAINS" => Operator::Contains,
                        "BETWEEN" => Operator::Between,
                        _ => return Err(format!("unsupported operator '{keyword}' after field '{field}'")),
                    }
                } else {
                    parser
                        .next_operator()
                        .ok_or_else(|| "expected comparison operator after field".to_owned())?
                };
                let value = match operator {
                    Operator::Contains => parser
                        .next_literal()
                        .ok_or_else(|| "CONTAINS requires a quoted or literal value".to_owned())?,
                    Operator::Between => {
                        let low = parser
                            .next_literal()
                            .ok_or_else(|| "BETWEEN requires a lower bound value".to_owned())?;
                        if !parser.consume_word("AND") {
                            return Err("BETWEEN requires an AND between lower and upper bounds".to_owned());
                        }
                        let high = parser
                            .next_literal()
                            .ok_or_else(|| "BETWEEN requires an upper bound value".to_owned())?;
                        Value::Array(vec![low, high])
                    }
                    _ => parser
                        .next_literal()
                        .ok_or_else(|| "expected a value after comparison operator".to_owned())?,
                };
                query.predicates.push(Predicate {
                    field,
                    operator,
                    value,
                });
                if !parser.consume_word("AND") {
                    break;
                }
            }
        } else if parser.consume_word("SIMILAR") {
            if !parser.consume_word("TO") {
                return Err("expected TO after SIMILAR".to_owned());
            }
            let value = parser
                .next_literal()
                .ok_or_else(|| "SIMILAR TO requires a quoted file path".to_owned())?
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "SIMILAR TO requires a quoted file path".to_owned())?;
            query.similar_to = Some(PathBuf::from(value));
        } else if parser.consume_word("LIMIT") {
            let value = parser
                .next_literal()
                .ok_or_else(|| "LIMIT requires a positive integer".to_owned())?;
            query.limit = value
                .as_u64()
                .or_else(|| value.as_str().and_then(|s| s.parse::<u64>().ok()))
                .filter(|limit| *limit > 0)
                .ok_or_else(|| "LIMIT requires a positive integer".to_owned())?
                .min(usize::MAX as u64) as usize;
        } else {
            return Err("unexpected token; expected WHERE, SIMILAR TO, or LIMIT".to_owned());
        }
    }
    Ok(Command::Find(query))
}

pub fn execute(
    catalog: &Catalog,
    metadata_index: &MetadataIndex,
    lsh_index: &LshIndex,
    store: &Store,
    query: &FindQuery,
    requested_plan: Plan,
) -> Result<ExecutionResult, String> {
    let start = Instant::now();
    if query.similar_to.is_none()
        && matches!(
            requested_plan,
            Plan::Exact | Plan::Ann | Plan::FilterFirst | Plan::VectorFirst
        )
    {
        return Err(format!(
            "plan '{}' requires a SIMILAR TO clause",
            requested_plan.name()
        ));
    }
    let resolved = query
        .similar_to
        .as_ref()
        .map(|path| resolve_query_path(path, store))
        .transpose()?;
    let analyzed = resolved
        .as_ref()
        .map(|path| media::analyze(path))
        .transpose()?;
    let media_type = query
        .media_type
        .or_else(|| analyzed.as_ref().map(|analysis| analysis.media_type));
    if let (Some(expected), Some(actual)) = (media_type, analyzed.as_ref()) {
        if expected != actual.media_type {
            return Err(format!(
                "query file is {}, but FIND specifies {}",
                actual.media_type.as_str(),
                expected.as_str()
            ));
        }
    }
    let effective_plan = choose_plan(requested_plan, query);

    let filtered: Vec<usize>;
    let mut candidates_examined;
    let positions = match effective_plan {
        Plan::Scan => {
            candidates_examined = catalog.objects.len();
            filtered = (0..catalog.objects.len())
                .filter(|position| {
                    object_matches(
                        &catalog.objects[*position],
                        media_type,
                        &query.predicates,
                    )
                })
                .collect();
            filtered.as_slice()
        }
        Plan::VectorFirst | Plan::Ann if analyzed.is_some() => {
            let analysis = analyzed.as_ref().expect("checked above");
            let Some(kind) = media_type else {
                return Err("similarity search requires IMAGE or AUDIO".to_owned());
            };
            let wanted = query
                .limit
                .saturating_mul(12)
                .max(64)
                .min(lsh_index.vector_count_for(kind));
            let vector_candidates =
                lsh_index.candidate_positions(&analysis.embedding, kind, wanted);
            candidates_examined = vector_candidates.len();
            filtered = vector_candidates
                .into_iter()
                .filter(|position| {
                    object_matches(
                        &catalog.objects[*position],
                        media_type,
                        &query.predicates,
                    )
                })
                .collect();
            filtered.as_slice()
        }
        _ => {
            let indexed = indexed_candidates(
                catalog,
                metadata_index,
                media_type,
                &query.predicates,
            );
            candidates_examined = indexed.len();
            filtered = indexed
                .into_iter()
                .filter(|position| {
                    object_matches(
                        &catalog.objects[*position],
                        media_type,
                        &query.predicates,
                    )
                })
                .collect();
            filtered.as_slice()
        }
    };

    let mut scored: Vec<(usize, Option<f32>)> = if let Some(analysis) = analyzed.as_ref() {
        if !matches!(effective_plan, Plan::Ann | Plan::VectorFirst) {
            candidates_examined = positions.len();
        }
        positions
            .iter()
            .map(|position| {
                (
                    *position,
                    Some(cosine_similarity(
                        &catalog.objects[*position].embedding,
                        &analysis.embedding,
                    )),
                )
            })
            .collect()
    } else {
        positions.iter().map(|position| (*position, None)).collect()
    };
    if analyzed.is_some() {
        scored.sort_by(|(left_position, left_score), (right_position, right_score)| {
            (*right_score)
                .unwrap_or(0.0)
                .partial_cmp(&(*left_score).unwrap_or(0.0))
                .unwrap_or(Ordering::Equal)
                .then_with(|| {
                    catalog.objects[*left_position]
                        .object_id
                        .cmp(&catalog.objects[*right_position].object_id)
                })
        });
    } else {
        scored.sort_by_key(|(position, _)| catalog.objects[*position].object_id);
    }
    let total_matches_before_limit = scored.len();
    let hits = scored
        .into_iter()
        .take(query.limit)
        .map(|(position, similarity)| SearchHit {
            object: catalog.objects[position].clone(),
            similarity,
        })
        .collect();
    Ok(ExecutionResult {
        hits,
        plan: effective_plan,
        candidates_examined,
        elapsed_micros: start.elapsed().as_micros(),
        total_matches_before_limit,
    })
}

fn choose_plan(requested: Plan, query: &FindQuery) -> Plan {
    if requested != Plan::Auto {
        return requested;
    }
    if query.similar_to.is_some() {
        if query.predicates.is_empty() {
            Plan::Ann
        } else {
            Plan::FilterFirst
        }
    } else {
        Plan::Indexed
    }
}

fn resolve_query_path(path: &Path, store: &Store) -> Result<PathBuf, String> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    let in_database = store.resolve_location(path.to_string_lossy().as_ref());
    if in_database.is_file() {
        return Ok(in_database);
    }
    Err(format!("similarity query file not found: {}", path.display()))
}

fn indexed_candidates(
    catalog: &Catalog,
    index: &MetadataIndex,
    media_type: Option<MediaType>,
    predicates: &[Predicate],
) -> Vec<usize> {
    let mut shortest: Option<&[usize]> = None;
    if let Some(kind) = media_type {
        let value = Value::String(kind.as_str().to_owned());
        let Some(positions) = index.equality("media_type", &value) else {
            return Vec::new();
        };
        shortest = Some(positions);
    }
    for predicate in predicates {
        if predicate.operator == Operator::Eq {
            if let Some(positions) = index.equality(&predicate.field, &predicate.value) {
                if shortest.is_none_or(|current| positions.len() < current.len()) {
                    shortest = Some(positions);
                }
            }
        }
    }
    shortest
        .map(|positions| positions.to_vec())
        .unwrap_or_else(|| (0..catalog.objects.len()).collect())
}

fn object_matches(
    object: &MediaObject,
    media_type: Option<MediaType>,
    predicates: &[Predicate],
) -> bool {
    if media_type.is_some_and(|kind| object.media_type != kind) {
        return false;
    }
    predicates.iter().all(|predicate| {
        let Some(actual) = object_value(object, &predicate.field) else {
            return false;
        };
        match predicate.operator {
            Operator::Eq => compare(&actual, &predicate.value) == Some(Ordering::Equal),
            Operator::Ne => compare(&actual, &predicate.value) != Some(Ordering::Equal),
            Operator::Lt => compare(&actual, &predicate.value) == Some(Ordering::Less),
            Operator::Le => matches!(compare(&actual, &predicate.value), Some(Ordering::Less | Ordering::Equal)),
            Operator::Gt => compare(&actual, &predicate.value) == Some(Ordering::Greater),
            Operator::Ge => matches!(compare(&actual, &predicate.value), Some(Ordering::Greater | Ordering::Equal)),
            Operator::Contains => {
                let (Some(actual_string), Some(expected)) = (actual.as_str(), predicate.value.as_str()) else {
                    return false;
                };
                actual_string.contains(expected)
            }
            Operator::Between => {
                let Some(bounds) = predicate.value.as_array() else {
                    return false;
                };
                if bounds.len() != 2 {
                    return false;
                }
                let Some(lower) = bounds.first() else {
                    return false;
                };
                let Some(upper) = bounds.get(1) else {
                    return false;
                };
                let lower_ok = matches!(compare(&actual, lower), Some(Ordering::Equal | Ordering::Greater));
                let upper_ok = matches!(compare(&actual, upper), Some(Ordering::Equal | Ordering::Less));
                lower_ok && upper_ok
            }
        }
    })
}

fn compare(left: &Value, right: &Value) -> Option<Ordering> {
    if let (Some(a), Some(b)) = (left.as_f64(), right.as_f64()) {
        return a.partial_cmp(&b);
    }
    if let (Some(a), Some(b)) = (left.as_str(), right.as_str()) {
        return Some(a.cmp(b));
    }
    if let (Some(a), Some(b)) = (left.as_bool(), right.as_bool()) {
        return Some(a.cmp(&b));
    }
    if left.is_null() && right.is_null() {
        return Some(Ordering::Equal);
    }
    None
}

#[derive(Debug)]
enum Token {
    Atom(String),
    Quoted(String),
    Operator(String),
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut cursor = 0;
    let mut tokens = Vec::new();
    while cursor < chars.len() {
        if chars[cursor].is_whitespace() {
            cursor += 1;
            continue;
        }
        if chars[cursor] == '\'' || chars[cursor] == '"' {
            let quote = chars[cursor];
            cursor += 1;
            let mut value = String::new();
            let mut closed = false;
            while cursor < chars.len() {
                let ch = chars[cursor];
                cursor += 1;
                if ch == quote {
                    closed = true;
                    break;
                }
                if ch == '\\' && cursor < chars.len() {
                    let next = chars[cursor];
                    if next == quote || next == '\\' {
                        value.push(next);
                        cursor += 1;
                        continue;
                    }
                }
                value.push(ch);
            }
            if !closed {
                return Err("unterminated quoted string".to_owned());
            }
            tokens.push(Token::Quoted(value));
            continue;
        }
        if matches!(chars[cursor], '=' | '!' | '<' | '>') {
            let first = chars[cursor];
            cursor += 1;
            let mut operator = first.to_string();
            if cursor < chars.len() && chars[cursor] == '=' {
                operator.push('=');
                cursor += 1;
            } else if first == '!' {
                return Err("expected != comparison operator".to_owned());
            }
            tokens.push(Token::Operator(operator));
            continue;
        }
        let start = cursor;
        while cursor < chars.len()
            && !chars[cursor].is_whitespace()
            && !matches!(chars[cursor], '=' | '!' | '<' | '>' | '\'' | '"')
        {
            cursor += 1;
        }
        if cursor == start {
            return Err(format!("unexpected character '{}'", chars[cursor]));
        }
        tokens.push(Token::Atom(chars[start..cursor].iter().collect()));
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    cursor: usize,
}

impl Parser {
    fn has_next(&self) -> bool {
        self.cursor < self.tokens.len()
    }

    fn next_word(&mut self) -> Option<String> {
        match self.tokens.get(self.cursor)? {
            Token::Atom(value) => {
                self.cursor += 1;
                Some(value.clone())
            }
            _ => None,
        }
    }

    fn consume_word(&mut self, expected: &str) -> bool {
        if matches!(self.tokens.get(self.cursor), Some(Token::Atom(value)) if value.eq_ignore_ascii_case(expected))
        {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn next_word_keyword(&mut self) -> Option<String> {
        let value = match self.tokens.get(self.cursor)? {
            Token::Atom(value) => value.clone(),
            _ => return None,
        };
        if value.eq_ignore_ascii_case("CONTAINS") || value.eq_ignore_ascii_case("BETWEEN") {
            self.cursor += 1;
            Some(value)
        } else {
            None
        }
    }

    fn next_operator(&mut self) -> Option<Operator> {
        let value = match self.tokens.get(self.cursor)? {
            Token::Operator(value) => value.as_str(),
            _ => return None,
        };
        let operator = match value {
            "=" => Operator::Eq,
            "!=" => Operator::Ne,
            "<" => Operator::Lt,
            "<=" => Operator::Le,
            ">" => Operator::Gt,
            ">=" => Operator::Ge,
            _ => return None,
        };
        self.cursor += 1;
        Some(operator)
    }

    fn next_literal(&mut self) -> Option<Value> {
        let token = self.tokens.get(self.cursor)?;
        self.cursor += 1;
        match token {
            Token::Quoted(value) => Some(Value::String(value.clone())),
            Token::Atom(value) => Some(
                serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.clone())),
            ),
            Token::Operator(_) => None,
        }
    }

    fn ensure_finished(&self) -> Result<(), String> {
        if self.has_next() {
            Err("unexpected trailing tokens".to_owned())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::MediaObject;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_TEMP_DIR.fetch_add(1, AtomicOrdering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mmdb-query-tests-{}-{id}", std::process::id()));
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

    struct Fixture {
        _directory: TempDir,
        store: Store,
        catalog: Catalog,
        metadata_index: MetadataIndex,
        lsh_index: LshIndex,
        query_path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = TempDir::new();
            let query_path = directory.path().join("query.png");
            let red_path = directory.path().join("red.png");
            let blue_path = directory.path().join("blue.png");
            let green_path = directory.path().join("green.png");
            let red_embedding = write_image(&red_path, [240, 20, 20]);
            let blue_embedding = write_image(&blue_path, [20, 20, 240]);
            let green_embedding = write_image(&green_path, [20, 240, 20]);
            write_image(&query_path, [240, 20, 20]);

            let catalog = Catalog {
                objects: vec![
                    MediaObject {
                        object_id: 20,
                        media_type: MediaType::Image,
                        physical_location: "data/objects/red.png".to_owned(),
                        size: 10,
                        timestamp: "2026-01-01T00:00:00Z".to_owned(),
                        metadata: BTreeMap::from([
                            ("camera".to_owned(), json!("drone")),
                            ("rating".to_owned(), json!(4)),
                            ("label".to_owned(), json!("north ridge")),
                        ]),
                        embedding: red_embedding,
                        segment_embeddings: vec![],
                    },
                    MediaObject {
                        object_id: 10,
                        media_type: MediaType::Image,
                        physical_location: "data/objects/blue.png".to_owned(),
                        size: 20,
                        timestamp: "2026-01-02T00:00:00Z".to_owned(),
                        metadata: BTreeMap::from([
                            ("camera".to_owned(), json!("tripod")),
                            ("rating".to_owned(), json!(8)),
                            ("label".to_owned(), json!("east ridge")),
                        ]),
                        embedding: blue_embedding,
                        segment_embeddings: vec![],
                    },
                    MediaObject {
                        object_id: 30,
                        media_type: MediaType::Image,
                        physical_location: "data/objects/green.png".to_owned(),
                        size: 30,
                        timestamp: "2026-01-03T00:00:00Z".to_owned(),
                        metadata: BTreeMap::from([
                            ("camera".to_owned(), json!("drone")),
                            ("rating".to_owned(), json!(7)),
                            ("label".to_owned(), json!("south ridge")),
                        ]),
                        embedding: green_embedding,
                        segment_embeddings: vec![],
                    },
                ],
                next_id: 31,
                ..Catalog::default()
            };
            let metadata_index = MetadataIndex::build(&catalog);
            let lsh_index = LshIndex::build(&catalog);
            let store = Store::new(directory.path().join("database"));
            Self {
                _directory: directory,
                store,
                catalog,
                metadata_index,
                lsh_index,
                query_path,
            }
        }
    }

    fn write_image(path: &Path, color: [u8; 3]) -> Vec<f32> {
        let mut image = image::RgbImage::new(4, 4);
        for pixel in image.pixels_mut() {
            *pixel = image::Rgb(color);
        }
        image.save(path).unwrap();
        crate::media::analyze(path).unwrap().embedding
    }

    fn find_query(media_type: Option<MediaType>, predicates: Vec<Predicate>) -> FindQuery {
        FindQuery {
            media_type,
            predicates,
            similar_to: None,
            limit: 10,
        }
    }

    fn execute_fixture(
        fixture: &Fixture,
        query: &FindQuery,
        plan: Plan,
    ) -> Result<ExecutionResult, String> {
        execute(
            &fixture.catalog,
            &fixture.metadata_index,
            &fixture.lsh_index,
            &fixture.store,
            query,
            plan,
        )
    }

    #[test]
    fn plan_parse_accepts_names_aliases_and_case_insensitively() {
        for (name, plan) in [
            ("AUTO", Plan::Auto),
            ("index", Plan::Indexed),
            ("indexed", Plan::Indexed),
            ("scan", Plan::Scan),
            ("full-scan", Plan::Scan),
            ("exact", Plan::Exact),
            ("ann", Plan::Ann),
            ("approx", Plan::Ann),
            ("filter-first", Plan::FilterFirst),
            ("vector-first", Plan::VectorFirst),
        ] {
            assert_eq!(Plan::parse(name).unwrap(), plan, "{name}");
        }
    }

    #[test]
    fn plan_name_returns_canonical_names() {
        assert_eq!(Plan::Auto.name(), "auto");
        assert_eq!(Plan::Indexed.name(), "indexed");
        assert_eq!(Plan::Scan.name(), "full-scan");
        assert_eq!(Plan::Exact.name(), "exact");
        assert_eq!(Plan::Ann.name(), "ann");
        assert_eq!(Plan::FilterFirst.name(), "filter-first");
        assert_eq!(Plan::VectorFirst.name(), "vector-first");
    }

    #[test]
    fn plan_parse_rejects_unknown_names() {
        assert!(Plan::parse("random").unwrap_err().contains("unknown plan"));
    }

    #[test]
    fn parse_get_accepts_numeric_and_quoted_unsigned_ids() {
        assert!(matches!(parse("GET 42").unwrap(), Command::Get(42)));
        assert!(matches!(parse("get '0007'").unwrap(), Command::Get(7)));
    }

    #[test]
    fn parse_get_rejects_invalid_or_trailing_values() {
        for statement in ["GET", "GET -1", "GET 1.5", "GET 1 trailing"] {
            assert!(parse(statement).is_err(), "{statement}");
        }
    }

    #[test]
    fn parse_find_defaults_and_media_types() {
        let Command::Find(object_query) = parse("FIND object").unwrap() else {
            panic!("expected FIND command");
        };
        assert_eq!(object_query.media_type, None);
        assert_eq!(object_query.limit, 10);
        assert!(object_query.predicates.is_empty());

        let Command::Find(image_query) = parse("find IMAGE").unwrap() else {
            panic!("expected FIND command");
        };
        assert_eq!(image_query.media_type, Some(MediaType::Image));

        let Command::Find(audio_query) = parse("FIND audio").unwrap() else {
            panic!("expected FIND command");
        };
        assert_eq!(audio_query.media_type, Some(MediaType::Audio));

        let Command::Find(video_query) = parse("FIND video").unwrap() else {
            panic!("expected FIND command");
        };
        assert_eq!(video_query.media_type, Some(MediaType::Video));
    }

    #[test]
    fn parse_find_handles_comparison_contains_between_similarity_and_limit() {
        let Command::Find(query) = parse(
            "FIND IMAGE WHERE Camera = 'drone' AND rating >= 4 AND label CONTAINS 'ridge' \
             AND timestamp BETWEEN '2026-01-01' AND '2026-12-31' \
             SIMILAR TO 'query image.png' LIMIT 25",
        )
        .unwrap() else {
            panic!("expected FIND command");
        };

        assert_eq!(query.media_type, Some(MediaType::Image));
        assert_eq!(query.predicates.len(), 4);
        assert_eq!(query.predicates[0].field, "camera");
        assert_eq!(query.predicates[0].operator, Operator::Eq);
        assert_eq!(query.predicates[0].value, json!("drone"));
        assert_eq!(query.predicates[1].operator, Operator::Ge);
        assert_eq!(query.predicates[1].value, json!(4));
        assert_eq!(query.predicates[2].operator, Operator::Contains);
        assert_eq!(query.predicates[3].operator, Operator::Between);
        assert_eq!(
            query.predicates[3].value,
            json!(["2026-01-01", "2026-12-31"])
        );
        assert_eq!(query.similar_to, Some(PathBuf::from("query image.png")));
        assert_eq!(query.limit, 25);
    }

    #[test]
    fn parse_supports_all_comparison_operators_and_string_limits() {
        for (operator, expected) in [
            ("=", Operator::Eq),
            ("!=", Operator::Ne),
            ("<", Operator::Lt),
            ("<=", Operator::Le),
            (">", Operator::Gt),
            (">=", Operator::Ge),
        ] {
            let statement = format!("FIND OBJECT WHERE value {operator} 3");
            let Command::Find(query) = parse(&statement).unwrap() else {
                panic!("expected FIND command");
            };
            assert_eq!(query.predicates[0].operator, expected, "{operator}");
        }

        let Command::Find(video_query) = parse("FIND VIDEO LIMIT '7'").unwrap() else {
            panic!("expected FIND command");
        };
        assert_eq!(video_query.media_type, Some(MediaType::Video));
        assert_eq!(video_query.limit, 7);
    }

    #[test]
    fn parse_unescapes_matching_quotes_and_backslashes() {
        let Command::Find(query) = parse(r#"FIND IMAGE WHERE note = 'a\'b\\c'"#).unwrap() else {
            panic!("expected FIND command");
        };
        assert_eq!(query.predicates[0].value, json!("a'b\\c"));
    }

    #[test]
    fn parse_rejects_invalid_commands_and_media_types() {
        for statement in ["", "SELECT IMAGE", "FIND"] {
            assert!(parse(statement).is_err(), "{statement}");
        }
    }

    #[test]
    fn parse_rejects_missing_or_unsupported_predicate_parts() {
        for statement in [
            "FIND IMAGE WHERE",
            "FIND IMAGE WHERE rating",
            "FIND IMAGE WHERE rating == 3",
            "FIND IMAGE WHERE rating =",
            "FIND IMAGE WHERE rating APPROX 3",
            "FIND IMAGE WHERE label CONTAINS",
            "FIND IMAGE WHERE rating BETWEEN 1",
            "FIND IMAGE WHERE rating BETWEEN 1 OR 3",
            "FIND IMAGE WHERE rating BETWEEN 1 AND",
        ] {
            assert!(parse(statement).is_err(), "{statement}");
        }
    }

    #[test]
    fn parse_rejects_invalid_similarity_clauses_and_limits() {
        for statement in [
            "FIND IMAGE SIMILAR",
            "FIND IMAGE SIMILAR IMAGE",
            "FIND IMAGE SIMILAR TO",
            "FIND IMAGE SIMILAR TO 5",
            "FIND IMAGE LIMIT",
            "FIND IMAGE LIMIT 0",
            "FIND IMAGE LIMIT -1",
            "FIND IMAGE LIMIT 1.5",
            "FIND IMAGE LIMIT 1 trailing",
        ] {
            assert!(parse(statement).is_err(), "{statement}");
        }
    }

    #[test]
    fn parse_rejects_unterminated_strings_and_bare_bang() {
        assert!(parse("FIND IMAGE WHERE name = 'open")
            .unwrap_err()
            .contains("unterminated"));
        assert!(parse("FIND IMAGE WHERE rating ! 2")
            .unwrap_err()
            .contains("expected !="));
    }

    #[test]
    fn compare_supports_numeric_text_boolean_and_null_values() {
        assert_eq!(compare(&json!(1), &json!(2)), Some(Ordering::Less));
        assert_eq!(compare(&json!(2.5), &json!(2)), Some(Ordering::Greater));
        assert_eq!(compare(&json!("a"), &json!("b")), Some(Ordering::Less));
        assert_eq!(compare(&json!(false), &json!(true)), Some(Ordering::Less));
        assert_eq!(compare(&Value::Null, &Value::Null), Some(Ordering::Equal));
        assert_eq!(compare(&json!(1), &json!("1")), None);
        assert_eq!(compare(&json!(false), &json!(0)), None);
    }

    #[test]
    fn object_matches_applies_all_predicate_types_and_media_filter() {
        let fixture = Fixture::new();
        let object = &fixture.catalog.objects[0];
        let predicates = vec![
            Predicate {
                field: "rating".to_owned(),
                operator: Operator::Ge,
                value: json!(4),
            },
            Predicate {
                field: "rating".to_owned(),
                operator: Operator::Le,
                value: json!(4),
            },
            Predicate {
                field: "camera".to_owned(),
                operator: Operator::Ne,
                value: json!("tripod"),
            },
            Predicate {
                field: "label".to_owned(),
                operator: Operator::Contains,
                value: json!("ridge"),
            },
            Predicate {
                field: "rating".to_owned(),
                operator: Operator::Between,
                value: json!([4, 5]),
            },
        ];

        assert!(object_matches(object, Some(MediaType::Image), &predicates));
        assert!(!object_matches(object, Some(MediaType::Audio), &predicates));
        assert!(!object_matches(
            object,
            None,
            &[Predicate {
                field: "missing".to_owned(),
                operator: Operator::Ne,
                value: json!(1),
            }]
        ));
    }

    #[test]
    fn object_matches_rejects_invalid_contains_and_between_operands() {
        let fixture = Fixture::new();
        let object = &fixture.catalog.objects[0];

        for predicate in [
            Predicate {
                field: "rating".to_owned(),
                operator: Operator::Contains,
                value: json!("4"),
            },
            Predicate {
                field: "rating".to_owned(),
                operator: Operator::Between,
                value: json!([4]),
            },
            Predicate {
                field: "rating".to_owned(),
                operator: Operator::Between,
                value: json!([5, 6]),
            },
        ] {
            assert!(!object_matches(object, None, &[predicate]));
        }
    }

    #[test]
    fn choose_plan_selects_defaults_and_preserves_explicit_choice() {
        let metadata_only = find_query(Some(MediaType::Image), Vec::new());
        let mut similarity = metadata_only.clone();
        similarity.similar_to = Some(PathBuf::from("query.png"));
        let mut hybrid = similarity.clone();
        hybrid.predicates.push(Predicate {
            field: "camera".to_owned(),
            operator: Operator::Eq,
            value: json!("drone"),
        });

        assert_eq!(choose_plan(Plan::Auto, &metadata_only), Plan::Indexed);
        assert_eq!(choose_plan(Plan::Auto, &similarity), Plan::Ann);
        assert_eq!(choose_plan(Plan::Auto, &hybrid), Plan::FilterFirst);
        assert_eq!(choose_plan(Plan::Scan, &metadata_only), Plan::Scan);
    }

    #[test]
    fn resolve_query_path_prefers_existing_path_then_database_relative_path() {
        let fixture = Fixture::new();
        let direct = resolve_query_path(&fixture.query_path, &fixture.store).unwrap();
        assert_eq!(direct, fixture.query_path);

        let relative = PathBuf::from("data/query.png");
        let database_path = fixture.store.resolve_location("data/query.png");
        fs::create_dir_all(database_path.parent().unwrap()).unwrap();
        fs::copy(&fixture.query_path, &database_path).unwrap();
        assert_eq!(
            resolve_query_path(&relative, &fixture.store).unwrap(),
            database_path
        );
    }

    #[test]
    fn resolve_query_path_reports_missing_file() {
        let fixture = Fixture::new();

        let error = resolve_query_path(&PathBuf::from("missing.png"), &fixture.store).unwrap_err();

        assert!(error.contains("similarity query file not found"));
    }

    #[test]
    fn indexed_candidates_narrows_by_media_and_equality_predicates() {
        let fixture = Fixture::new();
        let all_drone_images = indexed_candidates(
            &fixture.catalog,
            &fixture.metadata_index,
            Some(MediaType::Image),
            &[Predicate {
                field: "camera".to_owned(),
                operator: Operator::Eq,
                value: json!("drone"),
            }],
        );
        let all = indexed_candidates(
            &fixture.catalog,
            &fixture.metadata_index,
            None,
            &[Predicate {
                field: "rating".to_owned(),
                operator: Operator::Gt,
                value: json!(0),
            }],
        );
        let absent = indexed_candidates(
            &fixture.catalog,
            &fixture.metadata_index,
            None,
            &[Predicate {
                field: "camera".to_owned(),
                operator: Operator::Eq,
                value: json!("absent"),
            }],
        );

        assert_eq!(all_drone_images, vec![0, 2]);
        assert_eq!(all, vec![0, 1, 2]);
        assert_eq!(absent, vec![0, 1, 2]);
    }

    #[test]
    fn execute_runs_metadata_queries_with_scan_and_indexed_plans() {
        let fixture = Fixture::new();
        let query = find_query(
            Some(MediaType::Image),
            vec![
                Predicate {
                    field: "camera".to_owned(),
                    operator: Operator::Eq,
                    value: json!("drone"),
                },
                Predicate {
                    field: "rating".to_owned(),
                    operator: Operator::Gt,
                    value: json!(5),
                },
            ],
        );

        for plan in [Plan::Auto, Plan::Indexed, Plan::Scan] {
            let result = execute_fixture(&fixture, &query, plan).unwrap();
            assert_eq!(result.hits.len(), 1, "{plan:?}");
            assert_eq!(result.hits[0].object.object_id, 30, "{plan:?}");
            assert_eq!(result.total_matches_before_limit, 1, "{plan:?}");
            assert_eq!(
                result.plan,
                if plan == Plan::Auto {
                    Plan::Indexed
                } else {
                    plan
                }
            );
            assert_eq!(result.hits[0].similarity, None);
        }
    }

    #[test]
    fn execute_sorts_metadata_hits_by_id_and_applies_limit() {
        let fixture = Fixture::new();
        let mut query = find_query(None, Vec::new());
        query.limit = 2;

        let result = execute_fixture(&fixture, &query, Plan::Scan).unwrap();

        assert_eq!(
            result
                .hits
                .iter()
                .map(|hit| hit.object.object_id)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
        assert_eq!(result.total_matches_before_limit, 3);
        assert_eq!(result.candidates_examined, 3);
    }

    #[test]
    fn execute_rejects_similarity_plans_without_similarity_clause() {
        let fixture = Fixture::new();
        let query = find_query(None, Vec::new());

        for plan in [Plan::Exact, Plan::Ann, Plan::FilterFirst, Plan::VectorFirst] {
            assert!(
                execute_fixture(&fixture, &query, plan)
                    .unwrap_err()
                    .contains("requires a SIMILAR TO clause"),
                "{plan:?}"
            );
        }
    }

    #[test]
    fn execute_similarity_ranks_exact_matches_and_supports_all_plans() {
        let fixture = Fixture::new();
        let mut query = find_query(Some(MediaType::Image), Vec::new());
        query.similar_to = Some(fixture.query_path.clone());

        for plan in [
            Plan::Auto,
            Plan::Exact,
            Plan::Ann,
            Plan::FilterFirst,
            Plan::VectorFirst,
        ] {
            let result = execute_fixture(&fixture, &query, plan).unwrap();
            assert!(!result.hits.is_empty(), "{plan:?}");
            assert_eq!(result.hits[0].object.object_id, 20, "{plan:?}");
            assert!(result.hits[0].similarity.unwrap() > 0.999, "{plan:?}");
            assert_eq!(
                result.plan,
                if plan == Plan::Auto { Plan::Ann } else { plan }
            );
        }
    }

    #[test]
    fn execute_similarity_infers_modality_for_object_search() {
        let fixture = Fixture::new();
        let mut query = find_query(None, Vec::new());
        query.similar_to = Some(fixture.query_path.clone());

        let result = execute_fixture(&fixture, &query, Plan::Exact).unwrap();

        assert_eq!(result.hits[0].object.object_id, 20);
        assert!(result.hits.iter().all(|hit| hit.similarity.is_some()));
    }

    #[test]
    fn execute_similarity_respects_hybrid_filter_and_tie_breaking() {
        let fixture = Fixture::new();
        let mut query = find_query(
            Some(MediaType::Image),
            vec![Predicate {
                field: "camera".to_owned(),
                operator: Operator::Eq,
                value: json!("drone"),
            }],
        );
        query.similar_to = Some(fixture.query_path.clone());

        let exact = execute_fixture(&fixture, &query, Plan::Exact).unwrap();
        let filter_first = execute_fixture(&fixture, &query, Plan::FilterFirst).unwrap();

        assert_eq!(
            exact
                .hits
                .iter()
                .map(|hit| hit.object.object_id)
                .collect::<Vec<_>>(),
            vec![20, 30]
        );
        assert_eq!(
            filter_first
                .hits
                .iter()
                .map(|hit| hit.object.object_id)
                .collect::<Vec<_>>(),
            vec![20, 30]
        );
        assert_eq!(filter_first.plan, Plan::FilterFirst);
    }

    #[test]
    fn execute_reports_missing_query_path_and_modality_mismatch() {
        let fixture = Fixture::new();
        let mut missing = find_query(Some(MediaType::Image), Vec::new());
        missing.similar_to = Some(PathBuf::from("missing.png"));
        assert!(execute_fixture(&fixture, &missing, Plan::Exact)
            .unwrap_err()
            .contains("similarity query file not found"));

        let mut mismatch = find_query(Some(MediaType::Audio), Vec::new());
        mismatch.similar_to = Some(fixture.query_path.clone());
        assert!(execute_fixture(&fixture, &mismatch, Plan::Exact)
            .unwrap_err()
            .contains("query file is image, but FIND specifies audio"));
    }
}
