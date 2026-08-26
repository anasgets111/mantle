//! Presentation-Before-Authority (PBA) hot-reload orchestration (build-steps.md Phase 8,
//! `docs/oblisk-supervisor-services-dbus.md` § 15.1-15.4).
//!
//! This module is the ordering/gating state machine only: the numbered sequence build-steps.md
//! draws (Overlapping Spawn, State Hydration, Null-Buffer Staging, Activate Draw, Evidence
//! Verification, Swap & Reap), implemented against two real primitives and one seam:
//!
//! - Real process lifecycle: [`process::spawn_group_leader`] and [`process::reap_process_group`]
//!   (Phase 7, `supervisor/src/process/mod.rs`) do the actual spawning and reaping. This module
//!   is their first real caller, per docs/adr/0018-process-group-primitives-without-process-run-
//!   or-a-registry.md's upgrade-path item (c).
//! - [`CandidateLink`]: an abstract trait standing in for the real Unix control-socket wire
//!   transport, which doesn't exist yet. Its four methods are each one operation § 15.2/15.3
//!   describes crossing that socket -- see the trait doc comment for the § reference each maps
//!   to. `shared::StateSnapshot` is reused as-is for state hydration's payload (§ 15.2 point 1
//!   names exactly this: "a state snapshot pushed by the Supervisor"). `shared::CommandEnvelope`
//!   is *not* reused for `ActivateDraw`: § 7.2's envelope is a generation-guarded wrapper around
//!   a Lua-initiated *write action* traveling Renderer -> Supervisor (capability/action/
//!   arguments/expected_revision), the opposite direction and a different shape from a
//!   Supervisor-issued one-off activation nonce -- forcing it on would invent a mismatched
//!   payload rather than reuse a real fit.
//!
//! What this module does *not* build -- see
//! docs/adr/0019-pba-control-socket-and-lua-ast-evaluation-deferred.md: the real control-socket
//! transport, Lua AST evaluation, the Renderer-side null-buffer/`wp_presentation_feedback`
//! wiring, real NetworkManager/BlueZ hydration payloads, true multi-output evidence fan-out
//! (ADR-0003 describes per-output authority; this module gates on one verified-evidence signal
//! structurally), and any wiring of this module into `main()`'s runtime.
//!
//! Failure semantics (not spelled out by § 15's happy-path text, chosen as the reading
//! consistent with PBA's whole point -- never a black frame, never an unverified swap): any
//! failure before presentation evidence is verified -- a link error, or a deadline expiring --
//! aborts the Candidate (reaps its process group) and leaves Generation `N` untouched and still
//! authoritative. Generation `N` is reaped only after evidence verification succeeds, never
//! before. All four [`CandidateLink`] steps are deadline-gated, not just two: `ready_timeout`
//! bounds both `push_state_snapshot` and `recv_ready_signal`, and `evidence_timeout` bounds both
//! `send_activate_draw` and `recv_presentation_evidence` -- see [`PbaTimings`] and
//! [`drive_handshake`].

use std::io;
use std::time::Duration;

use tokio::process::Child;
use tokio::time::timeout;

use crate::process;

/// The control-socket operations § 15.2-15.3 describe crossing from the Supervisor to the
/// Candidate generation. The real Unix-socket wire transport doesn't exist yet (see the module
/// doc comment and ADR-0019) -- this trait is the seam a fake implementation drives in tests,
/// and whatever transport eventually replaces it implements this same contract.
///
/// ponytail: no real transport implements this yet, and `run_pba` has no runtime caller -- see
/// the module doc comment and docs/adr/0019-pba-control-socket-and-lua-ast-evaluation-deferred.md.
#[allow(dead_code)]
pub trait CandidateLink {
    /// What a control-link call can fail with. Kept generic rather than fixed to `io::Error`
    /// since the real transport's error type doesn't exist yet either.
    type Error: std::fmt::Debug;

    /// § 15.2 point 1 / build-steps.md step 2 ("State Hydration"): push the pre-cached state
    /// snapshot down to the Candidate so it can hydrate its signals without querying the system
    /// itself.
    async fn push_state_snapshot(&mut self, snapshot: &shared::StateSnapshot) -> Result<(), Self::Error>;

