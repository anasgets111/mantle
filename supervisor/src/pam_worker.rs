//! Real PAM conversation, closing ADR-0015. ADR-0028's halves share `shared::PamMessage`/
//! `PamOutcome` over `shared::framing` and [`pam_service`]. Blocking `nonstick` FFI runs in a
//! re-exec'd worker, not the async Supervisor. [`run_worker`] handles `MANTLE_PAM_WORKER=1`,
//! relaying masked prompts over stdio and answering with a `Response`, echo-on refused (ADR-0241).
//! [`run_authentication`] re-execs via [`crate::process::spawn_group_leader_stdio_piped`],
//! exchanges piped stdin/stdout, and reports to `main.rs`, the only unlock authority (ADR-0052).
//! [`run_polkit_helper`] uses polkit's root helper instead: polkitd accepts the agent response only
//! from uid 0 (ADR-0114). It does not reuse `RendererFrame`/`SupervisorFrame`, which cross a
//! different boundary.

use std::cell::RefCell;
use std::time::Duration;

use nonstick::{ConversationAdapter, Transaction};
use shared::{debug, error, info, warn};
use tokio::sync::mpsc::UnboundedSender;

/// Admin PAM service-stack directory. A constant lets [`pam_service_in`] tests use a temporary
/// directory.
const PAM_CONFIG_DIR: &str = "/etc/pam.d";

/// PAM 1.7+ vendor stack directory, where a package (not an admin) installs a stack.
const PAM_VENDOR_DIR: &str = "/usr/lib/pam.d";

/// Service name for Mantle's installed stack (`packaging/pam.d/mantle`).
const MANTLE_SERVICE: &str = "mantle";

/// [`run_conversation`] fallback when `packaging/pam.d/mantle` is absent. This system lacks
/// `/etc/pam.d/polkit-1`, so `"login"` is the disclosed fallback (ADR-0028).
const FALLBACK_SERVICE: &str = "login";

/// Uses Mantle's stack when installed, otherwise the console-login stack. Probing avoids PAM's
/// `/etc/pam.d/other` fallback, `pam_deny` on stock Arch: hardcoding `mantle` would turn a missing
/// package file into a lock screen rejecting every correct password, while failing closed locks
/// out the user. `stat` runs once per authentication, deliberately uncached so installing the file
/// takes effect without restarting the locked shell. Checks both dirs libpam itself would.
fn pam_service_in(pam_config_dir: &std::path::Path, pam_vendor_dir: &std::path::Path) -> &'static str {
    if pam_config_dir.join(MANTLE_SERVICE).exists() || pam_vendor_dir.join(MANTLE_SERVICE).exists() {
        MANTLE_SERVICE
    } else {
        FALLBACK_SERVICE
    }
}

fn pam_service() -> &'static str {
    pam_service_in(std::path::Path::new(PAM_CONFIG_DIR), std::path::Path::new(PAM_VENDOR_DIR))
}

/// Ceiling on the whole worker exchange (`exchange_over`), not one PAM call. Long because PAM may be
/// human-paced (network module, fingerprint retry), but bounds a wedged `read_json_frame` and
/// prevents a spawned task holding plaintext
/// forever while the prompt remains `authenticating`.
const PAM_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Relays a masked PAM prompt to the Supervisor and blocks for its answer (ADR-0241): `masked_prompt`
/// forwards its text rather than replaying a password captured up front. Blocking `std::io`, not
/// `shared::framing`: `nonstick` calls these synchronously from FFI, with no async context to
/// `.await` in. The received `secret` moves straight into PAM's `char*` copy, which is why nothing
/// here needs zeroizing on drop; there is no buffer left to scrub once each round returns.
struct RelayConversation<R, W> {
    reader: RefCell<R>,
    writer: RefCell<W>,
}

impl<R: std::io::Read, W: std::io::Write> RelayConversation<R, W> {
    fn relay(&self, text: String) -> nonstick::Result<std::ffi::OsString> {
        use std::os::unix::ffi::OsStringExt;
        write_frame(&mut *self.writer.borrow_mut(), &shared::PamMessage::Prompt { text, echo: false })
            .map_err(|_| nonstick::ErrorCode::ConversationError)?;
        match read_frame(&mut *self.reader.borrow_mut()).map_err(|_| nonstick::ErrorCode::ConversationError)? {
            shared::PamMessage::Response { secret } => Ok(std::ffi::OsString::from_vec(secret)),
            _ => Err(nonstick::ErrorCode::ConversationError),
        }
    }
}

impl<R: std::io::Read, W: std::io::Write> ConversationAdapter for RelayConversation<R, W> {
    /// Echo-on has no safe answer: the only secret held here is the lock password, and PAM treats
    /// an echo-on answer as displayable/loggable. Refusing, not relaying, keeps that password out
    /// of a channel PAM considers non-secret.
    fn prompt(&self, _request: impl AsRef<std::ffi::OsStr>) -> nonstick::Result<std::ffi::OsString> {
        Err(nonstick::ErrorCode::ConversationError)
    }

    fn masked_prompt(&self, request: impl AsRef<std::ffi::OsStr>) -> nonstick::Result<std::ffi::OsString> {
        self.relay(request.as_ref().to_string_lossy().into_owned())
    }

