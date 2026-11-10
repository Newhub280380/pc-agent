//! A failing set reduced to 1-minimality.
//!
//! Fail without a set is a shrug, not a failure.

use serde::{Deserialize, Serialize};

use crate::spans::SpanId;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Lit {
    Number(String),
    Quoted(String),
    Date(String),
    Ident(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinimalSet {
    spans: Vec<SpanId>,
    literals: Vec<Lit>,
}

impl MinimalSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty() && self.literals.is_empty()
    }

    pub fn len(&self) -> usize {
        self.spans.len() + self.literals.len()
    }

    pub fn push_span(&mut self, id: SpanId) {
        if !self.spans.contains(&id) {
            self.spans.push(id);
        }
    }

    pub fn push_lit(&mut self, lit: Lit) {
        if !self.literals.contains(&lit) {
            self.literals.push(lit);
        }
    }

    pub fn spans(&self) -> &[SpanId] {
        &self.spans
    }

    pub fn literals(&self) -> &[Lit] {
        &self.literals
    }

    pub fn extend(&mut self, other: &MinimalSet) {
        for &s in &other.spans {
            self.push_span(s);
        }
        for l in &other.literals {
            self.push_lit(l.clone());
        }
    }
}

pub fn shrink<F>(set: &MinimalSet, still_fails: F) -> MinimalSet
where
    F: Fn(&MinimalSet) -> bool,
{
    let mut cur = set.clone();
    loop {
        let mut changed = false;
        let mut i = 0;
        while i < cur.spans.len() {
            let mut trial = cur.clone();
            trial.spans.remove(i);
            if still_fails(&trial) {
                cur = trial;
                changed = true;
            } else {
                i += 1;
            }
        }
        let mut j = 0;
        while j < cur.literals.len() {
            let mut trial = cur.clone();
            trial.literals.remove(j);
            if still_fails(&trial) {
                cur = trial;
                changed = true;
            } else {
                j += 1;
            }
        }
        if !changed {
            return cur;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail(MinimalSet),
    Inconclusive,
}

impl Verdict {
    pub fn fail(set: MinimalSet) -> Self {
        if set.is_empty() {
            Verdict::Inconclusive
        } else {
            Verdict::Fail(set)
        }
    }

    pub fn is_pass(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid(n: u8) -> SpanId {
        SpanId::from_hex(&format!("{n:02x}").repeat(16)).unwrap()
    }

    fn set(ids: &[u8]) -> MinimalSet {
        let mut s = MinimalSet::new();
        for &i in ids {
            s.push_span(sid(i));
        }
        s
    }

    #[test]
    fn dedupes() {
        let mut s = MinimalSet::new();
        s.push_span(sid(1));
        s.push_span(sid(1));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn shrink_drops_irrelevant() {
        let full = set(&[1, 2, 3, 4]);
        let reduced = shrink(&full, |s| s.spans().contains(&sid(3)));
        assert_eq!(reduced.spans(), &[sid(3)]);
    }

    #[test]
    fn shrink_keeps_a_needed_one() {
        let full = set(&[1, 2, 3, 4]);
        let reduced = shrink(&full, |s| {
            s.spans().contains(&sid(2)) || s.spans().contains(&sid(4))
        });
        assert_eq!(reduced.len(), 1);
    }

    #[test]
    fn empty_fail_is_inconclusive() {
        assert_eq!(Verdict::fail(MinimalSet::new()), Verdict::Inconclusive);
        assert_eq!(Verdict::fail(set(&[1])), Verdict::Fail(set(&[1])));
    }

    #[test]
    fn shrink_terminates_on_always_failing() {
        let full = set(&[1, 2, 3]);
        let reduced = shrink(&full, |_| true);
        assert!(reduced.is_empty());
    }
}