    /// § 15.2 points 2-3 / build-steps.md step 3 ("Null-Buffer Staging"): block until the
    /// Candidate signals it has completed its Wayland layer-shell handshake and committed its
    /// null buffers -- i.e. it's ready to receive `ActivateDraw`.
    async fn recv_ready_signal(&mut self) -> Result<(), Self::Error>;

    /// § 15.2 point 3 / build-steps.md step 4 ("Activate Draw"): write the unique, nonce-bound
    /// `ActivateDraw` command telling the Candidate to compile its layout and draw its first
    /// GPU frame.
    async fn send_activate_draw(&mut self, nonce: u64) -> Result<(), Self::Error>;

    /// § 15.3 point 4 / build-steps.md step 5 ("Evidence Verification"): block until the
    /// Candidate transmits presentation evidence for `nonce` -- confirmation the compositor's
    /// `wp_presentation_feedback` `presented` callback fired for the frame `ActivateDraw`
    /// requested.
    async fn recv_presentation_evidence(&mut self, nonce: u64) -> Result<(), Self::Error>;
}

/// Which step of § 15.2-15.3's sequence a [`PbaFailure`] happened during.
///
/// ponytail: no runtime caller yet -- see the module doc comment.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// § 15.2 point 1 / build-steps.md step 2.
    StateHydration,
    /// § 15.2 points 2-3 / build-steps.md step 3.
    NullBufferStaging,
    /// § 15.2 point 3 / build-steps.md step 4.
    ActivateDraw,
    /// § 15.3 / build-steps.md step 5.
    EvidenceVerification,
}

/// Why a PBA reload didn't reach [`PbaOutcome::Promoted`]. Every variant except `SpawnFailed`
/// implies the Candidate's process group was aborted (reaped) before returning -- see the
/// module doc comment's failure-semantics paragraph. Generation `N` is never touched by any of
/// these.
///
/// ponytail: no runtime caller yet -- see the module doc comment.
#[allow(dead_code)]
#[derive(Debug)]
pub enum PbaFailure<E> {
    /// Step 1 (Overlapping Spawn) itself failed. There's no Candidate process to abort --
    /// nothing was spawned.
    SpawnFailed(io::Error),
    /// A `CandidateLink` call returned an error during `stage`.
    Link { stage: Stage, source: E },
    /// `recv_ready_signal` or `recv_presentation_evidence` didn't resolve before its deadline
    /// during `stage`.
    Timeout { stage: Stage },
    /// A failure above happened, and the abort-reap of the Candidate's process group that
    /// followed it *also* failed. Both are kept rather than the reap error replacing the
    /// original, so nothing about why the reload failed in the first place gets lost.
    AbortReapFailed { original: Box<PbaFailure<E>>, reap_error: io::Error },
}

/// A completed PBA reload: Generation `N+1` (`candidate`) is confirmed presented and
/// authoritative. `superseded_reap` is Generation `N`'s reap result -- kept as a `Result`
/// rather than unwrapped here because a reap failure (e.g. `N` stuck in uninterruptible I/O)
/// doesn't undo the promotion decision, which was already safe: it was made strictly after
/// evidence verification, before `N` was ever signaled.
///
/// ponytail: no runtime caller yet -- see the module doc comment.
#[allow(dead_code)]
#[derive(Debug)]
pub struct PbaOutcome {
    pub candidate: Child,
    pub superseded_reap: Result<process::ReapOutcome, io::Error>,
}

