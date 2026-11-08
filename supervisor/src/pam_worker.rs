//! Real PAM conversation, closing ADR-0015. ADR-0028's halves share `shared::PamMessage`/
//! `PamOutcome` over `shared::framing` and [`pam_service`]. Blocking `nonstick` FFI runs in a
//! re-exec'd worker, not the async Supervisor. [`run_worker`] handles `MANTLE_PAM_WORKER=1`,
//! relaying every prompt, info and error line over stdio and answering each prompt with a
//! `Response` (ADR-0241, ADR-0381). [`start_lock_conversation`] re-execs it via
//! [`crate::process::spawn_group_leader_stdio_piped`] and reports each frame to `main.rs`, the
//! only unlock authority (ADR-0052). [`start_polkit_conversation`] uses polkit's root helper
//! instead: polkitd accepts the agent response only from uid 0 (ADR-0114). It does not reuse
//! `RendererFrame`/`SupervisorFrame`, which cross a different boundary.

use std::cell::RefCell;
use std::time::Duration;

use nonstick::{ConversationAdapter, Transaction};
use shared::{debug, error, warn};
use tokio::sync::mpsc::UnboundedSender;

use crate::process::SELF_EXE;

/// Set (to anything) in the re-exec'd worker; `main.rs` checks it before any other setup.
pub const WORKER_ENV: &str = "MANTLE_PAM_WORKER";

/// The account the worker authenticates.
const USERNAME_ENV: &str = "MANTLE_PAM_USERNAME";

/// Which conversation the worker runs: [`PASSWORD`] or [`FINGERPRINT`]. A mode, not a service
/// name, so the Supervisor never names a stack the worker would open.
const MODE_ENV: &str = "MANTLE_PAM_MODE";

/// The password conversation, on [`pam_service_in`]'s stack.
pub const PASSWORD: &str = "password";

/// The fingerprint conversation beside it (ADR-0382), on [`FINGERPRINT_SERVICE`] only.
pub const FINGERPRINT: &str = "fingerprint";

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

/// `packaging/pam.d/mantle-fingerprint`. No fallback: any other stack asks for a password.
const FINGERPRINT_SERVICE: &str = "mantle-fingerprint";

/// Uses Mantle's stack when installed, otherwise the console-login stack. Probing avoids PAM's
/// `/etc/pam.d/other` fallback, `pam_deny` on stock Arch: hardcoding `mantle` would turn a missing
/// package file into a lock screen rejecting every correct password, while failing closed locks
/// out the user. `stat` runs once per authentication, deliberately uncached so installing the file
/// takes effect without restarting the locked shell. Checks both dirs libpam itself would.
/// `None` for an unknown mode.
fn pam_service_in(mode: &str, admin: &std::path::Path, vendor: &std::path::Path) -> Option<&'static str> {
    match mode {
        PASSWORD if installed_in(MANTLE_SERVICE, admin, vendor) => Some(MANTLE_SERVICE),
        PASSWORD => Some(FALLBACK_SERVICE),
        FINGERPRINT => Some(FINGERPRINT_SERVICE),
        _ => None,
    }
}

fn installed_in(service: &str, admin: &std::path::Path, vendor: &std::path::Path) -> bool {
    admin.join(service).exists() || vendor.join(service).exists()
}

fn pam_service(mode: &str) -> Option<&'static str> {
    pam_service_in(mode, std::path::Path::new(PAM_CONFIG_DIR), std::path::Path::new(PAM_VENDOR_DIR))
}

/// Whether the lock may start a fingerprint conversation; uncached like [`pam_service_in`].
pub fn fingerprint_installed() -> bool {
    installed_in(FINGERPRINT_SERVICE, std::path::Path::new(PAM_CONFIG_DIR), std::path::Path::new(PAM_VENDOR_DIR))
}

