use serde::{Deserialize, Serialize};

/// Tracks the origin of a stored [`Note`](crate::task::Task).
///
/// Every note written to the knowledge graph carries one of these flags so that
/// callers can distinguish established facts from LLM-generated inferences.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceFlag {
    /// Stored directly by a user or external system via an MCP tool call.
    #[default]
    UserInput,
    /// Generated or derived by an LLM (consolidation, inference, reflection, synthesis).
    SynthesisInference,
    /// Asserted to originate from the model's pre-training corpus (use sparingly).
    CoreTraining,
}

impl ProvenanceFlag {
    /// Returns the canonical lowercase string stored on the Neo4j node.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProvenanceFlag::UserInput => "user_input",
            ProvenanceFlag::SynthesisInference => "synthesis_inference",
            ProvenanceFlag::CoreTraining => "core_training",
        }
    }
}

impl std::fmt::Display for ProvenanceFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ProvenanceFlag {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user_input" => Ok(ProvenanceFlag::UserInput),
            "synthesis_inference" => Ok(ProvenanceFlag::SynthesisInference),
            "core_training" => Ok(ProvenanceFlag::CoreTraining),
            _ => Err(()),
        }
    }
}
