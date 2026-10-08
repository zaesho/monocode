//! Port of src/features/sessions/data/sessionCache.ts: closed chats stay
//! here so clicking a session card can paint without reading disk.

use monocode_core::Session;
use serde::Serialize;
use serde_json::Value;

/// `SESSION_LOAD_CACHE_LIMIT`.
pub const SESSION_LOAD_CACHE_LIMIT: usize = 12;
/// `SESSION_LOAD_CACHE_MAX_BYTES`.
pub const SESSION_LOAD_CACHE_MAX_BYTES: usize = 32 * 1024 * 1024;

/// `estimateSessionCacheBytes`: a conservative retained-size estimate with
/// the TypeScript's weights: 2 bytes per UTF-16 unit, 8 per number, 4 per
/// boolean, 24 plus 8 per element for arrays, 32 plus 8 per field for
/// objects.
///
/// The TypeScript walked the live object graph. This counts the session's
/// JSON value, which skips unset optional fields the object graph counted
/// as `undefined` slots. The cache estimates each session once, on insert.
///
/// The count runs as a serializer, so it gives what `serde_json::to_value`
/// followed by a walk would give without building the value. Building it
/// cost a full copy of the transcript on the UI thread each time a hidden
/// session detached or a history card prefetched.
pub fn estimate_session_cache_bytes(session: &Session) -> usize {
    let mut bytes = 0;
    match session.serialize(estimate::Estimator { bytes: &mut bytes }) {
        Ok(()) => bytes,
        Err(_) => 0,
    }
}

/// The estimate of a JSON value, with the weights above.
#[cfg_attr(not(test), allow(dead_code))]
fn estimate_value(value: &Value) -> usize {
    let mut bytes = 0;
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::String(text) => bytes += text.encode_utf16().count() * 2,
            Value::Number(_) => bytes += 8,
            Value::Bool(_) => bytes += 4,
            Value::Null => {}
            Value::Array(items) => {
                bytes += 24 + items.len() * 8;
                pending.extend(items.iter());
            }
            Value::Object(fields) => {
                bytes += 32 + fields.len() * 8;
                pending.extend(fields.values());
            }
        }
    }
    bytes
}

/// The closed-session cache, oldest first.
#[derive(Debug, Default, Clone)]
pub struct SessionCache {
    entries: Vec<(Session, usize)>,
}

impl SessionCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a cached session out. The cache owns closed sessions only, so a
    /// session that opens moves out instead of staying as a stale copy.
    pub fn take(&mut self, session_id: &str) -> Option<Session> {
        let index = self
            .entries
            .iter()
            .position(|(session, _)| session.id == session_id)?;
        Some(self.entries.remove(index).0)
    }

    pub fn remove(&mut self, session_id: &str) {
        self.entries.retain(|(session, _)| session.id != session_id);
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.entries
            .iter()
            .any(|(session, _)| session.id == session_id)
    }

    pub fn get(&self, session_id: &str) -> Option<&Session> {
        self.entries
            .iter()
            .map(|(session, _)| session)
            .find(|session| session.id == session_id)
    }

    /// Cached ids, oldest first.
    pub fn ids(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|(session, _)| session.id.as_str())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// `rememberLoadedSession` with the default limits.
    pub fn remember(&mut self, session: Session) {
        remember_loaded_session(
            self,
            session,
            SESSION_LOAD_CACHE_LIMIT,
            SESSION_LOAD_CACHE_MAX_BYTES,
        );
    }
}

/// `rememberLoadedSession`: insert as newest, then evict the oldest until
/// both the count and the byte budget fit. A session larger than the whole
/// budget is not kept.
pub fn remember_loaded_session(
    cache: &mut SessionCache,
    session: Session,
    limit: usize,
    max_bytes: usize,
) {
    cache.remove(&session.id);
    let session_bytes = estimate_session_cache_bytes(&session);
    if limit == 0 || session_bytes > max_bytes {
        return;
    }
    cache.entries.push((session, session_bytes));
    let mut cache_bytes: usize = cache.entries.iter().map(|(_, bytes)| bytes).sum();
    while cache.entries.len() > limit || cache_bytes > max_bytes {
        if cache.entries.is_empty() {
            break;
        }
        let (_, evicted) = cache.entries.remove(0);
        cache_bytes -= evicted;
    }
}

