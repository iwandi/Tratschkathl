//! Plugin capability traits (spec §5). A plugin implements one or more capabilities and is
//! compiled into the layer that hosts it: `Agent` into the Agent Handler, everything else
//! into Services. M1 uses `FactDeriver`; the other capabilities are declared so plugin
//! crates can state what they will provide.

use std::path::PathBuf;

pub use tk_bus::model::FactInput;

/// The capability layers from the spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    Agent,
    Vcs,
    Forge,
    Build,
    WorkItems,
    DocumentProvider,
    FactDeriver,
    EventSource,
    RefResolver,
}

/// What a fact deriver gets to look at.
#[derive(Clone, Debug)]
pub struct SessionContext {
    pub session: String,
    pub cwd: PathBuf,
}

/// Derives facts from a session's context (cwd, branch name, …). Called repeatedly; it
/// returns the full set of facts it currently stands behind. Facts it stops returning are
/// cleared, and it never overrides a fact an agent or the user set.
pub trait FactDeriver: Send + Sync {
    fn derive(&self, ctx: &SessionContext) -> Vec<FactInput>;
}

pub trait Plugin: Send + Sync {
    /// Short stable id, used as fact source `plugin:<id>`.
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> &'static [Capability];
    fn fact_deriver(&self) -> Option<&dyn FactDeriver> {
        None
    }
}

/// Convenience constructor for a text fact.
pub fn text(key: &str, value: impl Into<String>) -> FactInput {
    FactInput { key: key.into(), value: value.into(), kind: None }
}

/// Convenience constructor for a `Ref(kind, id)` fact.
pub fn reference(key: &str, kind: &str, id: impl Into<String>) -> FactInput {
    FactInput { key: key.into(), value: id.into(), kind: Some(kind.into()) }
}