/// Ceiling on PAM's reply once an answer is sent, until its next prompt or verdict. It never runs
/// while a prompt waits for the user, who may take minutes (ADR-0381); it bounds a wedged module
/// holding plaintext. A `Message` frame leaves it running: only a prompt hands the turn back.
const PAM_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Relays every PAM message to the Supervisor and blocks for the answer to each prompt (ADR-0241,
/// ADR-0381). Blocking `std::io`, not `shared::framing`: `nonstick` calls these synchronously from
/// FFI, with no async context to `.await` in. The received `secret` moves without a copy into the
/// `OsString` nonstick takes.
///
/// ponytail: nonstick copies that `OsString` into PAM's `char*` and drops it unscrubbed; upgrade by
/// patching nonstick to zeroize its answers.
struct RelayConversation<R, W> {
    reader: RefCell<R>,
    writer: RefCell<W>,
    /// Called when the Supervisor is gone. Returning `ConversationError` instead would let
    /// `pam_faillock authfail` count a failure nobody typed.
    hangup: fn() -> !,
}

impl<R: std::io::Read, W: std::io::Write> RelayConversation<R, W> {
    fn send(&self, message: &shared::PamMessage) {
        if write_frame(&mut *self.writer.borrow_mut(), message).is_err() {
            (self.hangup)()
        }
    }

    fn relay(&self, request: &std::ffi::OsStr, echo: bool) -> nonstick::Result<std::ffi::OsString> {
        use std::os::unix::ffi::OsStringExt;
        self.send(&shared::PamMessage::Prompt { text: request.to_string_lossy().into_owned(), echo });
        match read_frame(&mut *self.reader.borrow_mut()) {
            Ok(shared::PamMessage::Response { secret }) => Ok(std::ffi::OsString::from_vec(secret)),
            Ok(_) => Err(nonstick::ErrorCode::ConversationError),
            Err(_) => (self.hangup)(),
        }
    }

    fn notify(&self, kind: shared::PamNoticeKind, message: &std::ffi::OsStr) {
        self.send(&shared::PamMessage::Message { kind, text: message.to_string_lossy().into_owned() });
    }
}

impl<R: std::io::Read, W: std::io::Write> ConversationAdapter for RelayConversation<R, W> {
    fn prompt(&self, request: impl AsRef<std::ffi::OsStr>) -> nonstick::Result<std::ffi::OsString> {
        self.relay(request.as_ref(), true)
    }

    fn masked_prompt(&self, request: impl AsRef<std::ffi::OsStr>) -> nonstick::Result<std::ffi::OsString> {
        self.relay(request.as_ref(), false)
    }

    fn error_msg(&self, message: impl AsRef<std::ffi::OsStr>) {
        self.notify(shared::PamNoticeKind::Error, message.as_ref());
    }

    fn info_msg(&self, message: impl AsRef<std::ffi::OsStr>) {
        self.notify(shared::PamNoticeKind::Info, message.as_ref());
    }
}

/// The worker's [`RelayConversation::hangup`]: `exit`, so no PAM module sees the conversation fail.
fn exit_now() -> ! {
    std::process::exit(1)
}

/// Blocking because nonstick's conversation callbacks are synchronous FFI.
fn write_frame(writer: impl std::io::Write, message: &shared::PamMessage) -> std::io::Result<()> {
    let payload = serde_json::to_vec(message).map_err(std::io::Error::other)?;
    Ok(shared::framing::write_frame_blocking(writer, &payload)?)
}

/// [`write_frame`]'s read half. The wire buffer is `Zeroizing`: only a `Response` carries a secret,
/// but every frame is scrubbed alike.
fn read_frame(reader: impl std::io::Read) -> std::io::Result<shared::PamMessage> {
    let payload = shared::Zeroizing::new(shared::framing::read_frame_blocking(reader)?);
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

/// GDM's reauthentication rule (ADR-0381): the session is already logged in, so an expired
/// password still unlocks it. Changing it is the login's job, not the lock screen's.
fn account_verdict(result: nonstick::Result<()>) -> shared::PamOutcome {
    match result {
        Ok(()) | Err(nonstick::ErrorCode::NewAuthTokRequired) => shared::PamOutcome::Success,
        Err(err) => outcome_for_error(err),
    }
}

/// Runs `pam_start` via `TransactionBuilder`, then `authenticate` and `account_management`,
/// relaying every message over stdio (ADR-0381).
fn run_conversation(username: &str, service: &str) -> shared::PamOutcome {
    let conversation = RelayConversation {
        reader: RefCell::new(std::io::stdin().lock()),
        writer: RefCell::new(std::io::stdout().lock()),
        hangup: exit_now,
    };
    match nonstick::TransactionBuilder::new_with_service(service)
        .username(username)
        .build(conversation.into_conversation())
    {
        Ok(mut txn) => match txn.authenticate(nonstick::AuthnFlags::empty()) {
            Err(err) => outcome_for_error(err),
            Ok(()) => account_verdict(txn.account_management(nonstick::AuthnFlags::empty())),
        },
        Err(err) => shared::PamOutcome::StartFailed(format!("{err:?}")),
    }
}

/// ADR-0028/ADR-0241 worker path for `MANTLE_PAM_WORKER=1`. `main.rs` enters it before
/// D-Bus/runtime/audio setup. Every `stdin`/`stdout` round trip PAM asks for happens inside
/// `run_conversation`; this only writes the frame that ends it. Plain blocking I/O throughout, so
/// no runtime is spun up here.
pub fn run_worker() -> Result<(), Box<dyn std::error::Error>> {
    let username = std::env::var(USERNAME_ENV)?;
    let mode = std::env::var(MODE_ENV)?;
    let service = pam_service(&mode).ok_or_else(|| format!("unknown PAM mode {mode:?}"))?;
    let outcome = run_conversation(&username, service);
    write_frame(std::io::stdout().lock(), &shared::PamMessage::Outcome(outcome))?;
    Ok(())
}

pub(crate) fn username_for(uid: u32) -> Result<String, String> {
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

/// A running conversation: the lock's worker or polkit's helper. Dropping it ends the conversation,
/// by SIGTERM to the worker or by closing the helper's socket, never by answering.
pub struct Conversation {
    answers: UnboundedSender<shared::Zeroizing<Vec<u8>>>,
}

impl Conversation {
    /// Answers the prompt the conversation is waiting on. A closed channel means the task already
    /// ended, and its `Outcome` is on the way.
    pub fn answer(&self, secret: shared::Zeroizing<Vec<u8>>) {
        let _ = self.answers.send(secret);
    }

    /// A conversation with no task behind it, and the receiver its answers reach.
    #[cfg(test)]
    pub fn detached() -> (Self, tokio::sync::mpsc::UnboundedReceiver<shared::Zeroizing<Vec<u8>>>) {
        let (answers, received) = tokio::sync::mpsc::unbounded_channel();
        (Self { answers }, received)
    }
}

/// Each frame a conversation relays, tagged with the serial it was started under, so the receiver
/// drops what a killed one says late.
pub type Events = UnboundedSender<(u64, shared::PamMessage)>;

/// Starts the lock's worker for `uid` in `mode` (ADR-0381). The worker begins PAM at once, so its
/// prompt is on screen before anything is typed.
pub fn start_lock_conversation(uid: u32, mode: &'static str, serial: u64, events: Events) -> Conversation {
    let (answers, received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(lock_conversation(uid, mode, serial, events, received));
    Conversation { answers }
}

/// Starts polkit's helper for `uid` and `cookie` (ADR-0114, ADR-0381). Called on the first submit,
/// not on the challenge: closing a helper that is waiting on a prompt fails its PAM conversation,
/// and `pam_faillock` would count every cancel.
pub fn start_polkit_conversation(uid: u32, cookie: String, serial: u64, events: Events) -> Conversation {
    let (answers, received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(polkit_conversation(uid, cookie, serial, events, received));
    Conversation { answers }
}

type Answers = tokio::sync::mpsc::UnboundedReceiver<shared::Zeroizing<Vec<u8>>>;

async fn lock_conversation(uid: u32, mode: &str, serial: u64, events: Events, answers: Answers) {
    let mut guard = ReportOnDrop { pending: Some((serial, events.clone())) };
    let ended = match username_for(uid).and_then(|name| spawn_worker(&name, mode).map_err(|err| err.to_string())) {
        Err(reason) => Exchange::Verdict(shared::PamOutcome::StartFailed(reason)),
        Ok(child) => converse_with_worker(child, serial, &events, answers, PAM_REPLY_TIMEOUT).await,
    };
    guard.finish(ended, "pam");
}

async fn polkit_conversation(uid: u32, cookie: String, serial: u64, events: Events, answers: Answers) {
    let mut guard = ReportOnDrop { pending: Some((serial, events.clone())) };
    let ended = match open_helper(uid, &cookie).await {
        Err(reason) => Exchange::Verdict(shared::PamOutcome::StartFailed(reason)),
        Ok((reader, mut writer)) => {
            let (frames_tx, frames) = tokio::sync::mpsc::unbounded_channel();
            let lines = tokio::spawn(read_helper_lines(reader, frames_tx));
            let ended = converse(frames, &mut writer, Wire::Lines, serial, &events, answers, PAM_REPLY_TIMEOUT).await;
            lines.abort();
            ended
        }
    };
    guard.finish(ended, "polkit");
}

/// libpolkit-agent's `polkitagentsession.c` protocol opens with the username and cookie lines.
/// Verified against polkit 127.
async fn open_helper(
    uid: u32,
    cookie: &str,
) -> Result<(tokio::net::unix::OwnedReadHalf, tokio::net::unix::OwnedWriteHalf), String> {
    use tokio::io::AsyncWriteExt;
    let username = username_for(uid)?;
    let (reader, mut writer) = tokio::net::UnixStream::connect(POLKIT_HELPER_SOCKET)
        .await
        .map_err(|err| format!("could not connect to {POLKIT_HELPER_SOCKET}: {err}"))?
        .into_split();
    writer
        .write_all(format!("{username}\n{cookie}\n").as_bytes())
        .await
        .map_err(|err| format!("could not write to the polkit helper: {err}"))?;
    Ok((reader, writer))
}

/// Forwards the helper's lines as [`shared::PamMessage`]s until its verdict or EOF.
async fn read_helper_lines(
    reader: impl tokio::io::AsyncRead + Unpin,
    frames: UnboundedSender<std::io::Result<shared::PamMessage>>,
) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(reader).lines();
    loop {
        let message = match lines.next_line().await {
            Ok(Some(line)) => match helper_message(&line) {
                Some(message) => Ok(message),
                None => {
                    debug!("polkit helper: {line}");
                    continue;
                }
            },
            Ok(None) => Err(std::io::Error::other("the helper closed without a verdict")),
            Err(err) => Err(err),
        };
        let last = !matches!(message, Ok(shared::PamMessage::Prompt { .. } | shared::PamMessage::Message { .. }));
        if frames.send(message).is_err() || last {
            return;
        }
    }
}

/// One helper line as the worker would have framed it. The helper `g_strescape`s each line;
/// libpolkit-agent undoes it with `g_strcompress` before parsing, and so does this.
fn helper_message(line: &str) -> Option<shared::PamMessage> {
    use shared::{PamMessage, PamNoticeKind};
    let line = unescape(line);
    let text = |prefix: &str| line.strip_prefix(prefix).map(|rest| rest.strip_prefix(' ').unwrap_or(rest).to_string());
    match line.as_str() {
        "SUCCESS" => return Some(PamMessage::Outcome(shared::PamOutcome::Success)),
        "FAILURE" => return Some(PamMessage::Outcome(shared::PamOutcome::AuthFailed)),
        _ => {}
    }
    if let Some(text) = text("PAM_PROMPT_ECHO_OFF") {
        Some(PamMessage::Prompt { text, echo: false })
    } else if let Some(text) = text("PAM_PROMPT_ECHO_ON") {
        Some(PamMessage::Prompt { text, echo: true })
    } else if let Some(text) = text("PAM_ERROR_MSG") {
        Some(PamMessage::Message { kind: PamNoticeKind::Error, text })
    } else {
        text("PAM_TEXT_INFO").map(|text| PamMessage::Message { kind: PamNoticeKind::Info, text })
    }
}

/// GLib's `g_strcompress`: C escapes and up to three octal digits, so a localized `Contraseña` and a
/// multi-line notice arrive as written.
fn unescape(line: &str) -> String {
    let mut out = Vec::with_capacity(line.len());
    let mut bytes = line.bytes().peekable();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        match bytes.next() {
            Some(digit @ b'0'..=b'7') => {
                let mut value = u32::from(digit - b'0');
                for _ in 0..2 {
                    match bytes.next_if(|next| (b'0'..=b'7').contains(next)) {
                        Some(next) => value = value * 8 + u32::from(next - b'0'),
                        None => break,
                    }
                }
                out.push((value & 0xff) as u8);
            }
            Some(b'b') => out.push(0x08),
            Some(b'f') => out.push(0x0c),
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b't') => out.push(b'\t'),
            Some(b'v') => out.push(0x0b),
            Some(other) => out.push(other),
            None => {}
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// How a conversation ended, before it becomes the reported [`shared::PamOutcome`].
enum Exchange {
    Verdict(shared::PamOutcome),
    Failed(std::io::Error),
    /// The [`Conversation`] was dropped; nobody is waiting for a verdict.
    Cancelled,
}

/// How an answer goes onto the wire.
enum Wire {
    /// The worker's `shared::framing` `Response`.
    Framed,
    /// One line for polkit's helper, which reads with `fgets`.
    Lines,
}

impl Wire {
    async fn send(
        &self,
        writer: &mut (impl tokio::io::AsyncWrite + Unpin),
        mut secret: shared::Zeroizing<Vec<u8>>,
    ) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        match self {
            Wire::Framed => {
                let response =
                    shared::Zeroizing::new(shared::PamMessage::Response { secret: std::mem::take(&mut *secret) });
                shared::framing::write_json_frame(writer, &*response).await.map_err(std::io::Error::other)
            }
            // A line break would end the answer early and turn the rest into the next one.
            Wire::Lines if secret.contains(&b'\n') => {
                Err(std::io::Error::other("an answer with a line break cannot reach polkit's helper"))
            }
            Wire::Lines => {
                writer.write_all(&secret).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await
            }
        }
    }
}

/// Relays `frames` as events and writes each answer, until a verdict, a failure, the reply
/// timeout, or the [`Conversation`] being dropped. Frames arrive over a channel, not a read in the
/// `select!`, because a frame read is not cancel-safe.
async fn converse(
    mut frames: tokio::sync::mpsc::UnboundedReceiver<std::io::Result<shared::PamMessage>>,
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    wire: Wire,
    serial: u64,
    events: &Events,
    mut answers: Answers,
    reply_timeout: Duration,
) -> Exchange {
    let mut deadline = None;
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(Ok(shared::PamMessage::Outcome(outcome))) => return Exchange::Verdict(outcome),
                Some(Ok(shared::PamMessage::Response { .. })) => {
                    return Exchange::Failed(std::io::Error::other("the worker sent a Response, which is the Supervisor's to send"));
                }
                Some(Ok(message)) => {
                    if matches!(message, shared::PamMessage::Prompt { .. }) {
                        deadline = None;
                    }
                    let _ = events.send((serial, message));
                }
                Some(Err(err)) => return Exchange::Failed(err),
                None => return Exchange::Failed(std::io::Error::other("the conversation closed without a verdict")),
            },
            answer = answers.recv() => match answer {
                Some(secret) => {
                    if let Err(err) = wire.send(writer, secret).await {
                        return Exchange::Failed(err);
                    }
                    deadline = Some(tokio::time::Instant::now() + reply_timeout);
                }
                None => return Exchange::Cancelled,
            },
            () = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                return Exchange::Failed(std::io::Error::new(std::io::ErrorKind::TimedOut, "pam did not reply within the timeout"));
            }
        }
    }
}

