//! Per-task managed parser admission for the isolated extraction runtime.
//!
//! Source buffers are owned by the ready-input partition. Parsers may still
//! allocate decoded trees, indexes, graph facts, and deduplication state while
//! they inspect those buffers. This module gives registered byte adapters a
//! conservative worker-local estimate before parsing starts and fact credits
//! that builders consume before retaining output. It does not instrument the
//! allocator or claim a hard process-RSS ceiling; dependency-specific static
//! limits remain part of the boundary.

use std::cell::Cell;

const FIXED_PARSER_SCRATCH_BYTES: usize = 16 * 1024;
const SOURCE_SCRATCH_EXPANSION: usize = 16;
const RETAINED_BYTES_PER_FACT: usize = 2 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveFactBudget {
    remaining: usize,
    exhausted: bool,
}

thread_local! {
    static ACTIVE_FACT_BUDGET: Cell<Option<ActiveFactBudget>> = const { Cell::new(None) };
}

/// A conservative plan for one registered semantic parser invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ParserPlan {
    max_facts: usize,
}

impl ParserPlan {
    /// Reserve scratch proportional to the admitted source before granting any
    /// retained-fact credits. Overflow and undersized managed allowances reject
    /// before a registered parser or deserializer runs.
    pub(crate) fn for_source(allowance_bytes: usize, source_bytes: usize) -> Option<Self> {
        let scratch = source_bytes
            .checked_mul(SOURCE_SCRATCH_EXPANSION)?
            .checked_add(FIXED_PARSER_SCRATCH_BYTES)?;
        let remaining = allowance_bytes.checked_sub(scratch)?;
        let max_facts = remaining / RETAINED_BYTES_PER_FACT;
        (max_facts > 0).then_some(Self { max_facts })
    }

    pub(crate) const fn max_facts(self) -> usize {
        self.max_facts
    }
}

struct RestoreBudget(Option<ActiveFactBudget>);

impl Drop for RestoreBudget {
    fn drop(&mut self) {
        ACTIVE_FACT_BUDGET.set(self.0);
    }
}

/// Run one semantic adapter with its fact credits installed on this fixed CPU
/// owner. Nested calls restore the enclosing state even if parsing unwinds.
pub(crate) fn with_plan<T>(plan: ParserPlan, operation: impl FnOnce() -> T) -> (T, bool) {
    let previous = ACTIVE_FACT_BUDGET.replace(Some(ActiveFactBudget {
        remaining: plan.max_facts,
        exhausted: false,
    }));
    let _restore = RestoreBudget(previous);
    let value = operation();
    let exhausted = ACTIVE_FACT_BUDGET
        .get()
        .is_some_and(|budget| budget.exhausted);
    (value, exhausted)
}

/// Admit facts before their owned node, edge, or hyperedge representations are
/// retained. This is conservative output accounting, not allocator tracking.
/// Legacy extraction has no active dynamic budget and retains its fixed limits.
pub(crate) fn try_reserve_facts(facts: usize) -> bool {
    ACTIVE_FACT_BUDGET.with(|active| {
        let Some(mut budget) = active.get() else {
            return true;
        };
        let Some(remaining) = budget.remaining.checked_sub(facts) else {
            budget.exhausted = true;
            active.set(Some(budget));
            return false;
        };
        budget.remaining = remaining;
        active.set(Some(budget));
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_plan_rejects_before_scratch_can_exceed_the_allowance() {
        assert!(ParserPlan::for_source(64 * 1024, 4 * 1024).is_none());
        assert!(ParserPlan::for_source(128 * 1024, 4 * 1024).is_some());
        assert!(ParserPlan::for_source(usize::MAX, usize::MAX).is_none());
    }

    #[test]
    fn retained_fact_credits_are_consumed_and_restore_after_scope() {
        let plan = ParserPlan { max_facts: 2 };
        let (_, exhausted) = with_plan(plan, || {
            assert!(try_reserve_facts(1));
            assert!(try_reserve_facts(1));
            assert!(!try_reserve_facts(1));
        });
        assert!(exhausted);
        assert!(try_reserve_facts(usize::MAX));
    }
}
