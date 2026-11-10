//! Hard ceilings per run. Exhaustion is a normal transition, not a panic.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_nodes: usize,
    pub max_iters_per_node: usize,
    pub max_tokens: u64,
    pub wall: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_nodes: 400,
            max_iters_per_node: 4,
            max_tokens: 2_000_000,
            wall: Duration::from_secs(900),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exhausted {
    Nodes,
    Iters(String),
    Tokens,
    Wall,
}

pub struct Budget {
    limits: Limits,
    nodes: AtomicUsize,
    tokens: AtomicU64,
    iters: Mutex<HashMap<String, usize>>,
    deadline: Instant,
}

impl Budget {
    pub fn new(limits: Limits) -> Self {
        let deadline = Instant::now() + limits.wall;
        Self {
            limits,
            nodes: AtomicUsize::new(0),
            tokens: AtomicU64::new(0),
            iters: Mutex::new(HashMap::new()),
            deadline,
        }
    }

    pub fn check(&self) -> Result<(), Exhausted> {
        if Instant::now() >= self.deadline {
            return Err(Exhausted::Wall);
        }
        if self.nodes.load(Ordering::Relaxed) >= self.limits.max_nodes {
            return Err(Exhausted::Nodes);
        }
        if self.tokens.load(Ordering::Relaxed) >= self.limits.max_tokens {
            return Err(Exhausted::Tokens);
        }
        Ok(())
    }

    pub fn take_node(&self) -> Result<(), Exhausted> {
        self.check()?;
        self.nodes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn take_iter(&self, node: &str) -> Result<(), Exhausted> {
        self.check()?;
        let mut map = self.iters.lock().unwrap();
        let used = map.entry(node.to_string()).or_insert(0);
        if *used >= self.limits.max_iters_per_node {
            return Err(Exhausted::Iters(node.to_string()));
        }
        *used += 1;
        Ok(())
    }

    pub fn add_tokens(&self, n: u64) {
        self.tokens.fetch_add(n, Ordering::Relaxed);
    }

    pub fn iters_of(&self, node: &str) -> usize {
        *self.iters.lock().unwrap().get(node).unwrap_or(&0)
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tight() -> Budget {
        Budget::new(Limits {
            max_nodes: 2,
            max_iters_per_node: 2,
            max_tokens: 10,
            wall: Duration::from_secs(60),
        })
    }

    #[test]
    fn node_ceiling_holds() {
        let b = tight();
        assert!(b.take_node().is_ok());
        assert!(b.take_node().is_ok());
        assert_eq!(b.take_node(), Err(Exhausted::Nodes));
    }

    #[test]
    fn iter_ceiling_is_per_node() {
        let b = tight();
        assert!(b.take_iter("a").is_ok());
        assert!(b.take_iter("a").is_ok());
        assert_eq!(b.take_iter("a"), Err(Exhausted::Iters("a".into())));
        assert!(b.take_iter("b").is_ok());
        assert_eq!(b.iters_of("a"), 2);
    }

    #[test]
    fn token_ceiling_holds() {
        let b = tight();
        b.add_tokens(11);
        assert_eq!(b.check(), Err(Exhausted::Tokens));
    }

    #[test]
    fn wall_expires() {
        let b = Budget::new(Limits {
            wall: Duration::from_millis(1),
            ..Limits::default()
        });
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(b.check(), Err(Exhausted::Wall));
        assert_eq!(b.remaining(), Duration::ZERO);
    }
}