/// [`converse`] over the worker's pipes, then reaps it. The SIGTERM lands while stdin is still
/// open, so a module waiting on an answer is killed rather than failed. A failed exchange carries
/// [`post_mortem`]'s account of how the worker died, because the reap already knows and the lock
/// screen otherwise reports an I/O error with no subject.
async fn converse_with_worker(
    mut child: tokio::process::Child,
    serial: u64,
    events: &Events,
    answers: Answers,
    reply_timeout: Duration,
) -> Exchange {
    let mut stdin = child.stdin.take().expect("spawn_group_leader_stdio_piped always pipes stdin");
    let mut stdout = child.stdout.take().expect("spawn_group_leader_stdio_piped always pipes stdout");
    let (frames_tx, frames) = tokio::sync::mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        loop {
            let frame = shared::framing::read_json_frame(&mut stdout).await.map_err(std::io::Error::other);
            let last = !matches!(frame, Ok(shared::PamMessage::Prompt { .. } | shared::PamMessage::Message { .. }));
            if frames_tx.send(frame).is_err() || last {
                return;
            }
        }
    });
    let ended = converse(frames, &mut stdin, Wire::Framed, serial, events, answers, reply_timeout).await;
    let reaped = crate::process::reap_process_group(&mut child, crate::process::DEFAULT_REAP_GRACE).await;
    reader.abort();
    drop(stdin);
    if let Err(err) = &reaped {
        warn!("failed to reap pam worker: {err}");
    }
    match ended {
        // A worker that did not answer leaves the lock screen saying only "early eof" (the 2026-09-08
        // lockout had no other evidence), so log how it died, not just the on-screen error text.
        Exchange::Failed(err) => {
            let post_mortem = post_mortem(&reaped);
            error!("pam worker exchange failed: {err}; the worker {post_mortem}");
            Exchange::Verdict(shared::PamOutcome::StartFailed(format!(
                "pam worker failed: {err}; the worker {post_mortem}"
            )))
        }
        other => other,
    }
}

