//! Append-only span store. Every observable piece of material gets a stable,
//! content-addressed id. Nothing is ever overwritten or evicted.
//!
//! Provenance lives here and only here. A refusal names spans, and a span
//! stays re-readable after the trace ring has moved on.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

const ID_LEN: usize = 16;
const MAX_TEXT: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SpanId([u8; ID_LEN]);

impl SpanId {
    pub fn as_bytes(&self) -> &[u8; ID_LEN] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(ID_LEN * 2);
        for b in self.0 {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != ID_LEN * 2 {
            return None;
        }
        let bytes = s.as_bytes();
        let mut out = [0u8; ID_LEN];
        for i in 0..ID_LEN {
            let hi = (bytes[i * 2] as char).to_digit(16)?;
            let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
            out[i] = ((hi << 4) | lo) as u8;
        }
        Some(SpanId(out))
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "span:{}", self.to_hex())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SrcId(pub u32);

#[derive(Clone)]
pub struct Span {
    id: SpanId,
    src: SrcId,
    off: u32,
    hash: [u8; 32],
    text: Arc<str>,
}

impl Span {
    pub fn id(&self) -> SpanId {
        self.id
    }
    pub fn src(&self) -> SrcId {
        self.src
    }
    pub fn off(&self) -> u32 {
        self.off
    }
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SpanError {
    TooLong(usize),
}

impl fmt::Display for SpanError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SpanError::TooLong(n) => write!(f, "span too long: {} > {}", n, MAX_TEXT),
        }
    }
}

fn fnv1a64(bytes: &[u8], seed: u64) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64 ^ seed;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn content_hash(src: SrcId, off: u32, text: &[u8]) -> [u8; 32] {
    let mut buf = Vec::with_capacity(8 + text.len());
    buf.extend_from_slice(&src.0.to_le_bytes());
    buf.extend_from_slice(&off.to_le_bytes());
    buf.extend_from_slice(text);
    let mut out = [0u8; 32];
    for i in 0..4 {
        let h = fnv1a64(&buf, (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15));
        out[i * 8..i * 8 + 8].copy_from_slice(&h.to_le_bytes());
    }
    out
}
#[derive(Default)]
pub struct SpanStore {
    spans: Vec<Span>,
    by_hash: HashMap<[u8; 32], SpanId>,
}

impl SpanStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn append(&mut self, src: SrcId, off: u32, text: &str) -> Result<SpanId, SpanError> {
        if text.len() > MAX_TEXT {
            return Err(SpanError::TooLong(text.len()));
        }
        let hash = content_hash(src, off, text.as_bytes());
        if let Some(&id) = self.by_hash.get(&hash) {
            return Ok(id);
        }
        let mut id_bytes = [0u8; ID_LEN];
        id_bytes.copy_from_slice(&hash[..ID_LEN]);
        let id = SpanId(id_bytes);
        self.spans.push(Span {
            id,
            src,
            off,
            hash,
            text: Arc::from(text),
        });
        self.by_hash.insert(hash, id);
        Ok(id)
    }
}

impl SpanStore {
    pub fn get(&self, id: SpanId) -> Option<&Span> {
        self.spans.iter().find(|s| s.id == id)
    }

    pub fn text(&self, id: SpanId) -> Option<&str> {
        self.get(id).map(|s| s.text())
    }

    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Span> {
        self.spans.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_is_idempotent() {
        let mut st = SpanStore::new();
        let a = st.append(SrcId(1), 0, "hello").unwrap();
        let b = st.append(SrcId(1), 0, "hello").unwrap();
        assert_eq!(a, b);
        assert_eq!(st.len(), 1);
    }

    #[test]
    fn same_text_different_offset_differs() {
        let mut st = SpanStore::new();
        let a = st.append(SrcId(1), 0, "hello").unwrap();
        let b = st.append(SrcId(1), 7, "hello").unwrap();
        assert_ne!(a, b);
        assert_eq!(st.len(), 2);
    }

    #[test]
    fn hex_roundtrip() {
        let mut st = SpanStore::new();
        let id = st.append(SrcId(3), 42, "x").unwrap();
        assert_eq!(SpanId::from_hex(&id.to_hex()), Some(id));
        assert_eq!(SpanId::from_hex("zz"), None);
    }

    #[test]
    fn too_long_rejected() {
        let mut st = SpanStore::new();
        let big = "a".repeat(MAX_TEXT + 1);
        assert_eq!(
            st.append(SrcId(1), 0, &big),
            Err(SpanError::TooLong(MAX_TEXT + 1))
        );
        assert!(st.is_empty());
    }

    #[test]
    fn text_survives_lookup() {
        let mut st = SpanStore::new();
        let id = st.append(SrcId(1), 5, "row 214").unwrap();
        assert_eq!(st.text(id), Some("row 214"));
        assert_eq!(st.get(id).unwrap().off(), 5);
    }
}

