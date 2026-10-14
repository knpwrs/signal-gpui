//! Compiles the vendored emojibase data into:
//!
//! - `emoji_table.rs`: a static `[Emoji]` table in picker order plus the
//!   posting lists referenced by the term index.
//! - `terms.fst`: an FST map from normalized search term to a packed
//!   `(postings offset, postings len)` pair.
//! - `shortcodes.fst`: an FST map from exact shortcode to emoji index.
//!
//! Everything is computed at build time so the runtime index is a
//! zero-copy view over static bytes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::Value;

// Must match `TermKind` in src/search.rs.
const KIND_SHORTCODE: u32 = 0;
const KIND_NAME: u32 = 1;
const KIND_WORD: u32 = 2;
const KIND_TAG: u32 = 3;

const GROUPS: [&str; 10] = [
    "SmileysEmotion",
    "PeopleBody",
    "Component",
    "AnimalsNature",
    "FoodDrink",
    "TravelPlaces",
    "Activities",
    "Objects",
    "Symbols",
    "Flags",
];

const TONES: [&str; 5] = ["1F3FB", "1F3FC", "1F3FD", "1F3FE", "1F3FF"];

struct Entry {
    glyph: String,
    hexcode: String,
    name: String,
    group: usize,
    order: u64,
    version: f64,
    shortcodes: Vec<String>,
    tags: Vec<String>,
    skins: Option<[String; 5]>,
}

/// Lowercases and maps every run of separators to a single `_`, matching
/// `normalize` in src/search.rs.
fn normalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_sep = false;
    for c in s.chars() {
        if c.is_alphanumeric() || c == '+' {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.extend(c.to_lowercase());
        } else if c == '\'' || c == '’' || c == '.' {
            // "o’clock" -> "oclock", "st. lucia" -> "st_lucia"
            if c == '.' {
                pending_sep = true;
            }
        } else {
            pending_sep = true;
        }
    }
    out
}