/// The task's `Drop` backstop: reports once if dropped before [`Self::finish`].
///
/// The controller sets `authenticating` and only an outcome clears it; missing a send would create
/// a one-way latch, leaving the lock screen stuck until a VT switch. `UnboundedSender::send` is
/// synchronous, so `Drop::drop` can send while unwinding.
struct ReportOnDrop {
    pending: Option<(u64, Events)>,
}

impl ReportOnDrop {
    /// Sends the outcome this conversation owes, or nothing for a cancelled one. `label` names the
    /// caller because the two paths log under their own names.
    fn finish(&mut self, ended: Exchange, label: &str) {
        let outcome = match ended {
            Exchange::Verdict(outcome) => outcome,
            Exchange::Failed(err) => shared::PamOutcome::StartFailed(err.to_string()),
            Exchange::Cancelled => {
                self.pending = None;
                return;
            }
        };
        if let Some((tag, tx)) = self.pending.take()
            && tx.send((tag, shared::PamMessage::Outcome(outcome))).is_err()
        {
            // A closed channel means `main.rs`'s loop is gone, which `LockController::send` permits.
            warn!("{label}: the outcome channel is closed; dropping an authentication result");
        }
    }
}

impl Drop for ReportOnDrop {
    fn drop(&mut self) {
        if let Some((tag, tx)) = self.pending.take() {
            // Reached by a panic or a dropped future, not by any ordinary `.finish()` call: the
            // security decision this authentication owed was lost, not just delayed.
            error!("an authentication task ended without ever reporting a PAM outcome");
            let outcome =
                shared::PamOutcome::PamError("pam authentication task ended without reporting an outcome".to_string());
            let _ = tx.send((tag, shared::PamMessage::Outcome(outcome)));
        }
    }
}