    fn error_msg(&self, message: impl AsRef<std::ffi::OsStr>) {
        warn!("{}", message.as_ref().to_string_lossy());
    }

    fn info_msg(&self, message: impl AsRef<std::ffi::OsStr>) {
        info!("{}", message.as_ref().to_string_lossy());
    }
}

/// Blocking mirror of `shared::framing`'s wire format: nonstick's conversation callbacks are
/// synchronous FFI, so a worker round trip cannot `.await`.
fn write_frame(mut writer: impl std::io::Write, message: &shared::PamMessage) -> std::io::Result<()> {
    let payload = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    if payload.len() > shared::framing::MAX_FRAME_LEN {
        return Err(std::io::Error::other(format!("frame length {} exceeds the limit", payload.len())));
    }
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()
}

/// [`write_frame`]'s read half. The payload buffer is `Zeroizing`, matching
/// `shared::framing::read_frame`: only a `Response` ever carries a secret, but every frame is
/// scrubbed alike rather than teaching this function which ones do.
fn read_frame(mut reader: impl std::io::Read) -> std::io::Result<shared::PamMessage> {
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > shared::framing::MAX_FRAME_LEN {
        return Err(std::io::Error::other(format!("frame length {len} exceeds the limit")));
    }
    let mut payload = shared::Zeroizing::new(vec![0u8; len]);
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(std::io::Error::other)
}

/// Maps a `nonstick` failure to the [`shared::PamOutcome`] the spawn side handles. Pure and
/// directly testable without PAM.
fn outcome_for_error(err: nonstick::ErrorCode) -> shared::PamOutcome {
    use nonstick::ErrorCode::*;
    match err {
        MaxTries => shared::PamOutcome::MaxTries,
        AuthenticationError | PermissionDenied | UserUnknown | CredentialsInsufficient | CredentialsExpired => {
            shared::PamOutcome::AuthFailed
        }
        other => shared::PamOutcome::PamError(format!("{other:?}")),
    }
}

/// Runs `pam_start` via `TransactionBuilder`, then `authenticate` and `account_management`,
/// relaying masked prompts over stdio (ADR-0241).
fn run_conversation(username: &str) -> shared::PamOutcome {
    let conversation = RelayConversation {
        reader: RefCell::new(std::io::stdin().lock()),
        writer: RefCell::new(std::io::stdout().lock()),
    };
    match nonstick::TransactionBuilder::new_with_service(pam_service())
        .username(username)
        .build(conversation.into_conversation())
    {
        Ok(mut txn) => {
            if let Err(err) = txn.authenticate(nonstick::AuthnFlags::empty()) {
                outcome_for_error(err)
            } else if let Err(err) = txn.account_management(nonstick::AuthnFlags::empty()) {
                outcome_for_error(err)
            } else {
                shared::PamOutcome::Success
            }
        }
        Err(err) => shared::PamOutcome::StartFailed(format!("{err:?}")),
    }
}

/// ADR-0028/ADR-0241 worker path for `MANTLE_PAM_WORKER=1`. `main.rs` enters it before
/// D-Bus/runtime/audio setup. Every `stdin`/`stdout` round trip PAM asks for happens inside
/// `run_conversation`; this only writes the frame that ends it. Plain blocking I/O throughout, so
/// no runtime is spun up here.
pub fn run_worker() -> Result<(), Box<dyn std::error::Error>> {
    let username = std::env::var("MANTLE_PAM_USERNAME")?;
    let outcome = run_conversation(&username);
    write_frame(std::io::stdout().lock(), &shared::PamMessage::Outcome(outcome))?;
    Ok(())
}

/// Resolves `uid` and runs the worker round trip. Borrows, but never zeroizes, `secret`;
/// [`run_authentication`] owns scrubbing on every path.
///
/// ponytail: `User::from_uid` blocks in libc (`getpwuid_r`), but stays inline because local passwd
/// lookup is fast, has no NSS/LDAP, and is rare (one challenge/lock submission). Upgrade:
/// `spawn_blocking` if a networked NSS backend appears.
async fn authenticate_uid(uid: u32, secret: &[u8]) -> Result<shared::PamOutcome, String> {
    let username = username_for(uid)?;
    spawn_worker_and_exchange(&username, secret).await.map_err(|err| format!("pam worker failed: {err}"))
}

fn username_for(uid: u32) -> Result<String, String> {
    match nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid)) {
        Ok(Some(user)) => Ok(user.name),
        Ok(None) => Err(format!("uid {uid} has no passwd entry")),
        Err(err) => Err(format!("failed to resolve uid {uid}: {err}")),
    }
}

/// systemd starts polkit's helper as root for each connection. It runs PAM and invokes
/// `AuthenticationAgentResponse3`, which polkitd accepts only from uid 0, so [`run_worker`] cannot.
/// It reads our uid and pid from the socket.
const POLKIT_HELPER_SOCKET: &str = "/run/polkit/agent-helper.socket";