fn shortcodes_from(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect(),
        _ => Vec::new(),
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let data_dir = manifest.join("data");
    for f in ["emojibase-en.json", "shortcodes-iamcal.json", "shortcodes-github.json"] {
        println!("cargo:rerun-if-changed=data/{f}");
    }
    println!("cargo:rerun-if-changed=build.rs");

    let read = |name: &str| -> Value {
        let text = std::fs::read_to_string(data_dir.join(name)).expect(name);
        serde_json::from_str(&text).expect(name)
    };
    let data = read("emojibase-en.json");
    let iamcal = read("shortcodes-iamcal.json");
    let github = read("shortcodes-github.json");

    let mut entries: Vec<Entry> = Vec::new();
    for e in data.as_array().unwrap() {
        let Some(group) = e.get("group").and_then(Value::as_u64) else {
            continue; // regional indicators etc.
        };
        let group = group as usize;
        if GROUPS[group] == "Component" {
            continue; // bare skin tone / hair swatches
        }
        let hexcode = e["hexcode"].as_str().unwrap().to_owned();

        // iamcal first: it's what Signal Desktop's emoji-datasource uses.
        let mut shortcodes: Vec<String> = Vec::new();
        for source in [&iamcal, &github] {
            for code in shortcodes_from(source.get(&hexcode)) {
                if !shortcodes.contains(&code) {
                    shortcodes.push(code);
                }
            }
        }

        let skins = e.get("skins").and_then(Value::as_array).and_then(|skins| {
            let mut out: [Option<String>; 5] = Default::default();
            for skin in skins {
                let hex = skin["hexcode"].as_str().unwrap();
                let tones: Vec<usize> = hex
                    .split('-')
                    .filter_map(|cp| TONES.iter().position(|t| *t == cp))
                    .collect();
                // Multi-person sequences have 25 variants; keep the uniform ones.
                if !tones.is_empty() && tones.iter().all(|t| *t == tones[0]) {
                    out[tones[0]].get_or_insert_with(|| skin["emoji"].as_str().unwrap().to_owned());
                }
            }
            if out.iter().all(Option::is_some) {
                Some(out.map(Option::unwrap))
            } else {
                None
            }
        });

        entries.push(Entry {
            glyph: e["emoji"].as_str().unwrap().to_owned(),
            hexcode,
            name: e["label"].as_str().unwrap().to_owned(),
            group,
            order: e["order"].as_u64().unwrap(),
            version: e["version"].as_f64().unwrap_or(0.0),
            shortcodes,
            tags: e
                .get("tags")
                .and_then(Value::as_array)
                .map(|t| t.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
                .unwrap_or_default(),
            skins,
        });
    }
    entries.sort_by_key(|e| e.order);
    assert!(entries.len() < 1 << 16, "emoji id must fit in u16");

    // term -> emoji id -> best kind for that emoji
    let mut terms: BTreeMap<String, BTreeMap<u32, u32>> = BTreeMap::new();
    let mut add = |term: String, id: u32, kind: u32| {
        if term.is_empty() {
            return;
        }
        let slot = terms.entry(term).or_default().entry(id).or_insert(kind);
        *slot = (*slot).min(kind);
    };
    let mut shortcode_ids: BTreeMap<String, u64> = BTreeMap::new();

    for (id, e) in entries.iter().enumerate() {
        let id = id as u32;
        for code in &e.shortcodes {
            let norm = normalize(code);
            add(norm.clone(), id, KIND_SHORTCODE);
            // `flag-sm` would make "sm" an exact word hit for San Marino; the
            // name ("flag: San Marino") already covers flags.
            if !norm.starts_with("flag_") {
                for part in norm.split('_') {
                    add(part.to_owned(), id, KIND_WORD);
                }
            }
            shortcode_ids.entry(code.clone()).or_insert(id as u64);
        }
        let name = normalize(&e.name);
        add(name.clone(), id, KIND_NAME);
        for word in name.split('_') {
            add(word.to_owned(), id, KIND_WORD);
        }
        for tag in &e.tags {
            let tag = normalize(tag);
            for word in tag.split('_') {
                add(word.to_owned(), id, KIND_TAG);
            }
            add(tag, id, KIND_TAG);
        }
    }

    // Postings: (emoji id << 8) | kind, grouped per term.
    let mut postings: Vec<u32> = Vec::new();
    let mut map = fst::MapBuilder::memory();
    for (term, ids) in &terms {
        let offset = postings.len() as u64;
        postings.extend(ids.iter().map(|(id, kind)| (id << 8) | kind));
        let len = ids.len() as u64;
        map.insert(term, (offset << 32) | len).unwrap();
    }
    let terms_fst = map.into_inner().unwrap();

    let mut codes = fst::MapBuilder::memory();
    for (code, id) in &shortcode_ids {
        codes.insert(code, *id).unwrap();
    }
    let shortcodes_fst = codes.into_inner().unwrap();

    // Rust table.
    let mut src = String::new();
    let lit = |s: &str| format!("{s:?}");
    let list = |v: &[String]| {
        let mut out = String::from("&[");
        for s in v {
            write!(out, "{s:?},").unwrap();
        }
        out.push(']');
        out
    };
    writeln!(src, "pub(crate) static EMOJI: [Emoji; {}] = [", entries.len()).unwrap();
    for (id, e) in entries.iter().enumerate() {
        let skins = match &e.skins {
            Some(s) => format!(
                "Some([{}, {}, {}, {}, {}])",
                lit(&s[0]),
                lit(&s[1]),
                lit(&s[2]),
                lit(&s[3]),
                lit(&s[4])
            ),
            None => "None".into(),
        };
        writeln!(
            src,
            "Emoji {{ id: {id}, glyph: {}, hexcode: {}, name: {}, group: Group::{}, version: {:?}f32, shortcodes: {}, tags: {}, skins: {} }},",
            lit(&e.glyph),
            lit(&e.hexcode),
            lit(&e.name),
            GROUPS[e.group],
            e.version,
            list(&e.shortcodes),
            list(&e.tags),
            skins,
        )
        .unwrap();
    }
    writeln!(src, "];").unwrap();
    writeln!(src, "pub(crate) static POSTINGS: [u32; {}] = [", postings.len()).unwrap();
    for chunk in postings.chunks(16) {
        for p in chunk {
            write!(src, "{p},").unwrap();
        }
        src.push('\n');
    }
    writeln!(src, "];").unwrap();

    // Glyph lookup keys: fully-qualified, unqualified (no VS16) and skin variants.
    let mut glyphs: HashMap<String, (usize, u8)> = HashMap::new();
    for (id, e) in entries.iter().enumerate() {
        glyphs.entry(e.glyph.clone()).or_insert((id, 0));
        glyphs.entry(e.glyph.replace('\u{FE0F}', "")).or_insert((id, 0));
        if let Some(skins) = &e.skins {
            for (tone, s) in skins.iter().enumerate() {
                glyphs.entry(s.clone()).or_insert((id, tone as u8 + 1));
                glyphs.entry(s.replace('\u{FE0F}', "")).or_insert((id, tone as u8 + 1));
            }
        }
    }
    let glyphs: BTreeSet<_> = glyphs.into_iter().collect();
    let mut glyph_map = fst::MapBuilder::memory();
    for (glyph, (id, tone)) in &glyphs {
        glyph_map.insert(glyph, ((*id as u64) << 8) | *tone as u64).unwrap();
    }
    let glyphs_fst = glyph_map.into_inner().unwrap();

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::write(out.join("emoji_table.rs"), src).unwrap();
    std::fs::write(out.join("terms.fst"), terms_fst).unwrap();
    std::fs::write(out.join("shortcodes.fst"), shortcodes_fst).unwrap();
    std::fs::write(out.join("glyphs.fst"), glyphs_fst).unwrap();
}
