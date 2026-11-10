//! Terminal run states. A run may end without an answer and still be correct.

use serde::{Deserialize, Serialize};

use crate::budget::Exhausted;
use crate::minimal::MinimalSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Gap {
    Unresolved { node: String, missing: MinimalSet },
    Unverified { node: String, reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DegradeCause {
    Budget(Exhausted),
    NoVerifier,
    LowCoverage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunState {
    Closed,
    Refused { reason: String },
    Degraded { cause: DegradeCause, gaps: Vec<Gap> },
}

impl RunState {
    pub fn is_success(&self) -> bool {
        matches!(self, RunState::Closed)
    }

    /// Degraded is legitimate only when a cause is named and gaps are listed.
    pub fn degraded(cause: DegradeCause, gaps: Vec<Gap>) -> Self {
        RunState::Degraded { cause, gaps }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_closed_is_success() {
        assert!(RunState::Closed.is_success());
        assert!(!RunState::degraded(DegradeCause::NoVerifier, vec![]).is_success());
        assert!(!RunState::Refused { reason: "unknown route".into() }.is_success());
    }

    #[test]
    fn degraded_carries_cause() {
        let s = RunState::degraded(DegradeCause::Budget(Exhausted::Wall), vec![]);
        assert_eq!(
            s,
            RunState::Degraded {
                cause: DegradeCause::Budget(Exhausted::Wall),
                gaps: vec![]
            }
        );
    }
}