/// [`run_authentication`]'s polkit sibling (ADR-0114): same report and `Drop` backstop, but the
/// conversation is the helper's behind [`POLKIT_HELPER_SOCKET`]. `Success` means polkitd was told;
/// the caller answers the held `BeginAuthentication`.
pub async fn run_polkit_helper(
    uid: u32,
    cookie: String,
    mut secret: shared::Zeroizing<Vec<u8>>,
    outcome_tx: UnboundedSender<(String, shared::PamOutcome)>,
) {
    let mut guard = ReportOnDrop { pending: Some((cookie.clone(), outcome_tx)) };
    let outcome = match authenticate_via_helper(uid, &cookie, &secret).await {
        Ok(outcome) => outcome,
        Err(reason) => shared::PamOutcome::StartFailed(reason),
    };
    shared::Zeroize::zeroize(&mut *secret);
    guard.report(outcome, "polkit");
}

async fn authenticate_via_helper(uid: u32, cookie: &str, secret: &[u8]) -> Result<shared::PamOutcome, String> {
    let username = username_for(uid)?;
    let (reader, writer) = tokio::net::UnixStream::connect(POLKIT_HELPER_SOCKET)
        .await
        .map_err(|err| format!("could not connect to {POLKIT_HELPER_SOCKET}: {err}"))?
        .into_split();
    match tokio::time::timeout(PAM_EXCHANGE_TIMEOUT, drive_helper(reader, writer, &username, cookie, secret)).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(err)) => Err(format!("polkit helper exchange failed: {err}")),
        Err(_elapsed) => Err("the polkit helper did not respond within the timeout".to_string()),
    }
}

/// libpolkit-agent's `polkitagentsession.c` protocol: username and cookie lines first; answer every
/// `PAM_PROMPT_ECHO_OFF`/`PAM_PROMPT_ECHO_ON` with the one password (ADR-0028); log
/// `PAM_ERROR_MSG`/`PAM_TEXT_INFO`; `SUCCESS`/`FAILURE` end it. Verified against polkit 127.
async fn drive_helper(
    reader: impl tokio::io::AsyncRead + Unpin,
    mut writer: impl tokio::io::AsyncWrite + Unpin,
    username: &str,
    cookie: &str,
    secret: &[u8],
) -> std::io::Result<shared::PamOutcome> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    for line in [username, cookie] {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
    }
    let mut lines = tokio::io::BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        if line.starts_with("PAM_PROMPT_ECHO_OFF") || line.starts_with("PAM_PROMPT_ECHO_ON") {
            writer.write_all(secret).await?;
            writer.write_all(b"\n").await?;
        } else if line == "SUCCESS" {
            return Ok(shared::PamOutcome::Success);
        } else if line == "FAILURE" {
            return Ok(shared::PamOutcome::AuthFailed);
        } else {
            debug!("polkit helper: {line}");
        }
    }
    Err(std::io::Error::other("the helper closed without a verdict"))
}

/// One worker round trip for `uid`; send the outcome on `outcome_tx` tagged by the lock acquisition
/// number or polkit cookie, so the receiver matches answer to request (`lock::accepts_outcome` and
/// the polkit pending cookie). Both callers spawn it: Enter is unbounded and common, and
/// `pam_unix`'s failure delay would stall every frame if awaited inline.
///
/// `secret` is zeroized even when the future is dropped during shutdown or unwinds on panic, via
/// `Zeroizing::Drop` (ADR-0005). The wrapper is an argument because spawned arguments are captured
/// before the body is polled. Failure before PAM is `StartFailed`, matching `pam_start` failure.
///
/// [`ReportOnDrop`] sends something on every task exit. The caller sets `authenticating` and only
/// an outcome clears it; missing a send would create a one-way latch, leaving the lock screen
/// stuck until a VT switch. `UnboundedSender::send` is synchronous, so `Drop::drop` can send while
/// unwinding.
pub async fn run_authentication<T: Send + 'static>(
    uid: u32,
    mut secret: shared::Zeroizing<Vec<u8>>,
    tag: T,
    outcome_tx: UnboundedSender<(T, shared::PamOutcome)>,
) {
    let mut guard = ReportOnDrop { pending: Some((tag, outcome_tx)) };
    let outcome = match authenticate_uid(uid, &secret).await {
        Ok(outcome) => outcome,
        Err(reason) => shared::PamOutcome::StartFailed(reason),
    };
    shared::Zeroize::zeroize(&mut *secret);
    guard.report(outcome, "pam");
}

/// [`run_authentication`]'s `Drop` backstop: reports once if dropped before ordinary `.take()`.
struct ReportOnDrop<T> {
    pending: Option<(T, UnboundedSender<(T, shared::PamOutcome)>)>,
}

impl<T> ReportOnDrop<T> {
    /// Sends the one outcome this authentication owes, or logs that nobody is left to hear it.
    /// `label` names the caller because the two paths log under their own names.
    fn report(&mut self, outcome: shared::PamOutcome, label: &str) {
        if let Some((tag, tx)) = self.pending.take()
            && tx.send((tag, outcome)).is_err()
        {
            // A closed channel means `main.rs`'s loop is gone, which `LockController::send` permits.
            warn!("{label}: the outcome channel is closed; dropping an authentication result");
        }
    }
}