/// A `serde::Serializer` that adds up `estimate_value`'s weights for the
/// value `serde_json::to_value` would build.
mod estimate {
    use std::fmt;

    use serde::ser::{self, Serialize};

    /// Weights from `estimate_value`.
    const NUMBER: usize = 8;
    const BOOL: usize = 4;
    const ARRAY: usize = 24;
    const OBJECT: usize = 32;
    const SLOT: usize = 8;

    #[derive(Debug)]
    pub struct Error;

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("could not estimate the session size")
        }
    }

    impl std::error::Error for Error {}

    impl ser::Error for Error {
        fn custom<T: fmt::Display>(_msg: T) -> Self {
            Error
        }
    }

    /// UTF-16 units times two. Transcripts are mostly ASCII, where the
    /// count is the byte length.
    fn text(value: &str) -> usize {
        if value.is_ascii() {
            value.len() * 2
        } else {
            value.encode_utf16().count() * 2
        }
    }

    pub struct Estimator<'a> {
        pub bytes: &'a mut usize,
    }

    /// An array or object being counted. Each element or field adds a slot
    /// and its value.
    pub struct Compound<'a> {
        bytes: &'a mut usize,
    }

    impl Compound<'_> {
        fn add<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
            *self.bytes += SLOT;
            value.serialize(Estimator {
                bytes: &mut *self.bytes,
            })
        }
    }

    impl<'a> ser::Serializer for Estimator<'a> {
        type Ok = ();
        type Error = Error;
        type SerializeSeq = Compound<'a>;
        type SerializeTuple = Compound<'a>;
        type SerializeTupleStruct = Compound<'a>;
        type SerializeTupleVariant = Compound<'a>;
        type SerializeMap = Compound<'a>;
        type SerializeStruct = Compound<'a>;
        type SerializeStructVariant = Compound<'a>;

        fn serialize_bool(self, _v: bool) -> Result<(), Error> {
            *self.bytes += BOOL;
            Ok(())
        }
        fn serialize_i8(self, _v: i8) -> Result<(), Error> {
            *self.bytes += NUMBER;
            Ok(())
        }
        fn serialize_i16(self, _v: i16) -> Result<(), Error> {
            *self.bytes += NUMBER;
            Ok(())
        }
        fn serialize_i32(self, _v: i32) -> Result<(), Error> {
            *self.bytes += NUMBER;
            Ok(())
        }
        fn serialize_i64(self, _v: i64) -> Result<(), Error> {
            *self.bytes += NUMBER;
            Ok(())
        }
        fn serialize_i128(self, _v: i128) -> Result<(), Error> {
            *self.bytes += NUMBER;
            Ok(())
        }
        fn serialize_u8(self, _v: u8) -> Result<(), Error> {
            *self.bytes += NUMBER;
            Ok(())
        }
        fn serialize_u16(self, _v: u16) -> Result<(), Error> {
            *self.bytes += NUMBER;
            Ok(())
        }
        fn serialize_u32(self, _v: u32) -> Result<(), Error> {
            *self.bytes += NUMBER;
            Ok(())
        }
        fn serialize_u64(self, _v: u64) -> Result<(), Error> {
            *self.bytes += NUMBER;
            Ok(())
        }
        fn serialize_u128(self, _v: u128) -> Result<(), Error> {
            *self.bytes += NUMBER;
            Ok(())
        }
        fn serialize_f32(self, v: f32) -> Result<(), Error> {
            self.serialize_f64(f64::from(v))
        }
        fn serialize_f64(self, v: f64) -> Result<(), Error> {
            // `to_value` stores a non-finite float as null.
            if v.is_finite() {
                *self.bytes += NUMBER;
            }
            Ok(())
        }
        fn serialize_char(self, v: char) -> Result<(), Error> {
            *self.bytes += v.len_utf16() * 2;
            Ok(())
        }
        fn serialize_str(self, v: &str) -> Result<(), Error> {
            *self.bytes += text(v);
            Ok(())
        }
        fn serialize_bytes(self, v: &[u8]) -> Result<(), Error> {
            // An array of numbers.
            *self.bytes += ARRAY + v.len() * (SLOT + NUMBER);
            Ok(())
        }
        fn serialize_none(self) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<(), Error> {
            value.serialize(self)
        }
        fn serialize_unit(self) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_unit_variant(
            self,
            _name: &'static str,
            _index: u32,
            variant: &'static str,
        ) -> Result<(), Error> {
            *self.bytes += text(variant);
            Ok(())
        }
        fn serialize_newtype_struct<T: ?Sized + Serialize>(
            self,
            _name: &'static str,
            value: &T,
        ) -> Result<(), Error> {
            value.serialize(self)
        }
        fn serialize_newtype_variant<T: ?Sized + Serialize>(
            self,
            _name: &'static str,
            _index: u32,
            _variant: &'static str,
            value: &T,
        ) -> Result<(), Error> {
            // `{ variant: value }`.
            *self.bytes += OBJECT + SLOT;
            value.serialize(self)
        }
        fn serialize_seq(self, _len: Option<usize>) -> Result<Compound<'a>, Error> {
            *self.bytes += ARRAY;
            Ok(Compound { bytes: self.bytes })
        }
        fn serialize_tuple(self, len: usize) -> Result<Compound<'a>, Error> {
            self.serialize_seq(Some(len))
        }
        fn serialize_tuple_struct(
            self,
            _name: &'static str,
            len: usize,
        ) -> Result<Compound<'a>, Error> {
            self.serialize_seq(Some(len))
        }
        fn serialize_tuple_variant(
            self,
            _name: &'static str,
            _index: u32,
            _variant: &'static str,
            _len: usize,
        ) -> Result<Compound<'a>, Error> {
            // `{ variant: [...] }`.
            *self.bytes += OBJECT + SLOT + ARRAY;
            Ok(Compound { bytes: self.bytes })
        }
        fn serialize_map(self, _len: Option<usize>) -> Result<Compound<'a>, Error> {
            *self.bytes += OBJECT;
            Ok(Compound { bytes: self.bytes })
        }
        fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Compound<'a>, Error> {
            *self.bytes += OBJECT;
            Ok(Compound { bytes: self.bytes })
        }
        fn serialize_struct_variant(
            self,
            _name: &'static str,
            _index: u32,
            _variant: &'static str,
            _len: usize,
        ) -> Result<Compound<'a>, Error> {
            // `{ variant: { ... } }`.
            *self.bytes += OBJECT + SLOT + OBJECT;
            Ok(Compound { bytes: self.bytes })
        }
    }

    impl ser::SerializeSeq for Compound<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
            self.add(value)
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ser::SerializeTuple for Compound<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
            self.add(value)
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ser::SerializeTupleStruct for Compound<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
            self.add(value)
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ser::SerializeTupleVariant for Compound<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
            self.add(value)
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ser::SerializeMap for Compound<'_> {
        type Ok = ();
        type Error = Error;
        // Keys become object keys, which the estimate does not count.
        fn serialize_key<T: ?Sized + Serialize>(&mut self, _key: &T) -> Result<(), Error> {
            Ok(())
        }
        fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Error> {
            self.add(value)
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ser::SerializeStruct for Compound<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_field<T: ?Sized + Serialize>(
            &mut self,
            _key: &'static str,
            value: &T,
        ) -> Result<(), Error> {
            self.add(value)
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }

    impl ser::SerializeStructVariant for Compound<'_> {
        type Ok = ();
        type Error = Error;
        fn serialize_field<T: ?Sized + Serialize>(
            &mut self,
            _key: &'static str,
            value: &T,
        ) -> Result<(), Error> {
            self.add(value)
        }
        fn end(self) -> Result<(), Error> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_core::HarnessId;
    use monocode_core::block::{Block, BlockRole};

    /// The estimate the cache used before: build the JSON value, then walk it.
    fn estimate_through_value(session: &Session) -> usize {
        estimate_value(&serde_json::to_value(session).unwrap())
    }

    #[test]
    fn counts_what_a_walk_of_the_json_value_counts() {
        let mut session = chat("s1");
        assert_eq!(
            estimate_session_cache_bytes(&session),
            estimate_through_value(&session)
        );
        let block: Block = serde_json::from_value(serde_json::json!({
            "id": "a1",
            "role": "assistant",
            "text": "héllo 👋 wörld",
            "unknownField": { "nested": [1, 2.5, null, true, "x"] },
            "approval": { "requestId": 7, "title": "Run ls?" },
            "tool": { "name": "Bash", "kind": "execute", "status": "completed" },
        }))
        .unwrap();
        session.blocks = vec![
            Block::new("u1", BlockRole::User, "question"),
            block,
            Block::new("r1", BlockRole::Reasoning, "thinking ".repeat(50)),
        ];
        session.title = "Ünïcode 標題".into();
        session
            .model_settings
            .insert("effort".into(), "high".into());
        session.busy = Some(true);
        session.provider_session_id = Some("thread-1".into());
        assert_eq!(
            estimate_session_cache_bytes(&session),
            estimate_through_value(&session)
        );
    }

    fn chat(id: &str) -> Session {
        Session::blank(id, HarnessId::Cursor, "cursor:auto", "/tmp/project")
    }

    #[test]
    fn keeps_the_newest_session_and_drops_the_oldest_past_the_limit() {
        let mut cache = SessionCache::new();
        remember_loaded_session(&mut cache, chat("a"), 2, SESSION_LOAD_CACHE_MAX_BYTES);
        remember_loaded_session(&mut cache, chat("b"), 2, SESSION_LOAD_CACHE_MAX_BYTES);
        remember_loaded_session(&mut cache, chat("c"), 2, SESSION_LOAD_CACHE_MAX_BYTES);
        assert_eq!(cache.ids(), vec!["b", "c"]);
    }

    #[test]
    fn treats_a_repeat_as_newest_so_it_is_not_evicted() {
        let mut cache = SessionCache::new();
        for id in ["a", "b", "a", "c"] {
            remember_loaded_session(&mut cache, chat(id), 2, SESSION_LOAD_CACHE_MAX_BYTES);
        }
        assert_eq!(cache.ids(), vec!["a", "c"]);
    }

    #[test]
    fn evicts_by_estimated_memory_as_well_as_entry_count() {
        let mut cache = SessionCache::new();
        let mut a = chat("a");
        let mut b = chat("b");
        a.blocks = vec![Block::new("a1", BlockRole::Assistant, "a".repeat(200))];
        b.blocks = vec![Block::new("b1", BlockRole::Assistant, "b".repeat(200))];
        let budget = estimate_session_cache_bytes(&a).max(estimate_session_cache_bytes(&b));
        remember_loaded_session(&mut cache, a, 12, budget);
        remember_loaded_session(&mut cache, b, 12, budget);
        assert_eq!(cache.ids(), vec!["b"]);
    }

    #[test]
    fn does_not_retain_one_session_larger_than_the_whole_budget() {
        let mut cache = SessionCache::new();
        let mut oversized = chat("large");
        oversized.blocks = vec![Block::new(
            "large-1",
            BlockRole::Assistant,
            "x".repeat(1_000),
        )];
        remember_loaded_session(&mut cache, oversized, 12, 100);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn take_moves_a_session_out() {
        let mut cache = SessionCache::new();
        cache.remember(chat("a"));
        assert!(cache.contains("a"));
        assert_eq!(cache.take("a").map(|s| s.id), Some("a".into()));
        assert!(cache.is_empty());
    }
}
