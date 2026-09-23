//! Inhibit half of `mantle.idle` (ADR-0032): login1 `Inhibit` on the existing system bus, with
//! per-holder refcounts, a hand-written proxy, and shared fd state.
//!
//! Holds arrive from two sides. A config calls `idle:inhibit(reason)`; a session client calls
//! `org.freedesktop.ScreenSaver.Inhibit`, which is where a browser's video hold lands (ADR-0231).
//! Both take the one logind fd through the same refcount, so the gate has a single source.

use std::collections::{BTreeMap, HashMap};

use super::controller::IdleController;
use super::state::IdleInhibitor;

/// One refcount transition's fd action: open at 0->1 or close at ->0, never both.
/// [`apply_inhibit`] reports only `should_open_fd`; the others only `should_close_fd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InhibitTransition {
    pub should_open_fd: bool,
    pub should_close_fd: bool,
}

fn total(counts: &HashMap<u32, u32>) -> u32 {
    counts.values().sum()
}

/// `idle:inhibit(reason)`'s refcount half: increments `generation_id`; `should_open_fd` is true
/// only when the global total was zero before this call (ADR-0032's 0->1 rule).
pub fn apply_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    *counts.entry(generation_id).or_insert(0) += 1;
    InhibitTransition { should_open_fd: total_before == 0, should_close_fd: false }
}

/// `idle:release_inhibit()` decrements `generation_id`; `should_close_fd` is true only when the
/// global total reaches zero. Releasing an empty generation is a silent `saturating_sub` no-op.
pub fn apply_release_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    if let Some(count) = counts.get_mut(&generation_id) {
        *count = count.saturating_sub(1);
    }
    let total_after = total(counts);
    InhibitTransition { should_open_fd: false, should_close_fd: total_before > 0 && total_after == 0 }
}

/// Every `ScreenSaver` client at once, as one entry in [`InhibitState::counts`]. Generation ids
/// are handed out from 1 upwards, so the top of the range can never be one, and
/// [`cleanup_generation_inhibit`] can never drop a browser's hold on a config reload.
pub(crate) const SCREENSAVER_HOLDER: u32 = u32::MAX;

/// `ScreenSaver.Inhibit`: records the hold and reports the same 0->1 transition a config's hold
/// does. The cookie is the client's handle for releasing it.
pub(crate) fn apply_screensaver_inhibit(
    state: &mut InhibitState,
    peer: String,
    hold: IdleInhibitor,
) -> (u32, InhibitTransition) {
    let cookie = state.next_cookie;
    // Never hand out 0: it is a legal cookie that clients routinely read as failure.
    state.next_cookie = cookie.wrapping_add(1).max(1);
    state.screensaver.insert(cookie, (peer, hold));
    (cookie, apply_inhibit(&mut state.counts, SCREENSAVER_HOLDER))
}

/// `ScreenSaver.UnInhibit`. `None` for a cookie nothing holds, which must not move the refcount:
/// a client releasing twice would otherwise drop someone else's hold.
pub(crate) fn apply_screensaver_release(state: &mut InhibitState, cookie: u32) -> Option<InhibitTransition> {
    state.screensaver.remove(&cookie)?;
    Some(apply_release_inhibit(&mut state.counts, SCREENSAVER_HOLDER))
}

/// Every cookie a departed peer held (ADR-0231). A client that crashes mid-video sends no
/// `UnInhibit`, so without this the session never idles again.
pub(crate) fn drop_screensaver_peer(state: &mut InhibitState, departed: &str) -> Option<InhibitTransition> {
    let cookies: Vec<u32> =
        state.screensaver.iter().filter(|(_, (peer, _))| peer == departed).map(|(cookie, _)| *cookie).collect();
    // Fold rather than return each: only the last release can reach zero, and the caller closes
    // the fd once.
    cookies.into_iter().filter_map(|cookie| apply_screensaver_release(state, cookie)).last()
}

