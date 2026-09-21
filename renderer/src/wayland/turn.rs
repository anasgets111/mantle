//! What one main-loop turn owes: which surfaces repaint and which get their protocol state pushed.

/// Which surfaces a narrowed repaint must cover: the ones a tick advanced, plus every surface
/// already marked `stale`.
///
/// Pure so the narrowing is testable -- `TrackedSurface` holds Wayland objects no test can build,
/// and this is the part that was wrong. `stale` means a surface differs for a reason its tree
/// cannot show, so a repaint chosen by tree identity alone passes it over: a decode refused for
/// pool capacity armed a frame callback, ticked nothing, and was narrowed straight back out
/// (ADR-0185).
pub(super) fn narrowed_repaint_targets(ticked: &[String], stale: &[String]) -> Vec<String> {
    let mut targets = ticked.to_vec();
    for id in stale {
        if !targets.iter().any(|target| target == id) {
            targets.push(id.clone());
        }
    }
    targets
}

/// What changed on one turn of the main loop, as the repaint decision reads it.
pub(super) struct TurnChanges {
    /// A re-resolve ran, so any tree in the scene may differ.
    pub(super) passed: bool,
    /// Whether the pass was narrowed to targeted instances, rather than whole-scene.
    pub(super) targeted: bool,
    /// A tween tick advanced at least one instance. Never true on the same turn as `passed`.
    pub(super) ticked: bool,
    /// Some mapped surface owes a repaint its tree cannot ask for (ADR-0185).
    pub(super) stale: bool,
    /// A keystroke reached a field, moving a caret `field_focus_for` draws.
    pub(super) typed: bool,
    /// A decode landed, invalidating by file across every list that draws it.
    pub(super) landed: bool,
}

/// How wide this turn's repaint has to be.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Repaint {
    Nothing,
    /// The ticked instances plus whatever is `stale`; see [`narrowed_repaint_targets`].
    Narrowed,
    Everything,
}

/// A turn that only ticked owes the screen exactly the surfaces it advanced, and `tick` just
/// named them. Every other reason to repaint is scene-wide: a pass can change any tree, a
/// keystroke moves a caret through `field_focus_for`, and a landed decode invalidates by file
/// across every list that draws it.
///
/// So `passed` rules the narrowing out on its own, and it has to be the pass flag rather than the
/// "did anything change" one the main loop also derives from the tick. Reading the derived flag
/// let a pass that changed a panel be narrowed down to an unrelated `stale` wallpaper, and the
/// panel's own repaint was simply dropped: the tick list it narrowed by was empty, because a turn
/// that re-resolves does not tick.
///
/// A surface left `stale` by a decode turned away for capacity owes a repaint that no tree and no
/// landing can ask for, so it is its own reason to reach one (ADR-0185).
pub(super) fn repaint_for_turn(changes: TurnChanges) -> Repaint {
    if (changes.passed && !changes.targeted) || changes.typed || changes.landed {
        Repaint::Everything
    } else if (changes.passed && changes.targeted) || changes.ticked || changes.stale {
        Repaint::Narrowed
    } else {
        Repaint::Nothing
    }
}

/// Which surfaces owe a protocol-state push this turn.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum StateScope {
    /// Every tracked surface, because a pass can change any tree.
    Everything,
    /// Targeted instances from a narrowed pass.
    Targeted,
    /// The instances a tick named, and no others.
    Ticked,
    Nothing,
}

/// The protocol-state half of a turn: what [`App::apply_resolved_state`](super::App::apply_resolved_state) is run over, and whether
/// ADR-0051's popup latch still has to be looked at for the popups that misses.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SurfaceStateWork {
    pub(super) scope: StateScope,
    pub(super) popup_latch: bool,
}

