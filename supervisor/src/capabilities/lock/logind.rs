//! logind bridge for `mantle.lock` (ADR-0138): `loginctl lock-session` in, `LockedHint` out.
//!
//! `loginctl lock-session` calls `org.freedesktop.login1.Manager.LockSession`, making logind emit
//! `Lock` on this session's object. Ignoring it breaks `systemd-lock-handler`,
//! `xdg-desktop-portal`, `swayidle -l`, and keybinds using `loginctl lock-session`; `systemctl`
//! manages units and has no `systemctl --user lock`.
//!
//! The reverse direction is `Session.SetLockedHint`, reported by `loginctl show-session` as
//! `LockedHint` and read by greeters/session scripts. Only the session owner may set it; this
//! process is that owner.
//!
//! `Unlock` is logged and dropped. ADR-0042 permits only successful PAM authentication to lift a
//! lock; wiring it to `LockController::unlock` would let any bus caller bypass PAM. The way back
//! in remains the password prompt or a VT switch.
//!
//! Both halves degrade to inert, logged once; a shell unable to reach logind still locks when its
//! config asks.

use futures_util::StreamExt;
use shared::{error, warn};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::capabilities::idle::inhibit::Login1ManagerProxy;

/// `default_path` is logind's `session/auto`, the caller's own session; `SessionBridge` overrides it.
#[zbus::proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1/session/auto"
)]
pub(crate) trait Login1Session {
    #[zbus(name = "SetBrightness")]
    fn set_brightness(&self, subsystem: &str, name: &str, brightness: u32) -> zbus::Result<()>;

    /// `loginctl show-session`'s `LockedHint`; settable only by the session owner.
    #[zbus(name = "SetLockedHint")]
    fn set_locked_hint(&self, locked: bool) -> zbus::Result<()>;

    #[zbus(signal, name = "Lock")]
    fn lock(&self) -> zbus::Result<()>;

    #[zbus(signal, name = "Unlock")]
    fn unlock(&self) -> zbus::Result<()>;
}

/// This process's logind session path. Prefer `$XDG_SESSION_ID`, then `"auto"`; they differ only
/// when the variable was inherited from elsewhere, and there the explicit session name wins.
async fn resolve_session_path(connection: &zbus::Connection) -> Result<zbus::zvariant::OwnedObjectPath, String> {
    let manager = Login1ManagerProxy::new(connection).await.map_err(|err| format!("logind is unreachable: {err}"))?;
    let mut tried = String::new();
    if let Some(id) = std::env::var("XDG_SESSION_ID").ok().filter(|id| !id.is_empty()) {
        match manager.get_session(&id).await {
            Ok(path) => return Ok(path),
            Err(err) => tried = format!("XDG_SESSION_ID={id}: {err}; "),
        }
    }
    manager.get_session("auto").await.map_err(|err| format!("{tried}auto: {err}"))
}

/// logind session bridge. Holds the hint sender; both directions run as spawned tasks.
pub struct SessionBridge {
    /// `None` when the session is unresolved; hints then no-op.
    hints: Option<UnboundedSender<bool>>,
}