/// Inhibit half of `reset_registrations` (ADR-0006/ADR-0032): zero `generation_id`'s count;
/// close the fd when the global total reaches zero, so a crashed last holder releases it.
pub fn cleanup_generation_inhibit(counts: &mut HashMap<u32, u32>, generation_id: u32) -> InhibitTransition {
    let total_before = total(counts);
    counts.remove(&generation_id);
    let total_after = total(counts);
    InhibitTransition { should_open_fd: false, should_close_fd: total_before > 0 && total_after == 0 }
}

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
pub(crate) trait Login1Manager {
    #[zbus(name = "Inhibit")]
    fn inhibit(&self, what: &str, who: &str, why: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedFd>;

    /// Current `block` inhibitors, colon-separated, e.g. `"idle:handle-power-key"`. A real,
    /// change-notified property lets one watch cover every holder, including this shell (ADR-0139).
    #[zbus(property, name = "BlockInhibited")]
    fn block_inhibited(&self) -> zbus::Result<String>;

    /// Every held inhibitor's `what`, `who`, `why`, `mode`, `uid`, and `pid`. Called once per
    /// `BlockInhibited` change, never on a timer (ADR-0139/ADR-0141).
    #[zbus(name = "ListInhibitors")]
    fn list_inhibitors(&self) -> zbus::Result<Vec<super::state::InhibitorRow>>;
}

/// ADR-0032 fixes `what`/`who`/`mode`; only `mode = "block"` stops systemd's auto-suspend-on-idle,
/// while `delay` merely postpones it.
pub(crate) const INHIBIT_WHAT: &str = "idle";
pub(crate) const INHIBIT_WHO: &str = "mantle";
pub(crate) const INHIBIT_MODE: &str = "block";

/// KDE's screen locker registered both object paths, and clients have expected one or the other
/// ever since; niri and noctalia both answer on both (ADR-0231).
pub(crate) const SCREENSAVER_BUS_NAME: &str = "org.freedesktop.ScreenSaver";
pub(crate) const SCREENSAVER_OBJECT_PATHS: [&str; 2] = ["/org/freedesktop/ScreenSaver", "/ScreenSaver"];

pub(crate) struct InhibitState {
    /// Refcount per holder: one per config generation, plus [`SCREENSAVER_HOLDER`] for every
    /// `org.freedesktop.ScreenSaver` client together. Only the global total decides the fd.
    pub(crate) counts: HashMap<u32, u32>,
    /// Live `ScreenSaver` holds by cookie: the peer that asked, and what it said. Beside `counts`
    /// under one lock, so the roster and the refcount cannot drift. Ordered, so a drawn roster
    /// keeps its order between pushes.
    pub(crate) screensaver: BTreeMap<u32, (String, IdleInhibitor)>,
    pub(crate) next_cookie: u32,
    pub(crate) fd: Option<zbus::zvariant::OwnedFd>,
}

/// The exported `org.freedesktop.ScreenSaver` object (ADR-0231), registered on both object paths
/// KDE established and every client since expects.
#[derive(Clone)]
pub(crate) struct ScreenSaver {
    pub(crate) controller: IdleController,
}

#[zbus::interface(name = "org.freedesktop.ScreenSaver")]
impl ScreenSaver {
    /// `application_name` is empty whenever the caller is xdg-desktop-portal on an application's
    /// behalf, which is how Firefox and Chromium arrive, so `reason` is usually the only label.
    async fn inhibit(
        &self,
        application_name: String,
        reason: String,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<u32> {
        let Some(peer) = header.sender().map(|sender| sender.to_string()) else {
            return Err(zbus::fdo::Error::Failed("Inhibit arrived without a sender".to_string()));
        };
        Ok(self.controller.screensaver_inhibit(peer, application_name, reason).await)
    }

    async fn un_inhibit(&self, cookie: u32) -> zbus::fdo::Result<()> {
        if !self.controller.screensaver_release(cookie).await {
            return Err(zbus::fdo::Error::Failed(format!("no inhibitor holds cookie {cookie}")));
        }
        Ok(())
    }
}

pub(crate) struct LiveInhibit {
    /// Existing Supervisor system-bus connection. `Login1ManagerProxy` is built fresh per
    /// [`IdleController::inhibit`] call; caching would make one startup failure permanent.
    pub(crate) system_bus: zbus::Connection,
    /// `tokio::sync::Mutex`: refcount decision, D-Bus call, and `fd` write are one critical section
    /// held across `.await`, which `std::sync::Mutex` cannot do.
    pub(crate) state: tokio::sync::Mutex<InhibitState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- apply_inhibit / apply_release_inhibit (inhibit refcount arithmetic, TDD seam 2) ----

    #[test]
    fn apply_inhibit_opens_the_fd_on_the_global_zero_to_one_transition() {
        let mut counts = HashMap::new();
        let transition = apply_inhibit(&mut counts, 1);
        assert_eq!(transition, InhibitTransition { should_open_fd: true, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&1));
    }

