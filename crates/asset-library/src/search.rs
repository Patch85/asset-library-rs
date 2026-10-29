//! Search query language, as in the original's `app/search.py`.
//!
//! ```text
//! goblin archer          text (name, folder path, notes, tags), prefix match, AND
//! "fire giant"           exact phrase
//! -skeleton              exclude text
//! tag:undead  -tag:boss  require / exclude a tag
//! type:token             asset type
//! source:3               source id
//! is:fav  is:untagged  is:missing  is:transparent
//! w>=512  w<300  h>=512  size in pixels
//! color:pink  -color:black   dominant colours
//! ~"winged lizard with a spear"   plain-language image search (AI extra)
//! similar:123                visually similar to asset 123 (AI extra)
//! ```

use std::sync::LazyLock;

use regex::Regex;
use rusqlite::types::Value;

use crate::db::{marks, normalize_tag};

/// Fraction of opaque pixels for `color:x` to match.
const COLOR_MIN: f64 = 0.12;

static TOKEN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(-?)(~?)(?:(\w+):)?("[^"]*"?|\S+)"#).unwrap());
static DIM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(-?)([wh])(>=|<=|>|<|=)(\d+)$").unwrap());

/// Sort name to ORDER BY clause, in the order the UI lists them.
pub const SORTS: &[(&str, &str)] = &[
    ("name", "a.name COLLATE NOCASE ASC, a.id ASC"),
    ("newest", "a.added_at DESC, a.id DESC"),
    ("oldest", "a.added_at ASC, a.id ASC"),
    ("path", "a.source_id, a.rel_path COLLATE NOCASE"),
    ("largest", "a.width * a.height DESC, a.id ASC"),
];

pub fn order_by(sort: &str) -> &'static str {
    SORTS.iter().find(|(k, _)| *k == sort).unwrap_or(&SORTS[0]).1
}

#[derive(Debug, Default, PartialEq)]
pub struct Query {
    pub text: Vec<String>,
    pub not_text: Vec<String>,
    pub tags: Vec<String>,
    pub not_tags: Vec<String>,
    pub types: Vec<String>,
    pub sources: Vec<i64>,
    pub flags: Vec<String>,
    pub not_flags: Vec<String>,
    /// (column, operator, pixels)
    pub dims: Vec<(&'static str, String, i64)>,
    pub colors: Vec<String>,
    pub not_colors: Vec<String>,
    pub semantic: Vec<String>,
    pub similar: Option<i64>,
}

impl Query {
    pub fn ranked(&self) -> bool {
        !self.semantic.is_empty() || self.similar.is_some()
    }
}

fn digits(s: &str) -> Option<i64> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

pub fn parse(q: &str) -> Query {
    let mut out = Query::default();
    for m in TOKEN_RE.captures_iter(q) {
        let neg = !m[1].is_empty();
        let key = m.get(3).map_or(String::new(), |k| k.as_str().to_lowercase());
        let raw = &m[4];
        if !m[2].is_empty() {
            let v = raw.trim_matches('"');
            if !v.is_empty() {
                out.semantic.push(v.to_string());
            }
            continue;
        }
        if key.is_empty()
            && let Some(d) = DIM_RE.captures(&m[0])
        {
            if let Ok(n) = d[4].parse() {
                let col = if &d[2] == "w" { "width" } else { "height" };
                out.dims.push((col, d[3].to_string(), n));
            }
            continue;
        }
        let val = raw.trim_matches('"');
        if val.is_empty() {
            continue;
        }
        match key.as_str() {
            "tag" => (if neg { &mut out.not_tags } else { &mut out.tags }).push(normalize_tag(val)),
            "type" => out.types.push(val.to_lowercase()),
            "source" if digits(val).is_some() => out.sources.push(digits(val).unwrap()),
            "color" => {
                (if neg { &mut out.not_colors } else { &mut out.colors }).push(val.to_lowercase())
            }
            "similar" if digits(val).is_some() => out.similar = digits(val),
            "is" => (if neg { &mut out.not_flags } else { &mut out.flags }).push(val.to_lowercase()),
            _ => {
                let text = if key.is_empty() { val.to_string() } else { format!("{key}:{val}") };
                (if neg { &mut out.not_text } else { &mut out.text }).push(text);
            }
        }
    }
    out
}

/// Quote a term for FTS5; single words become prefix matches.
fn fts_term(term: &str) -> String {
    let quoted = format!("\"{}\"", term.replace('"', "\"\""));
    if term.contains(' ') { quoted } else { quoted + "*" }
}

fn flag_sql(flag: &str) -> Option<&'static str> {
    Some(match flag {
        "fav" | "favorite" => "a.favorite = 1",
        "missing" => "a.missing = 1",
        "transparent" => "a.has_alpha = 1",
        "untagged" => "NOT EXISTS (SELECT 1 FROM asset_tags x WHERE x.asset_id = a.id)",
        _ => return None,
    })
}