/// `apply_resolved_state` is a role spec parse and an undiffed `wl_region` round trip per surface.
/// A pass earns that for all of them. A tick earns it for the instances it named and no others:
/// `Scene::tick` mutates only the trees it returns, so every other surface still has the tree its
/// last push came from. Eighteen mapped surfaces at 60 Hz is otherwise seventeen region round
/// trips a frame for surfaces the narrowed repaint will not even paint.
///
/// The latch is the exception, and it is an exception because it does not come from the scene at
/// all. A press or release arms `input_serial` and bumps `pointer_input_count`, and the loop
/// clears the serial at the end of that same turn (ADR-0049 amendment). A click whose handler
/// writes no signal -- `on_click` setting an already-true `visible` -- re-resolves nothing, so
/// nothing would look at whether the compositor has dismissed a popup the config still calls
/// visible. That reopen used to depend on some unrelated surface happening to be mid-tween.
pub(super) fn surface_state_for_turn(
    passed: bool,
    targeted: bool,
    ticked: bool,
    armed_input: bool,
) -> SurfaceStateWork {
    let scope = match (passed, targeted, ticked) {
        (true, false, _) => StateScope::Everything,
        (true, true, _) => StateScope::Targeted,
        (false, _, true) => StateScope::Ticked,
        (false, _, false) => StateScope::Nothing,
    };
    // `Everything` has already visited every popup with this turn's serial in hand.
    let popup_latch = armed_input && scope != StateScope::Everything;
    SurfaceStateWork { scope, popup_latch }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0185. A decode refused for pool capacity records no slot, so asking again is the whole
    /// retry -- and only a paint asks. The surface is marked `stale` and arms a frame callback,
    /// but the callback advances no tween, so `Scene::tick` names nothing and a repaint narrowed
    /// to the ticked ids covers no surface at all. That is the stall, and this is the narrowing
    /// that has to stop causing it.
    #[test]
    fn a_narrowed_repaint_still_covers_a_stale_surface_no_tick_named() {
        let id = |s: &str| s.to_string();

        // The bug: nothing ticked, one surface stale. Narrowing by tick alone repaints nothing.
        assert_eq!(narrowed_repaint_targets(&[], &[id("wallpaper@eDP-1")]), vec![id("wallpaper@eDP-1")]);

        // A tick elsewhere must not narrow the stale surface out, which is the case that actually
        // happens: a clock ticks every second while a wallpaper waits on a refused decode.
        assert_eq!(
            narrowed_repaint_targets(&[id("bar@eDP-1")], &[id("wallpaper@eDP-1")]),
            vec![id("bar@eDP-1"), id("wallpaper@eDP-1")]
        );

        // Both at once is one repaint, not two.
        assert_eq!(narrowed_repaint_targets(&[id("bar@eDP-1")], &[id("bar@eDP-1")]), vec![id("bar@eDP-1")]);

        // Nothing owed, nothing painted: the idle turn stays idle (ADR-0124).
        assert!(narrowed_repaint_targets(&[], &[]).is_empty());
    }

    /// The narrowing above is only ever right for a turn that did not re-resolve. A pass can
    /// change any tree, and it does not tick, so its `ticked` list is empty: narrowing by it
    /// repaints the stale surface and drops the surface the pass actually changed.
    #[test]
    fn a_pass_repaints_everything_even_when_something_else_is_stale() {
        let turn = |passed, ticked, stale, typed, landed| {
            repaint_for_turn(TurnChanges { passed, targeted: false, ticked, stale, typed, landed })
        };

        // The bug: a pass changed a panel while a wallpaper waited on a refused decode. The
        // narrowed repaint covers the wallpaper and the panel never reaches the screen.
        assert_eq!(turn(true, false, true, false, false), Repaint::Everything);
        assert_eq!(turn(true, false, false, false, false), Repaint::Everything);

        // A targeted pass repaints narrowed when neither typed nor landed.
        assert_eq!(
            repaint_for_turn(TurnChanges {
                passed: true,
                targeted: true,
                ticked: false,
                stale: false,
                typed: false,
                landed: false,
            }),
            Repaint::Narrowed
        );

        // A tween frame is what narrowing exists for, stale surface or not (ADR-0178, ADR-0185).
        assert_eq!(turn(false, true, false, false, false), Repaint::Narrowed);
        assert_eq!(turn(false, false, true, false, false), Repaint::Narrowed);

        // A caret and a landed decode are both scene-wide, and outrank a tick on the same turn.
        assert_eq!(turn(false, true, false, true, false), Repaint::Everything);
        assert_eq!(turn(false, true, false, false, true), Repaint::Everything);

        // An idle turn paints nothing and stays timeout-free (ADR-0124).
        assert_eq!(turn(false, false, false, false, false), Repaint::Nothing);
    }

    /// The protocol-state half of the same turn. Narrowing it to the ticked instances is the
    /// point, and ADR-0051's latch is what that narrowing must not take with it: a click whose
    /// handler writes no signal re-resolves nothing, so a popup the compositor dismissed while
    /// `visible` stayed true would be reopened only when some unrelated surface was mid-tween.
    #[test]
    fn a_click_gets_the_popup_latch_looked_at_whatever_else_the_turn_did() {
        let turn = |passed, ticked, armed_input| surface_state_for_turn(passed, false, ticked, armed_input);

        // A pass visits every popup with this turn's serial in hand, so the latch is not owed
        // twice.
        assert_eq!(turn(true, false, true), SurfaceStateWork { scope: StateScope::Everything, popup_latch: false });
        assert_eq!(turn(true, false, false), SurfaceStateWork { scope: StateScope::Everything, popup_latch: false });

        // A targeted pass sets StateScope::Targeted and preserves popup latch on armed input.
        assert_eq!(
            surface_state_for_turn(true, true, false, true),
            SurfaceStateWork { scope: StateScope::Targeted, popup_latch: true }
        );

        // A tween frame re-derives state for what it advanced. The click on top of it is still
        // owed the latch, because the surfaces the tick named are not the popup's.
        assert_eq!(turn(false, true, false), SurfaceStateWork { scope: StateScope::Ticked, popup_latch: false });
        assert_eq!(turn(false, true, true), SurfaceStateWork { scope: StateScope::Ticked, popup_latch: true });

        // The case that was never handled at all: a click, no signal written, nothing animating.
        assert_eq!(turn(false, false, true), SurfaceStateWork { scope: StateScope::Nothing, popup_latch: true });

        // An idle turn pushes nothing (ADR-0124).
        assert_eq!(turn(false, false, false), SurfaceStateWork { scope: StateScope::Nothing, popup_latch: false });
    }
}