impl<T> Drop for ReportOnDrop<T> {
    fn drop(&mut self) {
        if let Some((tag, tx)) = self.pending.take() {
            // Reached by a panic or a dropped future, not by any ordinary `.report()` call: the
            // security decision this authentication owed was lost, not just delayed.
            error!("an authentication task ended without ever reporting a PAM outcome");
            // Any `PamOutcome` clears `authenticating`; `PamError` supplies the prompt's error
            // text when no real PAM answer exists.
            let outcome =
                shared::PamOutcome::PamError("pam authentication task ended without reporting an outcome".to_string());
            let _ = tx.send((tag, outcome));
        }
    }
}

/// Path this process re-execs to reach [`crate::pam_worker`]'s worker branch.
///
/// The magic link, not `current_exe()`, and the difference is a session that cannot be unlocked.
/// `current_exe()` *reads* the link into a pathname, and the kernel appends " (deleted)" once the
/// binary is replaced, so the spawn fails with `ENOENT`. Executing the link resolves to the inode
/// this process already pins, which Linux supports after unlinking. A `pacman -Syu` over a locked
/// session used to strand it behind "could not start authentication"; seen twice here, once with
/// the session locked, from `cargo build` doing the same thing to the same inode.
///
/// Not `renderer_binary_path()`'s problem: that one calls `with_file_name`, which drops the whole
/// " (deleted)" filename and rebuilds a real sibling path.
pub(crate) const SELF_EXE: &str = "/proc/self/exe";

/// Re-execs this binary as a PAM worker for `username`, then calls [`exchange_over`].
async fn spawn_worker_and_exchange(username: &str, secret: &[u8]) -> std::io::Result<shared::PamOutcome> {
    let child = crate::process::spawn_group_leader_stdio_piped(
        SELF_EXE,
        &[],
        &[
            ("MANTLE_PAM_WORKER".to_string(), "1".to_string()),
            ("MANTLE_PAM_USERNAME".to_string(), username.to_string()),
        ],
    )?;
    exchange_over(child, secret, PAM_EXCHANGE_TIMEOUT).await
}

/// Reads Prompt/Outcome frames from stdout and answers each Prompt with `secret` on stdin
/// ([`exchange_messages`]), then reaps the process group (ADR-0241). Stdin/stdout are taken out of
/// `child` before the timeout so `exchange_messages` is testable over a plain duplex pipe. Reap
/// always runs, including after failed/timed-out I/O, so a hung worker is not untracked. `timeout`
/// covers only the message loop; `reap_process_group` has its own grace. Without it, a wedged
/// prompt could hang forever and the inline polkit path would stall `main.rs`'s `select!`.
/// Parameterize it so tests avoid the real 30-second [`PAM_EXCHANGE_TIMEOUT`]. A failed exchange
/// carries [`post_mortem`]'s account of how the worker died, because the reap already knows and the
/// lock screen otherwise reports an I/O error with no subject.
async fn exchange_over(
    mut child: tokio::process::Child,
    secret: &[u8],
    timeout: Duration,
) -> std::io::Result<shared::PamOutcome> {
    let stdin = child.stdin.take().expect("spawn_group_leader_stdio_piped always pipes stdin");
    let stdout = child.stdout.take().expect("spawn_group_leader_stdio_piped always pipes stdout");
    let outcome_result = match tokio::time::timeout(timeout, exchange_messages(stdout, stdin, secret)).await {
        Ok(result) => result,
        Err(_elapsed) => {
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "pam worker did not respond within the timeout"))
        }
    };

    let reaped = crate::process::reap_process_group(&mut child, crate::process::DEFAULT_REAP_GRACE).await;
    if let Err(err) = &reaped {
        warn!("failed to reap pam worker: {err}");
    }

    // A worker that answered needs no post-mortem. One that did not leaves the lock screen saying
    // only "early eof", and on 2026-09-08 that was the whole of the evidence: nothing on the
    // worker's inherited stderr, no coredump, nothing in the journal, and a session that could not
    // be unlocked until the next attempt happened to work. How it died is already in hand here and
    // was being dropped on the floor: log it, not just the on-screen error text.
    outcome_result.map_err(|err| {
        let post_mortem = post_mortem(&reaped);
        error!("pam worker exchange failed: {err}; the worker {post_mortem}");
        std::io::Error::new(err.kind(), format!("{err}; the worker {post_mortem}"))
    })
}

/// How the worker died, for an [`exchange_over`] that never got a frame.
///
/// `reap_process_group` sends `SIGTERM` before waiting, so a live worker dies by our hand and
/// reports signal 15. That is not the ambiguity it looks like: this runs only when the pipe already
/// closed, which a live process does not do, so in practice the status is the worker's own.
fn post_mortem(reaped: &std::io::Result<crate::process::ReapOutcome>) -> String {
    use std::os::unix::process::ExitStatusExt;

    let status = match reaped {
        Ok(crate::process::ReapOutcome::ExitedCleanly(status) | crate::process::ReapOutcome::Escalated(status)) => {
            status
        }
        Err(err) => return format!("could not be reaped, so how it died is unknown: {err}"),
    };
    if let Some(signal) = status.signal() {
        return format!("was killed by signal {signal}");
    }
    match status.code() {
        // The worker prints its own reason to the inherited stderr before returning non-zero, so a
        // code here is a pointer to that line rather than the whole story.
        Some(code) => format!("exited with status {code}"),
        None => format!("ended in a way no exit status describes: {status}"),
    }
}