/// (WHERE clause, params) selecting from `assets a`.
pub fn build_sql(q: &Query, include_missing: bool) -> (String, Vec<Value>) {
    let mut wh: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if !q.text.is_empty() {
        wh.push("a.id IN (SELECT rowid FROM assets_fts WHERE assets_fts MATCH ?)".into());
        params.push(q.text.iter().map(|t| fts_term(t)).collect::<Vec<_>>().join(" ").into());
    }
    for t in &q.not_text {
        wh.push("a.id NOT IN (SELECT rowid FROM assets_fts WHERE assets_fts MATCH ?)".into());
        params.push(fts_term(t).into());
    }

    let tag_sql = "EXISTS (SELECT 1 FROM asset_tags x JOIN tags t ON t.id = x.tag_id \
                   WHERE x.asset_id = a.id AND t.name = ?)";
    for tag in &q.tags {
        wh.push(tag_sql.into());
        params.push(tag.clone().into());
    }
    for tag in &q.not_tags {
        wh.push(format!("NOT {tag_sql}"));
        params.push(tag.clone().into());
    }

    if !q.types.is_empty() {
        wh.push(format!("a.asset_type IN ({})", marks(q.types.len())));
        params.extend(q.types.iter().map(|t| t.clone().into()));
    }
    if !q.sources.is_empty() {
        wh.push(format!("a.source_id IN ({})", marks(q.sources.len())));
        params.extend(q.sources.iter().map(|&s| s.into()));
    }

    wh.extend(q.flags.iter().filter_map(|f| flag_sql(f)).map(String::from));
    wh.extend(q.not_flags.iter().filter_map(|f| flag_sql(f)).map(|s| format!("NOT ({s})")));
    for (col, op, n) in &q.dims {
        wh.push(format!("a.{col} {op} ?"));
        params.push((*n).into());
    }

    let color_sql = "EXISTS (SELECT 1 FROM image_features f WHERE f.file_hash = a.file_hash \
                     AND json_extract(f.colors, ?) >= ?)";
    for c in q.colors.iter().chain(&q.not_colors) {
        wh.push(if q.colors.contains(c) { color_sql.into() } else { format!("NOT {color_sql}") });
        params.push(format!("$.{c}").into());
        params.push(COLOR_MIN.into());
    }

    if !include_missing && !q.flags.iter().any(|f| f == "missing") {
        wh.push("a.missing = 0".into());
    }

    let clause = if wh.is_empty() { "1".to_string() } else { wh.join(" AND ") };
    (clause, params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mixed_query() {
        let q = parse(r#"Goblin "fire giant" -skel tag:Low Level -tag:boss w>=500 is:fav source:3 foo:bar ~"red dragon" similar:x"#);
        assert_eq!(q.text, ["Goblin", "fire giant", "Level", "foo:bar", "similar:x"]);
        assert_eq!(q.not_text, ["skel"]);
        assert_eq!(q.tags, ["low"]);
        assert_eq!(q.not_tags, ["boss"]);
        assert_eq!(q.dims, [("width", ">=".to_string(), 500)]);
        assert_eq!(q.flags, ["fav"]);
        assert_eq!(q.sources, [3]);
        assert_eq!(q.semantic, ["red dragon"]);
        assert_eq!(q.similar, None);
    }

    #[test]
    fn unterminated_quote_and_terms() {
        assert_eq!(parse(r#"bad "quote"#).text, ["bad", "quote"]);
        assert_eq!(fts_term("skel"), "\"skel\"*");
        assert_eq!(fts_term("fire giant"), "\"fire giant\"");
        assert_eq!(fts_term(r#"a"b"#), r#""a""b"*"#);
    }

    #[test]
    fn missing_flag_lifts_default_filter() {
        assert_eq!(build_sql(&parse(""), false).0, "a.missing = 0");
        assert_eq!(build_sql(&parse("is:missing"), false).0, "a.missing = 1");
        assert_eq!(build_sql(&parse(""), true).0, "1");
    }
}