    #[test]
    fn apply_inhibit_does_not_reopen_the_fd_for_a_second_concurrent_generation() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        let transition = apply_inhibit(&mut counts, 2);
        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&1));
        assert_eq!(counts.get(&2), Some(&1));
    }

    #[test]
    fn apply_release_inhibit_closes_the_fd_on_the_global_one_to_zero_transition() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        let transition = apply_release_inhibit(&mut counts, 1);
        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: true });
        assert_eq!(counts.get(&1), Some(&0));
    }

    #[test]
    fn apply_release_inhibit_from_one_generation_does_not_close_while_another_still_holds() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        apply_inhibit(&mut counts, 2);

        let transition = apply_release_inhibit(&mut counts, 1);

        assert_eq!(
            transition,
            InhibitTransition { should_open_fd: false, should_close_fd: false },
            "generation 2 still holds an inhibit"
        );
        assert_eq!(counts.get(&1), Some(&0));
        assert_eq!(counts.get(&2), Some(&1));
    }

    #[test]
    fn apply_release_inhibit_on_an_already_zero_generation_is_a_silent_no_op() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        apply_release_inhibit(&mut counts, 1);

        let transition = apply_release_inhibit(&mut counts, 1);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&0), "must not underflow below zero");
    }

    #[test]
    fn apply_release_inhibit_on_a_never_registered_generation_is_a_silent_no_op() {
        let mut counts = HashMap::new();
        let transition = apply_release_inhibit(&mut counts, 42);
        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert!(!counts.contains_key(&42));
    }

    // ---- ScreenSaver holds (ADR-0231): the inbound half, sharing one fd with the config's ----

    fn state() -> InhibitState {
        InhibitState { counts: HashMap::new(), screensaver: BTreeMap::new(), next_cookie: 1, fd: None }
    }

    /// What the portal sends: no application name, a reason worth drawing.
    fn hold(why: &str) -> IdleInhibitor {
        IdleInhibitor { who: String::new(), why: why.to_string() }
    }

    #[test]
    fn the_first_screensaver_hold_opens_the_fd_and_cookies_start_at_one() {
        let mut state = state();

        let (first, transition) = apply_screensaver_inhibit(&mut state, ":1.42".into(), hold("Playing video"));

        assert_eq!(first, 1, "0 is a cookie some clients reject");
        assert!(transition.should_open_fd);
        let (second, transition) = apply_screensaver_inhibit(&mut state, ":1.42".into(), hold("Playing audio"));
        assert_eq!(second, 2);
        assert!(!transition.should_open_fd, "the same client's second hold is already covered");
        assert_eq!(state.screensaver.len(), 2);
    }

    /// The whole point of one refcount: a browser's hold and a config's `idle:inhibit` are the
    /// same fd, so neither can drop it while the other wants it.
    #[test]
    fn a_screensaver_hold_neither_reopens_nor_closes_the_fd_a_config_generation_holds() {
        let mut state = state();
        apply_inhibit(&mut state.counts, 1);

        let (cookie, transition) = apply_screensaver_inhibit(&mut state, ":1.42".into(), hold("Playing video"));
        assert!(!transition.should_open_fd, "generation 1 already opened it");

        let transition = apply_screensaver_release(&mut state, cookie).expect("a live cookie");
        assert!(!transition.should_close_fd, "generation 1 still holds it");
        assert!(apply_release_inhibit(&mut state.counts, 1).should_close_fd, "and now the last holder is gone");
    }

    #[test]
    fn releasing_an_unknown_cookie_moves_no_refcount() {
        let mut state = state();
        let (cookie, _) = apply_screensaver_inhibit(&mut state, ":1.42".into(), hold("Playing video"));
        apply_screensaver_release(&mut state, cookie).expect("a live cookie");

        assert_eq!(apply_screensaver_release(&mut state, cookie), None, "releasing twice must not close it again");
        assert_eq!(apply_screensaver_release(&mut state, 9999), None);
        assert_eq!(total(&state.counts), 0);
    }

    #[test]
    fn a_departed_peer_loses_its_own_holds_and_no_one_elses() {
        let mut state = state();
        apply_screensaver_inhibit(&mut state, ":1.42".into(), hold("Playing video"));
        apply_screensaver_inhibit(&mut state, ":1.42".into(), hold("Playing audio"));
        let (survivor, _) = apply_screensaver_inhibit(&mut state, ":1.99".into(), hold("Presenting"));

        let transition = drop_screensaver_peer(&mut state, ":1.42").expect("that peer held two");

        assert!(!transition.should_close_fd, ":1.99 still holds one");
        assert_eq!(state.screensaver.keys().copied().collect::<Vec<u32>>(), vec![survivor]);
        assert!(drop_screensaver_peer(&mut state, ":1.99").expect("the last holder").should_close_fd);
        assert_eq!(drop_screensaver_peer(&mut state, ":1.42"), None, "a peer holding nothing releases nothing");
    }

    /// The reason [`SCREENSAVER_HOLDER`] sits at the top of the range: a config reload reaps its
    /// own generation, and a film playing through it must not lose its hold.
    #[test]
    fn a_config_reload_reaps_its_generation_without_touching_a_screensaver_hold() {
        let mut state = state();
        apply_inhibit(&mut state.counts, 1);
        let (cookie, _) = apply_screensaver_inhibit(&mut state, ":1.42".into(), hold("Playing video"));

        let transition = cleanup_generation_inhibit(&mut state.counts, 1);

        assert!(!transition.should_close_fd, "the browser still holds the session awake");
        assert_eq!(state.screensaver.len(), 1);
        assert!(apply_screensaver_release(&mut state, cookie).expect("a live cookie").should_close_fd);
    }

    // ---- cleanup_generation_inhibit (TDD seam 5, inhibit half) ----

    #[test]
    fn cleanup_generation_inhibit_zeros_only_the_named_generation_and_closes_if_it_was_the_last() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);

        let transition = cleanup_generation_inhibit(&mut counts, 1);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: true });
        assert!(!counts.contains_key(&1));
    }

    #[test]
    fn cleanup_generation_inhibit_does_not_close_while_another_generation_still_holds() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);
        apply_inhibit(&mut counts, 2);

        let transition = cleanup_generation_inhibit(&mut counts, 1);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert!(!counts.contains_key(&1));
        assert_eq!(counts.get(&2), Some(&1), "generation 2's own count must be untouched");
    }

    #[test]
    fn cleanup_generation_inhibit_on_an_untracked_generation_is_a_silent_no_op() {
        let mut counts = HashMap::new();
        apply_inhibit(&mut counts, 1);

        let transition = cleanup_generation_inhibit(&mut counts, 99);

        assert_eq!(transition, InhibitTransition { should_open_fd: false, should_close_fd: false });
        assert_eq!(counts.get(&1), Some(&1));
    }

    // ---- Login1ManagerProxy::inhibit (TDD seam 3: real D-Bus call, p2p pattern) ----

    use crate::capabilities::test_support::p2p_pair_serving;

    /// Stand-in for logind's `org.freedesktop.login1.Manager`; returns `/dev/null` so the proxy
    /// receives a real `OwnedFd`.
    struct StubLogin1Manager {
        calls: tokio::sync::mpsc::UnboundedSender<(String, String, String, String)>,
    }

    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl StubLogin1Manager {
        #[zbus(name = "Inhibit")]
        fn inhibit(&self, what: String, who: String, why: String, mode: String) -> zbus::zvariant::OwnedFd {
            let _ = self.calls.send((what, who, why, mode));
            let file = std::fs::File::open("/dev/null").expect("open /dev/null for a test fd");
            let owned: std::os::fd::OwnedFd = file.into();
            zbus::zvariant::OwnedFd::from(owned)
        }
    }

    #[tokio::test]
    async fn login1_manager_inhibit_sends_the_expected_arguments_and_returns_a_fd() {
        let (calls_tx, mut calls_rx) = tokio::sync::mpsc::unbounded_channel();
        let (caller_side, _manager_side) =
            p2p_pair_serving(|peer| peer.serve_at("/org/freedesktop/login1", StubLogin1Manager { calls: calls_tx }))
                .await;

        let proxy: Login1ManagerProxy<'_> = zbus::proxy::Builder::new(&caller_side)
            .destination("org.mantle.test")
            .expect("valid destination bus name")
            .path("/org/freedesktop/login1")
            .expect("valid object path")
            .interface("org.freedesktop.login1.Manager")
            .expect("valid interface name")
            .build()
            .await
            .expect("failed to build a p2p Login1ManagerProxy");

        let fd =
            proxy.inhibit("idle", "mantle", "playing a video", "block").await.expect("Inhibit call should succeed");
        // OwnedFd::drop must close it cleanly without panicking.
        drop(fd);

        let (what, who, why, mode) = calls_rx.recv().await.expect("stub Login1Manager never received Inhibit");
        assert_eq!(what, "idle");
        assert_eq!(who, "mantle");
        assert_eq!(why, "playing a video");
        assert_eq!(mode, "block");
    }
}
