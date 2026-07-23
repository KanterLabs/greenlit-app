//! Diagnostics for invalid `needs` graphs.

use greenlit_workflow::Span;

use super::JobId;

/// Everything that can go wrong building a workflow's `needs` graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    /// A job's `needs:` entry names a job id that is not defined.
    UnknownNeed {
        /// Location of the invalid `needs:` item.
        span: Span,
        /// The job whose `needs:` list has the bad reference.
        job: JobId,
        /// The unknown job id it referenced.
        needs: JobId,
    },
    /// Every disjoint dependency cycle found in the graph.
    Cycles(Vec<DependencyCycle>),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::UnknownNeed { span, job, needs } => {
                write!(formatter, "{span}: job '{job}' needs unknown job '{needs}'")
            }
            GraphError::Cycles(cycles) => {
                for (index, cycle) in cycles.iter().enumerate() {
                    if index > 0 {
                        write!(formatter, "; ")?;
                    }
                    write!(formatter, "{cycle}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for GraphError {}

/// One dependency cycle: a closed walk in the `needs` direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyCycle {
    /// Location of the canonical first member's declaration.
    pub span: Span,
    /// Members in canonical walk order starting at the lexicographically
    /// smallest [`JobId`]; a self-dependency has one member.
    pub members: Vec<JobId>,
}

impl std::fmt::Display for DependencyCycle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: dependency cycle: ", self.span)?;
        for (index, member) in self.members.iter().enumerate() {
            if index > 0 {
                write!(formatter, " -> ")?;
            }
            write!(formatter, "{member}")?;
        }
        if let Some(first) = self.members.first() {
            write!(formatter, " -> {first}")?;
        }
        Ok(())
    }
}
