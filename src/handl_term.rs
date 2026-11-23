//! `IrModule` for the arrow-calculus `Term` — behind feature
//! `handl-term`.
//!
//! Orphan-rule placement: `IrModule` is local to this crate while
//! `handl_calculus::Term` is foreign, so the impl can only live here
//! (or inside `handl-calculus`, which must stay free of engine deps).
//! Engine→kernel-crate deps are legal, so compile backends opt in via
//! `hnl-engine-codegen = { features = ["handl-term"] }` and write
//! `impl Emitter<Term> for MyBackend` in their own crates.

use crate::traits::{IrKind, IrModule};
use handl_calculus::Term;

/// `IrKind` tag for the arrow-calculus `Term` IR family.
pub const HANDL_TERM_KIND: &str = "handl-term";

impl IrModule for Term {
    fn kind(&self) -> IrKind {
        IrKind::new(HANDL_TERM_KIND)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn term_kind_is_handl_term() {
        let term = Term::constant(json!(42));
        assert_eq!(term.kind(), IrKind::new("handl-term"));
        assert_eq!(term.kind().as_str(), HANDL_TERM_KIND);
    }
}