/// Answers every `Prompt` with `secret` until `Outcome` ends the conversation (ADR-0241): the
/// lock's only caller today has one password, so every prompt -- echo-on or off -- gets the same
/// answer. A `Response` arriving from the worker is a protocol violation, not a valid frame.
async fn exchange_messages(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    mut writer: impl tokio::io::AsyncWrite + Unpin,
    secret: &[u8],
) -> std::io::Result<shared::PamOutcome> {
    // One clone for the whole exchange, not one per round, reused across every prompt and
    // zeroized once the loop ends.
    //
    // ponytail: left unzeroized if `exchange_over`'s timeout cancels this future mid-`.await`.
    // Bounded by that timeout and by the worker's own process lifetime; upgrade by giving
    // `response` a drop guard if that gap needs closing too.
    let mut response = shared::PamMessage::Response { secret: secret.to_vec() };
    let result = loop {
        let message = match shared::framing::read_json_frame(&mut reader).await {
            Ok(message) => message,
            Err(err) => break Err(std::io::Error::other(err)),
        };
        match message {
            shared::PamMessage::Outcome(outcome) => break Ok(outcome),
            shared::PamMessage::Prompt { .. } => {
                if let Err(err) = shared::framing::write_json_frame(&mut writer, &response).await {
                    break Err(std::io::Error::other(err));
                }
            }
            shared::PamMessage::Response { .. } => {
                break Err(std::io::Error::other("the worker sent a Response, which is the Supervisor's to send"));
            }
        }
    };
    shared::Zeroize::zeroize(&mut response);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- SELF_EXE (ADR-0161) ----

    /// The bug this constant exists for, reproduced without PAM: replace a running executable and
    /// then spawn it again. Asserting the string would prove nothing, because `current_exe()`
    /// returns the right path until the moment the inode is replaced.
    ///
    /// Uses `cp` as a stand-in for any binary: copy it, run the copy, unlink the copy, and check
    /// that both routes to "run myself again" still work from inside that process. `current_exe()`
    /// is what the shell used and what stranded a locked session.
    #[tokio::test]
    async fn a_replaced_binary_is_still_reachable_through_the_magic_link() {
        let dir = tempfile::tempdir().unwrap();
        let copy = dir.path().join("stand-in");
        std::fs::copy("/bin/cp", &copy).expect("a binary to stand in for this one");

        // Hold it open the way a running process holds its own image, then unlink it.
        let running = std::fs::File::open(&copy).unwrap();
        std::fs::remove_file(&copy).unwrap();

        let read_back = std::fs::read_link(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&running)))
            .expect("procfs names the open file");
        assert!(
            read_back.to_string_lossy().ends_with(" (deleted)"),
            "the kernel marks a replaced binary's pathname, which is what made the spawn fail: {read_back:?}"
        );
        assert!(
            !std::path::Path::new(&read_back).exists(),
            "so the pathname `current_exe()` hands back cannot be spawned"
        );
        assert_eq!(SELF_EXE, "/proc/self/exe", "and the link itself is what stays executable");
    }

    /// The child half of [`a_process_whose_binary_was_unlinked_can_still_spawn_itself`]. Inert
    /// unless that test asks for it by name and sets the variable, so the ordinary suite skips it
    /// rather than re-execing itself.
    ///
    /// Once stdin closes, the parent has unlinked the path this process was started from, so
    /// `/proc/self/exe` here reads `... (deleted)`. That is the state a `pacman -Syu` leaves a
    /// running shell in, and spawning through the link has to work anyway.
    #[test]
    fn self_exe_probe_child() {
        if std::env::var_os("MANTLE_SELF_EXE_CHILD").is_none() {
            return;
        }
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new()).expect("stdin closes after the unlink");
        let own = std::fs::read_link("/proc/self/exe").expect("procfs names this process's binary");
        assert!(
            own.to_string_lossy().ends_with(" (deleted)"),
            "the parent must have unlinked us first, or this proves nothing: {own:?}"
        );

        let ran = std::process::Command::new(SELF_EXE)
            .args(["--exact", "a_name_no_test_here_has"])
            .output()
            .expect("a process whose binary was unlinked must still reach itself through the link");
        assert!(ran.status.success(), "and the re-exec must run");
    }

    /// The bug end to end: unlink a running process's binary, then have it spawn itself the way
    /// `spawn_worker_and_exchange` does. Before [`SELF_EXE`] this was `current_exe()`, which hands
    /// back a pathname with " (deleted)" on it, and the spawn failed with `ENOENT` while a locked
    /// session waited on it.
    ///
    /// Hard-links rather than copies: same inode, no 250MB of I/O, and unlinking the new name is
    /// what marks the child's `/proc/self/exe`.
    #[tokio::test]
    async fn a_process_whose_binary_was_unlinked_can_still_spawn_itself() {
        let exe = std::env::current_exe().expect("the test binary");
        let dir = tempfile::tempdir_in(exe.parent().expect("it lives somewhere")).expect("a dir beside it");
        let stand_in = dir.path().join("stand-in");
        std::fs::hard_link(&exe, &stand_in).expect("the same filesystem, so a link rather than a copy");

        let child = tokio::process::Command::new(&stand_in)
            .args(["--exact", "pam_worker::tests::self_exe_probe_child", "--nocapture"])
            .env("MANTLE_SELF_EXE_CHILD", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("the linked binary runs");

        // Unlink while it runs, which is what an upgrade does.
        std::fs::remove_file(&stand_in).expect("the name goes, the inode stays");

        let done = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output())
            .await
            .expect("the child must not hang")
            .expect("and must report");
        let out = format!("{}{}", String::from_utf8_lossy(&done.stdout), String::from_utf8_lossy(&done.stderr));
        assert!(done.status.success(), "a process whose binary was unlinked must still spawn itself: {out}");
        // A filter that matches nothing also exits zero, so the pass has to be a test that ran.
        assert!(out.contains("1 passed"), "the child must have actually run the probe: {out}");
    }

    /// The other half: this process can spawn itself through [`SELF_EXE`] at all, using the
    /// production helper. Selects no test by name, because the helper pipes stdio and a child that
    /// fills its stdout pipe with output nobody reads deadlocks. `wait_with_output` drains it.
    #[tokio::test]
    async fn the_magic_link_spawns_this_test_binary_again() {
        let child = crate::process::spawn_group_leader_stdio_piped(
            SELF_EXE,
            &["--exact".to_string(), "a_name_no_test_here_has".to_string()],
            &[("MANTLE_SELF_EXE_PROBE".to_string(), "1".to_string())],
        );
        let child = child.expect("spawning /proc/self/exe must work for a live process");
        let done = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
            .await
            .expect("the re-exec must not hang")
            .expect("and must report a status");

        assert!(done.status.success(), "the re-exec ran and selected no test");
        assert!(
            String::from_utf8_lossy(&done.stdout).contains("0 passed"),
            "it was this test binary that ran again, not something else: {}",
            String::from_utf8_lossy(&done.stdout)
        );
    }

    // ---- pam_service_in ----

    /// Without a stack, keep the console-login service. Naming `mantle` would select
    /// `/etc/pam.d/other`, `pam_deny` on stock Arch, and reject the correct password.
    #[test]
    fn without_an_installed_stack_the_service_falls_back_to_login() {
        let admin = tempfile::tempdir().unwrap();
        let vendor = tempfile::tempdir().unwrap();
        assert_eq!(pam_service_in(admin.path(), vendor.path()), "login");
    }

    #[test]
    fn an_installed_stack_is_preferred_over_the_console_login_one() {
        let admin = tempfile::tempdir().unwrap();
        let vendor = tempfile::tempdir().unwrap();
        std::fs::write(admin.path().join("mantle"), "auth include system-auth\n").unwrap();
        assert_eq!(pam_service_in(admin.path(), vendor.path()), "mantle");
    }

    #[test]
    fn a_stack_installed_to_the_vendor_directory_is_found_without_an_admin_override() {
        let admin = tempfile::tempdir().unwrap();
        let vendor = tempfile::tempdir().unwrap();
        std::fs::write(vendor.path().join("mantle"), "auth include system-auth\n").unwrap();
        assert_eq!(pam_service_in(admin.path(), vendor.path()), "mantle");
    }

    /// An unreadable PAM directory is "not installed", not a panic; this runs in the unlock worker.
    #[test]
    fn a_missing_pam_config_directory_falls_back_rather_than_failing() {
        let no_such = std::path::Path::new("/no/such/pam.d");
        assert_eq!(pam_service_in(no_such, no_such), "login");
    }

    /// The shipped file is what the probe finds and must carry both chains: `run_conversation`
    /// calls `authenticate`, then `account_management`; without `account`, the second fails after
    /// password acceptance.
    #[test]
    fn the_shipped_pam_stack_declares_both_chains_the_worker_drives() {
        let shipped =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../packaging/pam.d/mantle")).unwrap();
        let directives: Vec<&str> =
            shipped.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')).collect();
        assert!(directives.iter().any(|line| line.starts_with("auth")), "no auth chain in {directives:?}");
        assert!(directives.iter().any(|line| line.starts_with("account")), "no account chain in {directives:?}");
        assert!(
            directives.iter().all(|line| line.starts_with("auth") || line.starts_with("account")),
            "the worker opens no session and changes no password, so anything else is dead config: {directives:?}"
        );
    }

    /// Test timeout for ordinary paths: short enough to bound a hung test, long enough for local
    /// `sh -c` fakes. Deliberately distinct from [`PAM_EXCHANGE_TIMEOUT`].
    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    #[test]
    fn outcome_for_error_maps_max_tries() {
        assert_eq!(outcome_for_error(nonstick::ErrorCode::MaxTries), shared::PamOutcome::MaxTries);
    }

    #[test]
    fn outcome_for_error_maps_authentication_error_to_auth_failed() {
        assert_eq!(outcome_for_error(nonstick::ErrorCode::AuthenticationError), shared::PamOutcome::AuthFailed);
    }

    #[test]
    fn outcome_for_error_maps_an_arbitrary_other_variant_to_pam_error() {
        assert_eq!(
            outcome_for_error(nonstick::ErrorCode::SystemError),
            shared::PamOutcome::PamError("SystemError".to_string())
        );
    }

    // Real libpam re-exec needs a real PAM stack; unit-test the wire protocol with a fake shell
    // worker instead.

    #[tokio::test]
    async fn exchange_over_reads_back_the_worker_s_final_outcome_frame() {
        // `shared::framing`: 4-byte big-endian length, then JSON. `{"Outcome":"Success"}` is 21
        // bytes. No `Prompt` is sent, so nothing is written back; the script need not drain stdin.
        let script = r#"printf '\000\000\000\025{"Outcome":"Success"}'"#;
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
            .expect("failed to spawn the fake worker");

        let outcome = exchange_over(child, b"the-password", TEST_TIMEOUT).await.expect("exchange_over failed");

        assert_eq!(outcome, shared::PamOutcome::Success);
    }

    #[tokio::test]
    async fn exchange_over_still_reaps_the_worker_when_the_frame_read_fails() {
        // Malformed frame, then a hang. Read must fail and the process must still be reaped.
        let script = r#"printf '\000\000\000\004evil'; sleep 30"#;
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
            .expect("failed to spawn the fake worker");
        let pid = child.id().expect("freshly spawned child has a pid");

        let result = exchange_over(child, b"the-password", TEST_TIMEOUT).await;

        assert!(result.is_err(), "an undecodable frame must surface as an error, not a silent outcome");

        assert!(
            crate::process::exited(&[pid]).await,
            "the worker (pid {pid}) should be reaped even though exchange_over returned an error, not left sleeping"
        );
    }

    /// The 2026-09-08 lockout: the worker vanished and "early eof" was the entire diagnosis.
    #[tokio::test]
    async fn a_worker_that_dies_without_answering_reports_how_it_died() {
        for (script, expected) in [("exit 3", "exited with status 3"), ("kill -SEGV $$", "was killed by signal 11")] {
            let child =
                crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
                    .expect("failed to spawn the fake worker");

            let err = exchange_over(child, b"the-password", TEST_TIMEOUT)
                .await
                .expect_err("a worker that writes no frame must fail the exchange");

            let message = err.to_string();
            assert!(message.contains(expected), "`{script}` should report `{expected}`, got: {message}");
        }
    }

    /// The multi-round case ADR-0241 exists for: a module asking twice, once echo-off and once
    /// echo-on, gets the same secret both times, then `Outcome` ends it. An in-memory duplex
    /// stands in for the worker's piped stdio; `exchange_over`'s own tests already cover the real
    /// child/reap plumbing around this loop.
    #[tokio::test]
    async fn exchange_messages_answers_every_prompt_with_the_secret_until_outcome() {
        let (ours, theirs) = tokio::io::duplex(256);
        let (their_reader, their_writer) = tokio::io::split(theirs);
        let fake_worker = tokio::spawn(async move {
            let mut writer = their_writer;
            let mut reader = their_reader;
            let mut secrets_seen = Vec::new();
            for (text, echo) in [("Password:", false), ("One-time code:", true)] {
                shared::framing::write_json_frame(&mut writer, &shared::PamMessage::Prompt { text: text.into(), echo })
                    .await
                    .unwrap();
                match shared::framing::read_json_frame(&mut reader).await.unwrap() {
                    shared::PamMessage::Response { secret } => secrets_seen.push(secret),
                    other => panic!("expected a Response, got {other:?}"),
                }
            }
            shared::framing::write_json_frame(&mut writer, &shared::PamMessage::Outcome(shared::PamOutcome::Success))
                .await
                .unwrap();
            secrets_seen
        });

        let (my_reader, my_writer) = tokio::io::split(ours);
        let outcome = exchange_messages(my_reader, my_writer, b"hunter2").await.expect("exchange_messages failed");

        assert_eq!(outcome, shared::PamOutcome::Success);
        assert_eq!(fake_worker.await.unwrap(), [b"hunter2".to_vec(), b"hunter2".to_vec()]);
    }

    #[tokio::test]
    async fn exchange_over_times_out_and_still_reaps_a_worker_that_never_responds() {
        // A sleeping worker models a PAM module blocked on unreachable network auth. Return an
        // error within `timeout` and still reap it.
        let script = "sleep 30";
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &["-c".to_string(), script.to_string()], &[])
            .expect("failed to spawn the fake worker");
        let pid = child.id().expect("freshly spawned child has a pid");

        let started = tokio::time::Instant::now();
        let result = exchange_over(child, b"the-password", Duration::from_millis(50)).await;
        let elapsed = started.elapsed();

        assert!(result.is_err(), "a worker that never responds must surface as an error, not hang forever");
        assert!(elapsed < Duration::from_secs(5), "exchange_over took {elapsed:?} -- the timeout did not bound it");

        assert!(
            crate::process::exited(&[pid]).await,
            "the worker (pid {pid}) should be reaped even after a timeout, not left sleeping for the full 30s"
        );
    }

    #[tokio::test]
    async fn drive_helper_writes_the_opening_lines_in_order_then_answers_the_prompt() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let (ours, theirs) = tokio::io::duplex(256);
        let helper = tokio::spawn(async move {
            let (reader, mut writer) = tokio::io::split(theirs);
            let mut lines = tokio::io::BufReader::new(reader).lines();
            let mut seen = vec![lines.next_line().await.unwrap().unwrap(), lines.next_line().await.unwrap().unwrap()];
            writer.write_all(b"PAM_PROMPT_ECHO_OFF Password: \n").await.unwrap();
            seen.push(lines.next_line().await.unwrap().unwrap());
            writer.write_all(b"SUCCESS\n").await.unwrap();
            seen
        });

        let (reader, writer) = tokio::io::split(ours);
        let outcome = drive_helper(reader, writer, "alice", "cookie-1", b"hunter2").await.unwrap();

        assert_eq!(outcome, shared::PamOutcome::Success);
        assert_eq!(helper.await.unwrap(), ["alice", "cookie-1", "hunter2"]);
    }

    #[test]
    fn read_frame_zeroizes_whatever_it_already_read_before_a_mid_stream_error() {
        // A valid length prefix, then real payload bytes once, then failure -- reproducing a
        // mid-`read_exact` pipe error after the secret has partly landed in the buffer.
        struct FailsAfterTheLengthPrefix {
            length: [u8; 4],
            chunk: &'static [u8],
            step: u8,
        }
        impl std::io::Read for FailsAfterTheLengthPrefix {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.step += 1;
                match self.step {
                    1 => {
                        buf[..4].copy_from_slice(&self.length);
                        Ok(4)
                    }
                    2 => {
                        let n = self.chunk.len().min(buf.len());
                        buf[..n].copy_from_slice(&self.chunk[..n]);
                        Ok(n)
                    }
                    _ => Err(std::io::Error::other("simulated mid-read pipe failure")),
                }
            }
        }

        let err = read_frame(FailsAfterTheLengthPrefix { length: 8u32.to_be_bytes(), chunk: b"hunter2", step: 0 })
            .expect_err("a reader that errors mid-stream must surface that error, not silently truncate");

        assert_eq!(err.to_string(), "simulated mid-read pipe failure");
        // The partial buffer is already dropped here; proving live zeroization requires observing
        // it before drop, not afterward.
    }

    /// The worker's actual conversation driver, proven without PAM or a subprocess: more than one
    /// prompt, echo-off then echo-on, each answered from the wire rather than one password
    /// replayed (ADR-0241).
    #[test]
    fn relay_conversation_relays_a_masked_prompt_but_refuses_an_echo_on_one() {
        let mut canned = Vec::new();
        write_frame(&mut canned, &shared::PamMessage::Response { secret: b"hunter2".to_vec() }).unwrap();
        let conversation =
            RelayConversation { reader: RefCell::new(std::io::Cursor::new(canned)), writer: RefCell::new(Vec::new()) };

        assert_eq!(conversation.masked_prompt("Password:").unwrap(), std::ffi::OsString::from("hunter2"));
        assert!(conversation.prompt("One-time code:").is_err(), "an echo-on prompt has no safe answer to relay");

        let mut sent = std::io::Cursor::new(conversation.writer.into_inner());
        assert_eq!(
            read_frame(&mut sent).unwrap(),
            shared::PamMessage::Prompt { text: "Password:".into(), echo: false }
        );
        assert!(
            sent.position() as usize == sent.get_ref().len(),
            "refusing must not write a frame for the echo-on prompt"
        );
    }

    // Pin release when the spawned authentication task panics or drops before reporting.

    #[test]
    fn report_on_drop_sends_a_fallback_outcome_when_dropped_before_reporting() {
        // Simulates panic from `run_authentication` or future drop mid-`.await`, before its send.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        drop(ReportOnDrop { pending: Some((7u64, tx)) });

        let (acquisition, outcome) =
            rx.try_recv().expect("a fallback outcome must be sent when the guard is dropped before reporting");
        assert!(
            matches!(outcome, shared::PamOutcome::PamError(_)),
            "the fallback must be a PamOutcome so main.rs's pam_outcomes arm can still clear `authenticating`"
        );
        assert_eq!(
            acquisition, 7,
            "and it must be tagged with the lock it was started for, or lock::accepts_outcome cannot place it"
        );
    }

    #[test]
    fn report_on_drop_does_not_double_send_once_the_real_outcome_already_went_out() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut guard = ReportOnDrop { pending: Some((7u64, tx)) };
        let (tag, tx) = guard.pending.take().expect("freshly built guard holds a sender");
        tx.send((tag, shared::PamOutcome::Success)).expect("send failed");
        drop(guard);

        assert_eq!(rx.try_recv(), Ok((7, shared::PamOutcome::Success)));
        assert!(
            rx.try_recv().is_err(),
            "the drop guard must not also send its fallback once the real outcome already went out"
        );
    }

    #[tokio::test]
    async fn a_panicking_task_still_reports_a_fallback_outcome_via_the_drop_guard() {
        // `tokio::spawn` catches an FFI panic, but locals, including `ReportOnDrop`, still drop;
        // this pins that fallback send.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            let _guard = ReportOnDrop { pending: Some((7u64, tx)) };
            panic!("simulated panic before the task's own explicit send");
        });
        let join_result = handle.await;
        assert!(
            join_result.is_err(),
            "the task did panic -- that part of the simulation, not the fix, is what's asserted here"
        );

        let (_acquisition, outcome) = rx
            .try_recv()
            .expect("the drop guard must still report a fallback outcome even though the task panicked first");
        assert!(matches!(outcome, shared::PamOutcome::PamError(_)));
    }
}