/// Runs steps 2-5 (State Hydration through Evidence Verification) against an already-spawned
/// Candidate's `link`. Split out from [`run_pba`] so its one job -- drive the handshake and
/// tag any failure with the [`Stage`] it happened during -- stays separate from spawn/abort/
/// reap, which need the Candidate's process handle that this function never touches.
async fn drive_handshake<L: CandidateLink>(
    link: &mut L,
    snapshot: &shared::StateSnapshot,
    nonce: u64,
    ready_timeout: Duration,
    evidence_timeout: Duration,
) -> Result<(), PbaFailure<L::Error>> {
    timeout(ready_timeout, link.push_state_snapshot(snapshot))
        .await
        .map_err(|_elapsed| PbaFailure::Timeout { stage: Stage::StateHydration })?
        .map_err(|source| PbaFailure::Link { stage: Stage::StateHydration, source })?;

    timeout(ready_timeout, link.recv_ready_signal())
        .await
        .map_err(|_elapsed| PbaFailure::Timeout { stage: Stage::NullBufferStaging })?
        .map_err(|source| PbaFailure::Link { stage: Stage::NullBufferStaging, source })?;

    timeout(evidence_timeout, link.send_activate_draw(nonce))
        .await
        .map_err(|_elapsed| PbaFailure::Timeout { stage: Stage::ActivateDraw })?
        .map_err(|source| PbaFailure::Link { stage: Stage::ActivateDraw, source })?;

    timeout(evidence_timeout, link.recv_presentation_evidence(nonce))
        .await
        .map_err(|_elapsed| PbaFailure::Timeout { stage: Stage::EvidenceVerification })?
        .map_err(|source| PbaFailure::Link { stage: Stage::EvidenceVerification, source })?;

    Ok(())
}

/// Aborts a Candidate that failed before evidence verification: reaps its process group and
/// folds a reap error into `failure` rather than discarding either. The one place that ever
/// reaps a Candidate on a failure path, so every `drive_handshake` error goes through the same
/// cleanup instead of each caller re-implementing it.
async fn abort_candidate<E>(candidate: &mut Child, grace: Duration, failure: PbaFailure<E>) -> PbaFailure<E> {
    match process::reap_process_group(candidate, grace).await {
        Ok(_) => failure,
        Err(reap_error) => PbaFailure::AbortReapFailed { original: Box::new(failure), reap_error },
    }
}

/// The three durations [`run_pba`] gates on: how long to wait for the Candidate's ready signal
/// and its presentation evidence before treating the reload as failed, and how long to give a
/// process group to honor `SIGTERM` before escalating to `SIGKILL` (passed straight through to
/// [`process::reap_process_group`]). Each of `ready_timeout` and `evidence_timeout` actually
/// bounds two handshake steps, not one: § 15.2's "state hydration and ready-signal wait" and
/// § 15.3's "activate draw and evidence wait" are each one logical stage, so `ready_timeout`
/// gates both `push_state_snapshot` and `recv_ready_signal`, and `evidence_timeout` gates both
/// `send_activate_draw` and `recv_presentation_evidence` -- see [`drive_handshake`]. Grouped into
/// one struct purely to keep `run_pba`'s parameter count reasonable -- these three don't share
/// any invariant with each other.
#[allow(dead_code)] // ponytail: no runtime caller yet -- see the module doc comment.
#[derive(Debug, Clone, Copy)]
pub struct PbaTimings {
    pub ready_timeout: Duration,
    pub evidence_timeout: Duration,
    pub reap_grace: Duration,
}

