//! Resolve-pass timing spans and the retained-tree census.

use super::*;

/// Where one resolve pass spends itself, split the three ways [`Scene::apply_one_instance`]
/// divides into: saving the retained tree for rollback, running Lua and updating the solver tree,
/// then solving and measuring. `ms resolve` is one number for all three plus the tween tick, which
/// says the phase is expensive without saying which part is.
///
/// Sums to less than `ms resolve`: a tick-only turn resolves nothing, and the per-pass work
/// outside `apply_one_instance` (the budget, `start_secure_submit_capabilities`) is in neither.
/// Never past it: the turn loop drops what `apply_instances` and `handle_apply_pending` accumulate
/// from dispatch, which `ms resolve` does not cover either.
#[derive(Clone, Copy, Default)]
pub struct ResolveSplit {
    pub clone: Duration,
    pub list: Duration,
    pub resolve: Duration,
    pub solve: Duration,
}

/// One getenv for the process, like the profilers this feeds: `--profile` cannot come and go while
/// the Renderer runs. Off, [`open_span`] reads no clock, the switch `Phases` already uses.
pub(crate) fn timing_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| shared::profile_interval().is_some())
}

/// Opens a timing span, or `None` with the profile off.
pub(super) fn open_span() -> Option<Instant> {
    timing_on().then(Instant::now)
}

/// Closes `at` into `total` and reopens, so three spans cost three clock reads, not six.
pub(super) fn close(at: &mut Option<Instant>, total: &mut Duration) {
    if let Some(started) = *at {
        let now = Instant::now();
        *total += now.duration_since(started);
        *at = Some(now);
    }
}

/// Adds `node` and its descendants to the running node and property totals. Shared by
/// [`Scene::census`] and [`Scene::census_by_surface`] so the per-surface figures always sum to the
/// total the same report prints beside them.
pub(super) fn census_walk(node: &ResolvedNode, nodes: &mut usize, properties: &mut usize) {
    *nodes += 1;
    *properties += node.properties.len();
    for child in &node.children {
        census_walk(child, nodes, properties);
    }
}