/// Re-execs this binary as a PAM worker for `username` in `mode`.
fn spawn_worker(username: &str, mode: &str) -> std::io::Result<tokio::process::Child> {
    crate::process::spawn_group_leader_stdio_piped(
        SELF_EXE,
        &[],
        &[
            (WORKER_ENV.to_string(), "1".to_string()),
            (USERNAME_ENV.to_string(), username.to_string()),
            (MODE_ENV.to_string(), mode.to_string()),
        ],
    )
}

/// How the worker died, for a [`converse_with_worker`] that failed.
///
/// `reap_process_group` sends `SIGTERM` before waiting, so a live worker dies by our hand and
/// reports signal 15. That is not the ambiguity it looks like: this runs only when the pipe already
/// closed, which a live process does not do, so in practice the status is the worker's own.
fn post_mortem(reaped: &std::io::Result<std::process::ExitStatus>) -> String {
    match reaped {
        // The worker prints its own reason to the inherited stderr before exiting non-zero, so a code
        // here points at that line rather than telling the whole story.
        Ok(status) => crate::generation::classify_departure(*status).to_string(),
        Err(err) => format!("could not be reaped, so how it died is unknown: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::test_support::within;

    // ---- SELF_EXE (ADR-0161) ----

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
    /// `/etc/pam.d/other`, `pam_deny` on stock Arch, and reject the correct password. An unreadable
    /// PAM directory is "not installed", not a panic; this runs in the unlock worker.
    #[test]
    fn the_pam_service_is_mantle_only_when_a_stack_is_installed() {
        let no_such = std::path::Path::new("/no/such/pam.d");
        assert_eq!(pam_service_in(PASSWORD, no_such, no_such), Some("login"), "missing directories");

        let (admin, vendor) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        assert_eq!(pam_service_in(PASSWORD, admin.path(), vendor.path()), Some("login"), "no stack installed");

        std::fs::write(admin.path().join("mantle"), "auth include system-auth\n").unwrap();
        assert_eq!(pam_service_in(PASSWORD, admin.path(), vendor.path()), Some("mantle"), "admin directory");

        let (admin, vendor) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(vendor.path().join("mantle"), "auth include system-auth\n").unwrap();
        assert_eq!(
            pam_service_in(PASSWORD, admin.path(), vendor.path()),
            Some("mantle"),
            "vendor directory without an override"
        );
    }

    /// The fingerprint worker never falls back: `login` would wait for a password nobody types.
    /// Its probe looks for its own file in both dirs, and `mantle` alone does not count.
    #[test]
    fn the_fingerprint_stack_is_its_own_and_an_unknown_mode_is_refused() {
        let (admin, vendor) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        assert_eq!(pam_service_in(FINGERPRINT, admin.path(), vendor.path()), Some("mantle-fingerprint"));
        assert_eq!(pam_service_in("login", admin.path(), vendor.path()), None, "a mode is not a service name");
        assert_eq!((pam_service(FINGERPRINT), pam_service("login")), (Some("mantle-fingerprint"), None), "on any host");
        std::fs::write(admin.path().join("mantle"), "auth include system-auth\n").unwrap();
        assert!(!installed_in(FINGERPRINT_SERVICE, admin.path(), vendor.path()), "no stack, no fingerprint worker");
        std::fs::write(vendor.path().join(FINGERPRINT_SERVICE), "").unwrap();
        assert!(installed_in(FINGERPRINT_SERVICE, admin.path(), vendor.path()), "vendor directory");
        let (admin, vendor) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(admin.path().join(FINGERPRINT_SERVICE), "").unwrap();
        assert!(installed_in(FINGERPRINT_SERVICE, admin.path(), vendor.path()), "admin directory");
    }

    /// The shipped files are what the probe finds and must carry both chains: `run_conversation`
    /// calls `authenticate`, then `account_management`; without `account`, the second fails after
    /// password acceptance.
    #[test]
    fn the_shipped_pam_stacks_declare_both_chains_the_worker_drives() {
        for name in ["mantle", "mantle-debian", "mantle-fingerprint", "mantle-fingerprint-debian"] {
            let path = format!("{}/../packaging/pam.d/{name}", env!("CARGO_MANIFEST_DIR"));
            let shipped = std::fs::read_to_string(path).unwrap();
            let directives: Vec<String> = shipped
                .lines()
                .filter(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
                .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect();
            let chain = |prefix: &str| directives.iter().any(|line| line.trim_start_matches('-').starts_with(prefix));
            assert!(chain("auth") && chain("account"), "{name}: both chains in {directives:?}");
            assert!(
                directives
                    .iter()
                    .all(|line| line.trim_start_matches('-').starts_with("auth") || line.starts_with("account")),
                "{name}: the worker opens no session and changes no password, so anything else is dead config: {directives:?}"
            );
            if name.starts_with("mantle-fingerprint") {
                // `required` would run `authsucc` after a failed scan and reset the faillock tally.
                assert!(directives.contains(&"-auth requisite pam_fprintd.so max-tries=-1 timeout=-1".to_string()));
                assert!(
                    !directives.iter().any(|line| line.contains("authfail")),
                    "{name}: a missed scan must not count toward a lockout"
                );
            }
        }
    }

    /// Upper bound for ordinary paths, never waited on in a passing run. Distinct from [`PAM_REPLY_TIMEOUT`].
    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    #[test]
    fn outcome_for_error_maps_known_errors_and_names_any_other_variant() {
        for (error, want) in [
            (nonstick::ErrorCode::MaxTries, shared::PamOutcome::MaxTries),
            (nonstick::ErrorCode::AuthenticationError, shared::PamOutcome::AuthFailed),
            (nonstick::ErrorCode::SystemError, shared::PamOutcome::PamError("SystemError".to_string())),
        ] {
            assert_eq!(outcome_for_error(error), want, "{error:?}");
        }
    }

    /// GDM's rule: an expired password still unlocks the session it already logged in.
    #[test]
    fn an_expired_password_passes_the_account_check_and_other_refusals_do_not() {
        assert_eq!(account_verdict(Ok(())), shared::PamOutcome::Success);
        assert_eq!(account_verdict(Err(nonstick::ErrorCode::NewAuthTokRequired)), shared::PamOutcome::Success);
        assert_eq!(account_verdict(Err(nonstick::ErrorCode::PermissionDenied)), shared::PamOutcome::AuthFailed);
    }

    /// A frame as the worker writes it: 4-byte big-endian length, then JSON.
    fn frame_bytes(message: &shared::PamMessage) -> Vec<u8> {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, message).unwrap();
        bytes
    }

    /// A fake worker: `sh` printing `frames`, then running `then`.
    fn fake_worker(frames: &[shared::PamMessage], then: &str) -> tokio::process::Child {
        let octal: String = frames.iter().flat_map(frame_bytes).map(|byte| format!("\\{byte:03o}")).collect();
        let script = format!("printf '{octal}'; {then}");
        crate::process::spawn_group_leader_stdio_piped("sh", &crate::process::sh_args(&script), &[])
            .expect("failed to spawn the fake worker")
    }

    async fn with_worker(child: tokio::process::Child, timeout: Duration) -> (Exchange, Vec<shared::PamMessage>) {
        let (events, mut heard) = tokio::sync::mpsc::unbounded_channel();
        // Held so the exchange is never cancelled.
        let (_answer_tx, answers) = tokio::sync::mpsc::unbounded_channel();
        let ended = within(converse_with_worker(child, 7, &events, answers, timeout)).await;
        let mut seen = Vec::new();
        while let Ok((serial, message)) = heard.try_recv() {
            assert_eq!(serial, 7, "every frame carries the conversation's serial");
            seen.push(message);
        }
        (ended, seen)
    }

    fn verdict(ended: Exchange) -> shared::PamOutcome {
        match ended {
            Exchange::Verdict(outcome) => outcome,
            Exchange::Failed(err) => panic!("expected a verdict, got failure {err}"),
            Exchange::Cancelled => panic!("expected a verdict, got a cancel"),
        }
    }

    // Real libpam re-exec needs a real PAM stack; unit-test the wire protocol with a fake shell
    // worker instead.

    #[tokio::test]
    async fn the_worker_s_messages_are_relayed_and_its_outcome_ends_the_conversation() {
        let info = shared::PamMessage::Message { kind: shared::PamNoticeKind::Info, text: "Place your finger".into() };
        let child = fake_worker(&[info.clone(), shared::PamMessage::Outcome(shared::PamOutcome::Success)], "");
        let (ended, seen) = with_worker(child, TEST_TIMEOUT).await;
        assert_eq!(verdict(ended), shared::PamOutcome::Success);
        assert_eq!(seen, [info]);
    }

    #[tokio::test]
    async fn an_undecodable_frame_fails_and_the_worker_is_still_reaped() {
        let script = r#"printf '\000\000\000\004evil'; sleep 30"#;
        let child = crate::process::spawn_group_leader_stdio_piped("sh", &crate::process::sh_args(script), &[])
            .expect("failed to spawn the fake worker");
        let pid = child.id().expect("freshly spawned child has a pid");
        let (ended, _) = with_worker(child, TEST_TIMEOUT).await;
        assert!(matches!(verdict(ended), shared::PamOutcome::StartFailed(_)));
        assert!(crate::process::exited(&[pid]).await, "the worker (pid {pid}) should be reaped, not left sleeping");
    }

    /// The 2026-09-08 lockout: the worker vanished and "early eof" was the entire diagnosis.
    #[tokio::test]
    async fn a_worker_that_dies_without_answering_reports_how_it_died() {
        for (script, expected) in [("exit 3", "exited with code 3"), ("kill -KILL $$", "was killed by signal 9")] {
            let child = crate::process::spawn_group_leader_stdio_piped("sh", &crate::process::sh_args(script), &[])
                .expect("failed to spawn the fake worker");
            let (ended, _) = with_worker(child, TEST_TIMEOUT).await;
            let shared::PamOutcome::StartFailed(message) = verdict(ended) else { panic!("{script}: not StartFailed") };
            assert!(message.contains(expected), "`{script}` should report `{expected}`, got: {message}");
        }
    }

    /// A prompt waits for the user with no limit; once an answer is written, PAM owes a reply within
    /// the timeout, and info lines do not buy it more time.
    #[tokio::test(start_paused = true)]
    async fn the_reply_timeout_runs_only_after_an_answer_and_a_message_does_not_pause_it() {
        let (frames_tx, frames) = tokio::sync::mpsc::unbounded_channel();
        let (answer_tx, answers) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut heard) = tokio::sync::mpsc::unbounded_channel();
        let timeout = Duration::from_secs(30);
        let mut writer = Vec::new();
        let task = async { converse(frames, &mut writer, Wire::Lines, 1, &events, answers, timeout).await };
        let driver = async {
            frames_tx.send(Ok(shared::PamMessage::Prompt { text: "Password:".into(), echo: false })).unwrap();
            tokio::time::sleep(timeout * 10).await;
            assert!(heard.try_recv().is_ok(), "the prompt reached the controller and is still waiting");
            answer_tx.send(shared::Zeroizing::new(b"hunter2".to_vec())).unwrap();
            for _ in 0..10 {
                tokio::time::sleep(timeout / 5).await;
                let line = shared::PamMessage::Message { kind: shared::PamNoticeKind::Info, text: "...".into() };
                if frames_tx.send(Ok(line)).is_err() {
                    break;
                }
            }
            std::future::pending::<()>().await
        };
        let ended = tokio::select! { ended = task => ended, () = driver => unreachable!() };
        let Exchange::Failed(err) = ended else { panic!("a module silent after the answer must time out") };
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(writer, b"hunter2\n");
    }

    /// Dropping the [`Conversation`] cancels without an answer and without a verdict.
    #[tokio::test]
    async fn dropping_the_conversation_cancels_without_writing_anything() {
        let (_frames_tx, frames) = tokio::sync::mpsc::unbounded_channel();
        let (answer_tx, answers) = tokio::sync::mpsc::unbounded_channel();
        let (events, _heard) = tokio::sync::mpsc::unbounded_channel();
        drop(answer_tx);
        let mut writer = Vec::new();
        let ended = within(converse(frames, &mut writer, Wire::Framed, 1, &events, answers, TEST_TIMEOUT)).await;
        assert!(matches!(ended, Exchange::Cancelled));
        assert!(writer.is_empty());
    }

    #[tokio::test]
    async fn an_answer_with_a_line_break_never_reaches_the_helper() {
        let mut writer = Vec::new();
        let sent = Wire::Lines.send(&mut writer, shared::Zeroizing::new(b"one\ntwo".to_vec())).await;
        assert!(sent.is_err());
        assert!(writer.is_empty());
    }

    #[test]
    fn helper_lines_are_unescaped_into_prompts_messages_and_verdicts() {
        use shared::{PamMessage, PamNoticeKind};
        for (line, want) in [
            (
                r"PAM_PROMPT_ECHO_OFF Contrase\303\261a: ",
                Some(PamMessage::Prompt { text: "Contraseña: ".into(), echo: false }),
            ),
            ("PAM_PROMPT_ECHO_ON Code:", Some(PamMessage::Prompt { text: "Code:".into(), echo: true })),
            (
                r"PAM_ERROR_MSG locked\nfor 10 minutes",
                Some(PamMessage::Message { kind: PamNoticeKind::Error, text: "locked\nfor 10 minutes".into() }),
            ),
            (
                r#"PAM_TEXT_INFO say \"hi\"\t\\"#,
                Some(PamMessage::Message { kind: PamNoticeKind::Info, text: "say \"hi\"\t\\".into() }),
            ),
            ("SUCCESS", Some(PamMessage::Outcome(shared::PamOutcome::Success))),
            ("FAILURE", Some(PamMessage::Outcome(shared::PamOutcome::AuthFailed))),
            ("SUCCESS but not quite", None),
        ] {
            assert_eq!(helper_message(line), want, "{line}");
        }
        assert_eq!(unescape(r"\b\f\r\v\101\7x\"), "\u{8}\u{c}\r\u{b}A\u{7}x");
    }

    #[tokio::test]
    async fn the_helper_s_lines_are_relayed_until_its_verdict() {
        let (ours, mut theirs) = tokio::io::duplex(256);
        tokio::io::AsyncWriteExt::write_all(&mut theirs, b"PAM_TEXT_INFO hi\nnoise\nFAILURE\nPAM_TEXT_INFO late\n")
            .await
            .unwrap();
        let (frames_tx, mut frames) = tokio::sync::mpsc::unbounded_channel();
        within(read_helper_lines(ours, frames_tx)).await;
        let info = shared::PamMessage::Message { kind: shared::PamNoticeKind::Info, text: "hi".into() };
        assert_eq!(frames.recv().await.unwrap().unwrap(), info);
        assert_eq!(frames.recv().await.unwrap().unwrap(), shared::PamMessage::Outcome(shared::PamOutcome::AuthFailed));
        assert!(frames.recv().await.is_none(), "nothing after the verdict");
    }

    #[test]
    fn username_for_names_the_passwd_entry_or_refuses() {
        assert_eq!(username_for(0).as_deref(), Ok("root"));
        assert!(username_for(UNMAPPED_UID).unwrap_err().contains("no passwd entry"));
    }

    /// Far above any login or `systemd-homed` range, so no passwd entry exists.
    const UNMAPPED_UID: u32 = 3_999_999_999;

    #[tokio::test]
    async fn a_conversation_that_cannot_start_reports_once_under_its_serial() {
        let (events, mut heard) = tokio::sync::mpsc::unbounded_channel();
        let _lock = start_lock_conversation(UNMAPPED_UID, PASSWORD, 7, events.clone());
        let _polkit = start_polkit_conversation(UNMAPPED_UID, "cookie".into(), 8, events);
        let mut serials = Vec::new();
        for _ in 0..2 {
            let (serial, message) = within(heard.recv()).await.expect("every conversation reports");
            serials.push(serial);
            assert!(
                matches!(&message, shared::PamMessage::Outcome(shared::PamOutcome::StartFailed(reason)) if reason.contains("no passwd entry")),
                "{message:?}"
            );
        }
        serials.sort();
        assert_eq!(serials, [7, 8]);
        assert!(within(heard.recv()).await.is_none(), "exactly one report each, then the tasks are gone");
    }

    #[test]
    fn read_frame_surfaces_a_mid_stream_read_error() {
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
    }

    fn hung_up() -> ! {
        panic!("the supervisor hung up")
    }

    /// The worker's actual conversation driver, proven without PAM or a subprocess: both prompt
    /// kinds and both message kinds go out as frames, each prompt answered from the wire.
    #[test]
    fn relay_conversation_relays_every_prompt_and_message() {
        let mut canned = Vec::new();
        for secret in [&b"hunter2"[..], b"123456"] {
            write_frame(&mut canned, &shared::PamMessage::Response { secret: secret.to_vec() }).unwrap();
        }
        let conversation = RelayConversation {
            reader: RefCell::new(std::io::Cursor::new(canned)),
            writer: RefCell::new(Vec::new()),
            hangup: hung_up,
        };

        assert_eq!(conversation.masked_prompt("Password:").unwrap(), std::ffi::OsString::from("hunter2"));
        conversation.info_msg("Place your finger");
        assert_eq!(conversation.prompt("Code:").unwrap(), std::ffi::OsString::from("123456"));
        conversation.error_msg("Wrong code");

        let mut sent = std::io::Cursor::new(conversation.writer.into_inner());
        let kinds = [shared::PamNoticeKind::Info, shared::PamNoticeKind::Error];
        for want in [
            shared::PamMessage::Prompt { text: "Password:".into(), echo: false },
            shared::PamMessage::Message { kind: kinds[0], text: "Place your finger".into() },
            shared::PamMessage::Prompt { text: "Code:".into(), echo: true },
            shared::PamMessage::Message { kind: kinds[1], text: "Wrong code".into() },
        ] {
            assert_eq!(read_frame(&mut sent).unwrap(), want);
        }
    }

    /// A closed stdin is the Supervisor gone: the worker leaves rather than fail PAM's conversation,
    /// which `pam_faillock authfail` would count.
    #[test]
    fn a_closed_pipe_hangs_up_instead_of_failing_the_prompt() {
        let conversation = RelayConversation {
            reader: RefCell::new(std::io::Cursor::new(Vec::new())),
            writer: RefCell::new(Vec::new()),
            hangup: hung_up,
        };
        let asked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| conversation.masked_prompt("Password:")));
        assert!(asked.is_err(), "the prompt must not return into PAM");
    }

    #[tokio::test]
    async fn a_panicking_task_still_reports_a_fallback_outcome_via_the_drop_guard() {
        // `tokio::spawn` catches an FFI panic, but locals, including `ReportOnDrop`, still drop;
        // this pins that fallback send.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(async move {
            let _guard = ReportOnDrop { pending: Some((7, tx)) };
            panic!("simulated panic before the task's own explicit send");
        });
        assert!(handle.await.is_err(), "the task did panic");

        let (serial, message) = rx.try_recv().expect("the drop guard must still report a fallback outcome");
        assert!(matches!(message, shared::PamMessage::Outcome(shared::PamOutcome::PamError(_))));
        assert_eq!(serial, 7, "tagged with its serial, or the controller cannot place it");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_finished_or_cancelled_conversation_sends_no_fallback() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut guard = ReportOnDrop { pending: Some((7, tx.clone())) };
        guard.finish(Exchange::Verdict(shared::PamOutcome::Success), "test");
        drop(guard);
        assert_eq!(rx.try_recv(), Ok((7, shared::PamMessage::Outcome(shared::PamOutcome::Success))));
        let mut guard = ReportOnDrop { pending: Some((8, tx)) };
        guard.finish(Exchange::Cancelled, "test");
        drop(guard);
        assert!(rx.try_recv().is_err(), "a cancelled conversation owes nobody a verdict");
    }
}