/// Runs one full PBA reload (§ 15.1-15.4, build-steps.md's numbered steps 1-6):
///
/// 1. **Overlapping Spawn**: spawns the Candidate via [`process::spawn_group_leader`], leaving
///    `superseded` (Generation `N`) running and untouched.
/// 2. **State Hydration** through 5. **Evidence Verification**: see [`drive_handshake`] and
///    [`CandidateLink`].
/// 6. **Swap & Reap**: only the Reap half is implemented here -- once evidence is verified,
///    reaps `superseded`'s process group. The Swap half (§ 15.4's input deselection on `N` and
///    promotion signaling to `N+1`) has no code in this module at all; there's no control-socket
///    message for either yet (see ADR-0019 item 6). Generation `N+1` is the returned,
///    still-running [`PbaOutcome::candidate`] -- authoritative from this point on, per § 15.4's
///    promotion step, but only in the sense that the *caller* is left to infer that from which
///    `Child` it now holds, not because this function signals authority transfer over the wire.
///
/// Any failure in steps 1-5 aborts the Candidate and returns `superseded` untouched and still
/// authoritative -- see the module doc comment's failure-semantics paragraph.
///
/// ponytail: no runtime caller yet -- `main.rs` declares `mod reload;` (like Phase 7's
/// `mod process;`) but nothing invokes this from `main()`'s event loop, since there's no real
/// control-socket transport to implement [`CandidateLink`] against yet, and no real second
/// Renderer binary that would cooperate with the handshake this drives. See the module doc
/// comment and docs/adr/0019-pba-control-socket-and-lua-ast-evaluation-deferred.md.
#[allow(dead_code)]
pub async fn run_pba<L: CandidateLink>(
    candidate_cmd: &str,
    candidate_args: &[String],
    superseded: &mut Child,
    link: &mut L,
    snapshot: &shared::StateSnapshot,
    nonce: u64,
    timings: PbaTimings,
) -> Result<PbaOutcome, PbaFailure<L::Error>> {
    let mut candidate = process::spawn_group_leader(candidate_cmd, candidate_args).map_err(PbaFailure::SpawnFailed)?;

    let handshake = drive_handshake(link, snapshot, nonce, timings.ready_timeout, timings.evidence_timeout).await;
    if let Err(failure) = handshake {
        return Err(abort_candidate(&mut candidate, timings.reap_grace, failure).await);
    }

    let superseded_reap = process::reap_process_group(superseded, timings.reap_grace).await;
    Ok(PbaOutcome { candidate, superseded_reap })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn sh_args(script: &str) -> Vec<String> {
        vec!["-c".to_string(), script.to_string()]
    }

    fn sample_snapshot() -> shared::StateSnapshot {
        shared::StateSnapshot { revision: 1, payload: serde_json::json!({}) }
    }

    fn proc_exists(pid: i32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    /// Polls `condition` until it's true or `timeout` elapses -- avoids a flaky single-shot
    /// check immediately after a reap, matching `process::mod`'s own test helper.
    async fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if condition() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[derive(Debug, PartialEq, Eq, Clone)]
    struct FakeLinkError(String);

    /// What each `CandidateLink` call should do, configured up front per test.
    enum StepBehavior {
        Succeed,
        SucceedAfter(Duration),
        Fail,
        /// Never resolves within any reasonable test deadline -- simulates a wedged or silent
        /// Candidate so the orchestrator's own timeout is what has to save the test.
        Hang,
    }

    /// In-memory [`CandidateLink`] fake: every method's behavior is configured up front, and
    /// calls are recorded in order so tests can assert on the sequence observed, not just the
    /// final outcome.
    struct FakeCandidateLink {
        hydration: StepBehavior,
        ready: StepBehavior,
        activate: StepBehavior,
        evidence: StepBehavior,
        calls: Mutex<Vec<&'static str>>,
    }

    impl FakeCandidateLink {
        fn new(ready: StepBehavior, evidence: StepBehavior) -> Self {
            Self::with_all_steps(StepBehavior::Succeed, ready, StepBehavior::Succeed, evidence)
        }

        /// Like [`Self::new`], but also configures `push_state_snapshot` and `send_activate_draw`
        /// -- the two steps `new` hardcodes to always succeed immediately, needed by tests
        /// exercising a hang on either of those specifically.
        fn with_all_steps(hydration: StepBehavior, ready: StepBehavior, activate: StepBehavior, evidence: StepBehavior) -> Self {
            Self { hydration, ready, activate, evidence, calls: Mutex::new(Vec::new()) }
        }

        fn record(&self, call: &'static str) {
            self.calls.lock().expect("test-only lock").push(call);
        }

        async fn run_step(&self, behavior: &StepBehavior) -> Result<(), FakeLinkError> {
            match behavior {
                StepBehavior::Succeed => Ok(()),
                StepBehavior::SucceedAfter(delay) => {
                    tokio::time::sleep(*delay).await;
                    Ok(())
                }
                StepBehavior::Fail => Err(FakeLinkError("candidate link failed".to_string())),
                StepBehavior::Hang => std::future::pending().await,
            }
        }
    }

    impl CandidateLink for FakeCandidateLink {
        type Error = FakeLinkError;

        async fn push_state_snapshot(&mut self, _snapshot: &shared::StateSnapshot) -> Result<(), Self::Error> {
            self.record("push_state_snapshot");
            self.run_step(&self.hydration).await
        }

        async fn recv_ready_signal(&mut self) -> Result<(), Self::Error> {
            self.record("recv_ready_signal");
            self.run_step(&self.ready).await
        }

        async fn send_activate_draw(&mut self, _nonce: u64) -> Result<(), Self::Error> {
            self.record("send_activate_draw");
            self.run_step(&self.activate).await
        }

        async fn recv_presentation_evidence(&mut self, _nonce: u64) -> Result<(), Self::Error> {
            self.record("recv_presentation_evidence");
            self.run_step(&self.evidence).await
        }
    }

    const SHORT_DEADLINE: Duration = Duration::from_millis(80);
    const GRACE: Duration = Duration::from_millis(200);

    /// `PbaTimings` with `reap_grace` fixed to [`GRACE`] -- what every test below wants, since
    /// none of them are exercising the reap-grace/escalation behavior itself (that's
    /// `process::mod`'s own test coverage).
    fn timings(ready_timeout: Duration, evidence_timeout: Duration) -> PbaTimings {
        PbaTimings { ready_timeout, evidence_timeout, reap_grace: GRACE }
    }

    #[tokio::test]
    async fn run_pba_promotes_the_candidate_and_reaps_generation_n_on_full_success() {
        let mut superseded = process::spawn_group_leader("sh", &sh_args("sleep 30")).expect("failed to spawn N");
        let superseded_pid = superseded.id().expect("N has a pid") as i32;
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, StepBehavior::Succeed);

        let mut outcome = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &mut superseded,
            &mut link,
            &sample_snapshot(),
            42,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect("full success must promote");

        assert!(matches!(outcome.superseded_reap, Ok(process::ReapOutcome::ExitedCleanly(_))));
        assert!(!proc_exists(superseded_pid), "generation N must be reaped once promotion succeeds");
        assert_eq!(
            *link.calls.lock().unwrap(),
            vec!["push_state_snapshot", "recv_ready_signal", "send_activate_draw", "recv_presentation_evidence"],
            "the handshake must run in § 15.2-15.3's order"
        );

        // Clean up the newly-promoted candidate rather than leaking the sleep.
        process::reap_process_group(&mut outcome.candidate, GRACE).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn run_pba_does_not_reap_generation_n_until_evidence_is_verified() {
        let mut superseded = process::spawn_group_leader("sh", &sh_args("sleep 30")).expect("failed to spawn N");
        let superseded_pid = superseded.id().expect("N has a pid") as i32;
        // Evidence resolves only after a real delay, giving the assertion below a genuine
        // window to observe N still alive *while* evidence verification is in flight, not just
        // after `run_pba` has already returned.
        let evidence_delay = Duration::from_millis(150);
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, StepBehavior::SucceedAfter(evidence_delay));
        let candidate_args = sh_args("sleep 30");
        let snapshot = sample_snapshot();

        let run = run_pba(
            "sh",
            &candidate_args,
            &mut superseded,
            &mut link,
            &snapshot,
            7,
            timings(Duration::from_millis(500), Duration::from_millis(500)),
        );
        tokio::pin!(run);

        // Poll well inside the configured evidence delay -- if `reap_process_group(superseded,
        // ..)` had already run by this point, N would be gone; catching that here is the actual
        // ordering assertion, not just checking the final state after everything settled.
        tokio::select! {
            _ = &mut run => panic!("run_pba resolved before the evidence delay elapsed; the timing margin below is too tight"),
            _ = tokio::time::sleep(evidence_delay / 3) => {
                assert!(proc_exists(superseded_pid), "generation N must not be reaped while evidence verification is still pending");
            }
        }

        let mut outcome = run.await.expect("delayed-but-successful evidence must still promote");
        assert!(!proc_exists(superseded_pid), "N should be reaped once evidence is verified");
        process::reap_process_group(&mut outcome.candidate, GRACE).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_and_leaves_generation_n_untouched_when_ready_signal_times_out() {
        let mut superseded = process::spawn_group_leader("sh", &sh_args("sleep 30")).expect("failed to spawn N");
        let superseded_pid = superseded.id().expect("N has a pid") as i32;
        let mut link = FakeCandidateLink::new(StepBehavior::Hang, StepBehavior::Succeed);

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &mut superseded,
            &mut link,
            &sample_snapshot(),
            1,
            timings(Duration::from_millis(30), SHORT_DEADLINE),
        )
        .await
        .expect_err("a hanging ready signal must not promote");

        assert!(matches!(failure, PbaFailure::Timeout { stage: Stage::NullBufferStaging }));
        assert!(proc_exists(superseded_pid), "generation N must stay untouched on a failed handoff");

        process::reap_process_group(&mut superseded, GRACE).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_and_leaves_generation_n_untouched_when_evidence_times_out() {
        let mut superseded = process::spawn_group_leader("sh", &sh_args("sleep 30")).expect("failed to spawn N");
        let superseded_pid = superseded.id().expect("N has a pid") as i32;
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, StepBehavior::Hang);

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &mut superseded,
            &mut link,
            &sample_snapshot(),
            2,
            timings(SHORT_DEADLINE, Duration::from_millis(30)),
        )
        .await
        .expect_err("evidence that never arrives must not promote");

        assert!(matches!(failure, PbaFailure::Timeout { stage: Stage::EvidenceVerification }));
        assert!(proc_exists(superseded_pid), "generation N must stay untouched on a failed handoff");

        process::reap_process_group(&mut superseded, GRACE).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_and_leaves_generation_n_untouched_when_state_hydration_times_out() {
        let mut superseded = process::spawn_group_leader("sh", &sh_args("sleep 30")).expect("failed to spawn N");
        let superseded_pid = superseded.id().expect("N has a pid") as i32;
        let mut link =
            FakeCandidateLink::with_all_steps(StepBehavior::Hang, StepBehavior::Succeed, StepBehavior::Succeed, StepBehavior::Succeed);

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &mut superseded,
            &mut link,
            &sample_snapshot(),
            6,
            timings(Duration::from_millis(30), SHORT_DEADLINE),
        )
        .await
        .expect_err("a hanging state-snapshot push must not promote");

        assert!(matches!(failure, PbaFailure::Timeout { stage: Stage::StateHydration }));
        assert!(proc_exists(superseded_pid), "generation N must stay untouched on a failed handoff");

        process::reap_process_group(&mut superseded, GRACE).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_and_leaves_generation_n_untouched_when_activate_draw_times_out() {
        let mut superseded = process::spawn_group_leader("sh", &sh_args("sleep 30")).expect("failed to spawn N");
        let superseded_pid = superseded.id().expect("N has a pid") as i32;
        let mut link =
            FakeCandidateLink::with_all_steps(StepBehavior::Succeed, StepBehavior::Succeed, StepBehavior::Hang, StepBehavior::Succeed);

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &mut superseded,
            &mut link,
            &sample_snapshot(),
            7,
            timings(SHORT_DEADLINE, Duration::from_millis(30)),
        )
        .await
        .expect_err("a hanging activate-draw send must not promote");

        assert!(matches!(failure, PbaFailure::Timeout { stage: Stage::ActivateDraw }));
        assert!(proc_exists(superseded_pid), "generation N must stay untouched on a failed handoff");

        process::reap_process_group(&mut superseded, GRACE).await.expect("cleanup reap failed");
    }

    /// Polls for `path` to contain a parseable pid, up to `timeout`. Used below to recover the
    /// spawned Candidate's own pid on a failure path, where `run_pba` doesn't return the
    /// `Child` -- the Candidate writes its own `$$` to `path` before blocking, since it's
    /// `exec`'d into the shell's own pid (no fork in between) and thus identical to
    /// `Child::id()`.
    async fn wait_for_pidfile(path: &std::path::Path, timeout: Duration) -> Option<i32> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Ok(contents) = std::fs::read_to_string(path)
                && let Ok(pid) = contents.trim().parse()
            {
                return Some(pid);
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_process_group_on_a_ready_timeout() {
        let mut superseded = process::spawn_group_leader("true", &[]).expect("failed to spawn N");
        let mut link = FakeCandidateLink::new(StepBehavior::Hang, StepBehavior::Succeed);

        // A pid-per-test-run file rather than matching on the shared "sleep 30" command line --
        // `cargo test` runs these tests in parallel, and several of them spawn that exact
        // command, so a name-based check (e.g. `pgrep -f`) would false-positive on a sibling
        // test's own live child.
        let unique = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("system clock").as_nanos();
        let pidfile = std::env::temp_dir().join(format!("oblisk-reload-test-{}-{unique}.pid", std::process::id()));

        run_pba(
            "sh",
            &sh_args(&format!("echo $$ > {}; exec sleep 30", pidfile.display())),
            &mut superseded,
            &mut link,
            &sample_snapshot(),
            3,
            timings(Duration::from_millis(30), SHORT_DEADLINE),
        )
        .await
        .expect_err("a hanging ready signal must not promote");

        let candidate_pid = wait_for_pidfile(&pidfile, Duration::from_millis(300))
            .await
            .expect("candidate should have written its own pid before hanging on the ready signal");
        let _ = std::fs::remove_file(&pidfile);

        let gone = wait_until(Duration::from_millis(500), || !proc_exists(candidate_pid)).await;
        assert!(gone, "the aborted candidate (pid {candidate_pid}) must have been reaped, not leaked");
    }

    #[tokio::test]
    async fn run_pba_aborts_the_candidate_and_leaves_generation_n_untouched_on_a_link_error() {
        let mut superseded = process::spawn_group_leader("sh", &sh_args("sleep 30")).expect("failed to spawn N");
        let superseded_pid = superseded.id().expect("N has a pid") as i32;
        let mut link = FakeCandidateLink::new(StepBehavior::Fail, StepBehavior::Succeed);

        let failure = run_pba(
            "sh",
            &sh_args("sleep 30"),
            &mut superseded,
            &mut link,
            &sample_snapshot(),
            4,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect_err("a link error must not promote");

        match failure {
            PbaFailure::Link { stage: Stage::NullBufferStaging, source } => {
                assert_eq!(source, FakeLinkError("candidate link failed".to_string()))
            }
            other => panic!("expected a NullBufferStaging link error, got {other:?}"),
        }
        assert!(proc_exists(superseded_pid), "generation N must stay untouched on a failed handoff");

        process::reap_process_group(&mut superseded, GRACE).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn run_pba_reports_spawn_failure_without_touching_generation_n_at_all() {
        let mut superseded = process::spawn_group_leader("sh", &sh_args("sleep 30")).expect("failed to spawn N");
        let superseded_pid = superseded.id().expect("N has a pid") as i32;
        let mut link = FakeCandidateLink::new(StepBehavior::Succeed, StepBehavior::Succeed);

        let failure = run_pba(
            "/no/such/binary-oblisk-reload-test",
            &[],
            &mut superseded,
            &mut link,
            &sample_snapshot(),
            5,
            timings(SHORT_DEADLINE, SHORT_DEADLINE),
        )
        .await
        .expect_err("spawning a nonexistent binary must fail");

        assert!(matches!(failure, PbaFailure::SpawnFailed(_)));
        assert!(link.calls.lock().unwrap().is_empty(), "no handshake call should happen if the spawn itself failed");
        assert!(proc_exists(superseded_pid), "generation N must stay untouched when the candidate never spawned");

        process::reap_process_group(&mut superseded, GRACE).await.expect("cleanup reap failed");
    }

    #[tokio::test]
    async fn wait_until_helper_reports_false_on_a_condition_that_never_becomes_true() {
        // Self-check for the polling helper above, mirroring process::mod's own test.
        let became_true = wait_until(Duration::from_millis(30), || false).await;
        assert!(!became_true);
    }
}
