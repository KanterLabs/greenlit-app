//! Incremental bound on the retained/serializable execution plan.

use std::io::{self, Write};

use serde::Serialize;

use greenlit_workflow::Span;

use super::PlanError;

/// A local plan is retained as one in-process object, unlike GitHub's
/// separately dispatched matrix jobs. Bound its stable JSON representation
/// so a small anchored workflow cannot amplify into hundreds of MiB while
/// expanding 256 legs.
pub(super) const MAX_EXECUTION_PLAN_JSON_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct PlanSizeBudget {
    used: usize,
}

impl PlanSizeBudget {
    pub(crate) fn new() -> Self {
        Self { used: 0 }
    }

    pub(crate) fn add<T: Serialize + ?Sized>(
        &mut self,
        value: &T,
        span: &Span,
    ) -> Result<(), PlanError> {
        let remaining = MAX_EXECUTION_PLAN_JSON_BYTES.saturating_sub(self.used);
        let mut counter = BoundedCounter::new(remaining);
        if serde_json::to_writer(&mut counter, value).is_err() {
            return Err(PlanError::SizeLimit {
                span: span.clone(),
                max_bytes: MAX_EXECUTION_PLAN_JSON_BYTES,
            });
        }
        self.used = self.used.saturating_add(counter.written);
        Ok(())
    }

    /// Charges fixed JSON/container overhead before retaining the enclosing
    /// object. Repository-controlled payloads are charged through [`Self::add`];
    /// this accounts conservatively for field names, delimiters, and vector
    /// slots without ever materializing the complete aggregate.
    pub(crate) fn add_bytes(&mut self, bytes: usize, span: &Span) -> Result<(), PlanError> {
        let Some(used) = self.used.checked_add(bytes) else {
            return Err(self.limit_error(span));
        };
        if used > MAX_EXECUTION_PLAN_JSON_BYTES {
            return Err(self.limit_error(span));
        }
        self.used = used;
        Ok(())
    }

    fn limit_error(&self, span: &Span) -> PlanError {
        PlanError::SizeLimit {
            span: span.clone(),
            max_bytes: MAX_EXECUTION_PLAN_JSON_BYTES,
        }
    }
}

struct BoundedCounter {
    remaining: usize,
    written: usize,
}

impl BoundedCounter {
    fn new(remaining: usize) -> Self {
        Self {
            remaining,
            written: 0,
        }
    }
}

impl Write for BoundedCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > self.remaining {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "execution plan size limit reached",
            ));
        }
        self.remaining -= buffer.len();
        self.written += buffer.len();
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