impl SessionBridge {
    /// Resolves the session, spawns the `Lock`/`Unlock` listener and `SetLockedHint` writer, then
    /// returns. `()` on `lock_requests` means logind requested a lock; `main.rs` decides.
    pub async fn new(connection: zbus::Connection, lock_requests: UnboundedSender<()>) -> Self {
        let path = match resolve_session_path(&connection).await {
            Ok(path) => path,
            Err(why) => {
                warn!(
                    "could not resolve this process's logind session ({why}); `loginctl lock-session` will not \
                     reach the shell and LockedHint will not be published"
                );
                return Self { hints: None };
            }
        };
        let session = match Login1SessionProxy::builder(&connection).path(path.clone()) {
            Ok(builder) => match builder.build().await {
                Ok(session) => session,
                Err(err) => {
                    warn!("failed to bind the logind session at {path}; lock-session will not reach us: {err}");
                    return Self { hints: None };
                }
            },
            Err(err) => {
                warn!("logind handed back an unusable session path {path}: {err}");
                return Self { hints: None };
            }
        };

        tokio::spawn(forward_lock_signals(session.clone(), lock_requests));
        let (hints_tx, hints_rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(publish_locked_hints(session, hints_rx));
        Self { hints: Some(hints_tx) }
    }

    /// Tells logind whether the screen is locked. Fire-and-forget: other software reads
    /// `LockedHint`, but this shell's lock path does not depend on setting it.
    pub fn publish_locked_hint(&self, locked: bool) {
        if let Some(hints) = &self.hints {
            let _ = hints.send(locked);
        }
    }
}

/// Forwards each `Lock` signal as one request. Logs and drops `Unlock` (see the module doc).
async fn forward_lock_signals(session: Login1SessionProxy<'static>, lock_requests: UnboundedSender<()>) {
    let (locks, unlocks) = match (session.receive_lock().await, session.receive_unlock().await) {
        (Ok(locks), Ok(unlocks)) => (locks, unlocks),
        (locks, unlocks) => {
            let err = locks.err().or(unlocks.err());
            warn!("failed to subscribe to logind's Lock/Unlock signals; lock-session will not reach us: {err:?}");
            return;
        }
    };
    let mut locks = locks.fuse();
    let mut unlocks = unlocks.fuse();
    loop {
        tokio::select! {
            signal = locks.next() => {
                if signal.is_none() {
                    break;
                }
                if lock_requests.send(()).is_err() {
                    break; // main is gone.
                }
            }
            signal = unlocks.next() => {
                if signal.is_none() {
                    break;
                }
                error!(
                    "logind asked for an unlock; refusing. Only a successful password authentication lifts a \
                     lock here (ADR-0042), so the way back in is the prompt or a VT switch"
                );
            }
        }
    }
}

/// Writes every hint through in order, so the newest wins. One task serializes changes; per-change
/// tasks could race and leave logind reporting the opposite of what is on the glass.
async fn publish_locked_hints(session: Login1SessionProxy<'static>, mut hints: UnboundedReceiver<bool>) {
    while let Some(locked) = hints.recv().await {
        if let Err(err) = session.set_locked_hint(locked).await {
            warn!("failed to publish LockedHint={locked} to logind: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use zbus::zvariant::OwnedObjectPath;

    use super::*;
    use crate::capabilities::test_support::{p2p_pair_serving, within};

    const SESSION: &str = "/org/freedesktop/login1/session/c7";

    /// logind's Manager, naming one session for any id.
    struct Manager;

    #[zbus::interface(name = "org.freedesktop.login1.Manager")]
    impl Manager {
        fn get_session(&self, _id: &str) -> OwnedObjectPath {
            OwnedObjectPath::try_from(SESSION).unwrap()
        }
    }

    /// The session object, forwarding each `SetLockedHint`.
    struct Session(UnboundedSender<bool>);

    #[zbus::interface(name = "org.freedesktop.login1.Session")]
    impl Session {
        fn set_locked_hint(&self, locked: bool) {
            self.0.send(locked).unwrap();
        }
    }

    #[tokio::test]
    async fn the_bridge_forwards_lock_session_and_publishes_hints_in_order() {
        let (hints_tx, mut hints) = tokio::sync::mpsc::unbounded_channel();
        let (connection, peer) = p2p_pair_serving(|peer| {
            peer.serve_at("/org/freedesktop/login1", Manager)?.serve_at(SESSION, Session(hints_tx))
        })
        .await;
        let (lock_tx, mut lock_requests) = tokio::sync::mpsc::unbounded_channel();
        let bridge = SessionBridge::new(connection, lock_tx).await;

        bridge.publish_locked_hint(true);
        bridge.publish_locked_hint(false);
        assert_eq!(within(hints.recv()).await, Some(true));
        assert_eq!(within(hints.recv()).await, Some(false));

        // The listener subscribes in its own task, so knock until it answers.
        within(async {
            loop {
                peer.emit_signal(None::<&str>, SESSION, "org.freedesktop.login1.Session", "Lock", &()).await.unwrap();
                let knock = tokio::time::timeout(std::time::Duration::from_millis(20), lock_requests.recv()).await;
                if matches!(knock, Ok(Some(()))) {
                    break;
                }
            }
        })
        .await;
    }
}
