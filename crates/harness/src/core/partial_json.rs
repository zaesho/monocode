//! Tool input JSON that a provider streams in pieces.
//!
//! Claude and the Pi family send a tool call's input as JSON fragments and
//! the TypeScript ran `JSON.parse` on the whole buffer after every fragment,
//! so a long input such as a file write cost O(n^2) to parse. [`PartialJson`]
//! tracks bracket depth outside strings as fragments arrive, so callers run
//! the full parse only when the buffer can hold one whole object.

/// The JSON text received so far, with enough scan state to tell when it
/// can be a complete object.
///
/// [`PartialJson::complete`] returns the text whenever `serde_json` could
/// parse it as an object: such text has balanced brackets outside strings,
/// no open string, and `}` as its last non-whitespace byte. Text that fails
/// the check would also fail to parse, so a caller that parses only what
/// `complete` returns gets the same results as one that parses every time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartialJson {
    text: String,
    /// Open `{` and `[` minus closing `}` and `]`, outside strings.
    depth: i64,
    in_string: bool,
    /// The previous byte in a string was an unescaped backslash.
    escaped: bool,
}

impl PartialJson {
    /// Append one fragment.
    pub fn push_str(&mut self, fragment: &str) {
        for byte in fragment.bytes() {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.in_string = false;
                }
                continue;
            }
            match byte {
                b'"' => self.in_string = true,
                b'{' | b'[' => self.depth += 1,
                b'}' | b']' => self.depth -= 1,
                _ => {}
            }
        }
        self.text.push_str(fragment);
    }

    /// Everything received so far.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The text, when it can be one complete JSON object.
    pub fn complete(&self) -> Option<&str> {
        let closed = !self.in_string
            && self.depth == 0
            && self
                .text
                .trim_end_matches([' ', '\t', '\n', '\r'])
                .ends_with('}');
        closed.then_some(self.text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn parses_as_object(text: &str) -> bool {
        matches!(serde_json::from_str::<Value>(text), Ok(Value::Object(_)))
    }

    /// Feed `text` one byte-sized fragment at a time and check that every
    /// prefix serde parses as an object also passes `complete`.
    fn assert_never_skips_an_object(text: &str) {
        let mut partial = PartialJson::default();
        let mut start = 0;
        for (index, _) in text.char_indices().skip(1) {
            partial.push_str(&text[start..index]);
            start = index;
            let prefix = partial.as_str();
            if parses_as_object(prefix) {
                assert_eq!(partial.complete(), Some(prefix), "prefix {prefix:?}");
            }
        }
        partial.push_str(&text[start..]);
        if parses_as_object(text) {
            assert_eq!(partial.complete(), Some(text));
        }
    }

    #[test]
    fn waits_for_the_closing_brace() {
        let mut partial = PartialJson::default();
        partial.push_str("{\"command\":\"git status");
        assert_eq!(partial.complete(), None);
        partial.push_str("\"}");
        assert_eq!(partial.complete(), Some("{\"command\":\"git status\"}"));
    }

    #[test]
    fn ignores_brackets_and_escaped_quotes_inside_strings() {
        let mut partial = PartialJson::default();
        partial.push_str(r#"{"content":"fn main() { let a = \"}\"; }"#);
        assert_eq!(partial.complete(), None);
        partial.push_str(r#"","n":[1,{"x":"\\"}]}"#);
        assert!(partial.complete().is_some());
        assert!(parses_as_object(partial.as_str()));
    }

    #[test]
    fn keeps_trailing_json_whitespace() {
        let mut partial = PartialJson::default();
        partial.push_str(" {\"a\":1}\n\t ");
        assert_eq!(partial.complete(), Some(" {\"a\":1}\n\t "));
    }

    #[test]
    fn rejects_text_that_is_not_an_object() {
        for text in ["", "  ", "1", "\"a\"", "[1]", "{\"a\":1", "{\"a\":\"}"] {
            let mut partial = PartialJson::default();
            partial.push_str(text);
            assert_eq!(partial.complete(), None, "{text:?}");
        }
    }

    #[test]
    fn passes_every_prefix_serde_accepts() {
        for text in [
            r#"{"file_path":"/tmp/a.rs","content":"fn main() {\n    println!(\"{}\", [1, 2]);\n}\n"}"#,
            r#"{"todos":[{"content":"a","status":"pending"},{"content":"b \\","status":"done"}]}"#,
            r#"{"a":{"b":{"c":[]}},"d":"é ünïcode " }"}"#,
            "{}",
            " { } ",
            r#"{"a":1}{"b":2}"#,
        ] {
            assert_never_skips_an_object(text);
        }
    }
}
