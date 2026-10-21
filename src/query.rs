use crate::catalog::{Catalog, MediaObject, Store};
use crate::index::{cosine_similarity, object_value, LshIndex, MetadataIndex};
use crate::media::{self, MediaType};
use serde_json::Value;
use std::cmp::Ordering;
use std::path::PathBuf;
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
        .ok_or_else(|| "FIND requires IMAGE, AUDIO, or OBJECT".to_owned())?;
    let media_type = match media.to_ascii_uppercase().as_str() {
        "IMAGE" => Some(MediaType::Image),
        "AUDIO" => Some(MediaType::Audio),
        "OBJECT" => None,
        _ => return Err(format!("unsupported media type '{media}'; use IMAGE, AUDIO, or OBJECT")),
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

fn resolve_query_path(path: &PathBuf, store: &Store) -> Result<PathBuf, String> {
    if path.is_file() {
        return Ok(path.clone());
    }
    let in_database = store.resolve_location(&path.to_string_lossy());
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
                if shortest.map_or(true, |current| positions.len() < current.len()) {
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
