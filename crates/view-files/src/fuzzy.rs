//! Port of src/shared/lib/fuzzy.ts: sequential fuzzy match. Every query
//! character must appear in order, with bonuses for consecutive runs, path
//! separators, and camelCase boundaries. The engine keeps the same port in
//! runtime/util/fuzzy.rs; the palette and the default ranking here need it
//! without depending on the engine.
//!
//! Positions and lengths count UTF-16 code units, as JavaScript strings do.

/// `FuzzyHit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyHit {
    pub score: i64,
    pub positions: Vec<usize>,
}

/// A query split into tokens and lowercased once, so ranking thousands of
/// paths per keystroke does not redo that work, and allocate for it, on
/// every path.
#[derive(Debug, Clone)]
pub struct PreparedQuery {
    /// The query was the empty string. A query of only whitespace is not
    /// empty, though it has no tokens.
    empty: bool,
    /// Each whitespace-separated token, lowercased, in UTF-16 code units.
    tokens: Vec<Vec<u16>>,
}

impl PreparedQuery {
    pub fn new(query: &str) -> Self {
        Self {
            empty: query.is_empty(),
            tokens: query
                .split_whitespace()
                .map(|token| units(&token.to_lowercase()))
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.empty
    }
}

/// `fuzzyMatch`: whitespace-separated tokens all have to match.
pub fn fuzzy_match(query: &str, text: &str) -> Option<FuzzyHit> {
    fuzzy_match_prepared(&PreparedQuery::new(query), text)
}

/// [`fuzzy_match`] with the query prepared ahead of time.
pub fn fuzzy_match_prepared(query: &PreparedQuery, text: &str) -> Option<FuzzyHit> {
    if query.tokens.is_empty() {
        return Some(FuzzyHit {
            score: 0,
            positions: Vec::new(),
        });
    }
    if query.tokens.len() == 1 {
        return match_token(&query.tokens[0], text);
    }
    let mut positions = Vec::new();
    let mut score = 0;
    for token in &query.tokens {
        let hit = match_token(token, text)?;
        score += hit.score;
        positions.extend(hit.positions);
    }
    positions.sort_unstable();
    Some(FuzzyHit { score, positions })
}

fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// One lowercased token against `text`. ASCII text, which is nearly every
/// path, is scanned in place: its lowercase form has the same length and its
/// bytes are its UTF-16 code units, so the result is the same as the general
/// path's.
fn match_token(needle: &[u16], text: &str) -> Option<FuzzyHit> {
    if needle.is_empty() {
        return Some(FuzzyHit {
            score: 0,
            positions: Vec::new(),
        });
    }
    if text.is_ascii() {
        let original = text.as_bytes();
        // Cheap rejection before any allocation: the token has to fit.
        if needle.len() > original.len() {
            return None;
        }
        return match_units(
            needle,
            original.len(),
            |i| u16::from(original[i].to_ascii_lowercase()),
            |i| original.get(i).map(|byte| u16::from(*byte)),
        );
    }
    let hay = units(&text.to_lowercase());
    // TODO(port): like the TypeScript, the bonuses read the original text at
    // indexes into the lowercased one, which drift if lowercasing changes the
    // length.
    let original = units(text);
    match_units(needle, hay.len(), |i| hay[i], |i| original.get(i).copied())
}

fn match_units(
    needle: &[u16],
    hay_len: usize,
    hay: impl Fn(usize) -> u16,
    original: impl Fn(usize) -> Option<u16>,
) -> Option<FuzzyHit> {
    let mut positions = Vec::new();
    let mut score: i64 = 0;
    let mut consecutive: i64 = 0;
    let mut qi = 0;
    let mut i = 0;
    while i < hay_len && qi < needle.len() {
        if hay(i) != needle[qi] {
            consecutive = 0;
            i += 1;
            continue;
        }
        positions.push(i);
        consecutive += 1;
        score += 1 + consecutive * 4;
        if i == 0 || is_break(original(i - 1)) {
            score += 14;
        } else if is_upper(original(i)) && !is_upper(original(i - 1)) {
            score += 10;
        }
        qi += 1;
        i += 1;
    }
    if qi != needle.len() {
        return None;
    }
    score -= hay_len as i64 - needle.len() as i64;
    Some(FuzzyHit { score, positions })
}

fn is_break(unit: Option<u16>) -> bool {
    matches!(
        unit.and_then(|unit| char::from_u32(unit as u32)),
        Some('/' | '\\' | '-' | '_' | '.' | ' ')
    )
}

fn is_upper(unit: Option<u16>) -> bool {
    unit.is_some_and(|unit| (b'A' as u16..=b'Z' as u16).contains(&unit))
}

/// `scorePath`: prefer filename hits over directory-only hits. Positions
/// index `relative`.
pub fn score_path(query: &str, relative: &str, name: &str) -> Option<FuzzyHit> {
    score_path_prepared(
        &PreparedQuery::new(monocode_core::js::trim(query)),
        relative,
        name,
    )
}

/// [`score_path`] with the query prepared ahead of time. Prepare the
/// trimmed query, as `score_path` does.
pub fn score_path_prepared(query: &PreparedQuery, relative: &str, name: &str) -> Option<FuzzyHit> {
    if query.is_empty() {
        return Some(FuzzyHit {
            score: 0,
            positions: Vec::new(),
        });
    }
    if let Some(hit) = fuzzy_match_prepared(query, name) {
        let offset = js_len(relative).saturating_sub(js_len(name));
        return Some(FuzzyHit {
            score: hit.score + 400,
            positions: hit
                .positions
                .into_iter()
                .map(|index| index + offset)
                .collect(),
        });
    }
    fuzzy_match_prepared(query, relative)
}

/// `String.prototype.length`, without a scan for ASCII text.
fn js_len(text: &str) -> usize {
    if text.is_ascii() {
        text.len()
    } else {
        monocode_core::js::len(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_in_order_with_boundary_bonuses() {
        let hit = fuzzy_match("ab", "a/b").unwrap();
        assert_eq!(hit.positions, vec![0, 2]);
        // a: 1 + 4 + 14 at the start; b: 1 + 4 + 14 after "/"; minus 1 skipped.
        assert_eq!(hit.score, 37);
        assert!(fuzzy_match("ba", "ab").is_none());
        assert_eq!(
            fuzzy_match("  ", "x").unwrap().positions,
            Vec::<usize>::new()
        );
    }

    #[test]
    fn camel_case_and_tokens() {
        let hit = fuzzy_match("fb", "fooBar").unwrap();
        assert_eq!(hit.positions, vec![0, 3]);
        assert_eq!(hit.score, 19 + 15 - 4);
        let multi = fuzzy_match("bar foo", "fooBar").unwrap();
        assert_eq!(multi.positions, vec![0, 1, 2, 3, 4, 5]);
        assert!(fuzzy_match("foo zed", "fooBar").is_none());
    }

    /// The matcher before the ASCII fast path, as ported.
    fn reference_match_token(query: &str, text: &str) -> Option<FuzzyHit> {
        if query.is_empty() {
            return Some(FuzzyHit {
                score: 0,
                positions: Vec::new(),
            });
        }
        let needle = units(&query.to_lowercase());
        let hay = units(&text.to_lowercase());
        let original = units(text);
        let mut positions = Vec::new();
        let mut score: i64 = 0;
        let mut consecutive: i64 = 0;
        let mut qi = 0;
        let mut i = 0;
        while i < hay.len() && qi < needle.len() {
            if hay[i] != needle[qi] {
                consecutive = 0;
                i += 1;
                continue;
            }
            positions.push(i);
            consecutive += 1;
            score += 1 + consecutive * 4;
            if i == 0 || is_break(original.get(i - 1).copied()) {
                score += 14;
            } else if is_upper(original.get(i).copied()) && !is_upper(original.get(i - 1).copied())
            {
                score += 10;
            }
            qi += 1;
            i += 1;
        }
        if qi != needle.len() {
            return None;
        }
        score -= hay.len() as i64 - needle.len() as i64;
        Some(FuzzyHit { score, positions })
    }

    fn reference_fuzzy_match(query: &str, text: &str) -> Option<FuzzyHit> {
        let tokens: Vec<&str> = query.split_whitespace().collect();
        if tokens.is_empty() {
            return Some(FuzzyHit {
                score: 0,
                positions: Vec::new(),
            });
        }
        if tokens.len() == 1 {
            return reference_match_token(tokens[0], text);
        }
        let mut positions = Vec::new();
        let mut score = 0;
        for token in tokens {
            let hit = reference_match_token(token, text)?;
            score += hit.score;
            positions.extend(hit.positions);
        }
        positions.sort_unstable();
        Some(FuzzyHit { score, positions })
    }

    #[test]
    fn fast_ascii_path_matches_the_ported_matcher() {
        let texts = [
            "",
            "a",
            "src/app/App.tsx",
            "crates/view-files/src/file_picker.rs",
            "Some_Mixed-Case.File Name.TXT",
            "docs/Ünïcode/Äpp.md",
            "ΣΑΣ/final.rs",
            "emoji/😀app.ts",
            "İstanbul.txt",
        ];
        let queries = [
            "a", "A", "app", "APP", "src app", "fp", "fpr", "ü", "Ä", "σ", "ς", "i", "İ", "😀",
            "x y", "txt", "z",
        ];
        for text in texts {
            for query in queries {
                assert_eq!(
                    fuzzy_match(query, text),
                    reference_fuzzy_match(query, text),
                    "query {query:?} text {text:?}"
                );
            }
        }
    }

    #[test]
    fn path_scores_prefer_the_file_name() {
        let hit = score_path("app", "src/app/App.tsx", "App.tsx").unwrap();
        assert_eq!(hit.positions, vec![8, 9, 10]);
        assert!(hit.score > 400);
        let dir = score_path("src", "src/app/App.tsx", "App.tsx").unwrap();
        assert_eq!(dir.positions, vec![0, 1, 2]);
        assert!(dir.score < 400);
    }
}
