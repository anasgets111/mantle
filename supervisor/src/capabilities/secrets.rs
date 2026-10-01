//! Named writes to the session Secret Service. Secret bytes never enter a state snapshot.

pub use shared::state::secrets::{SecretStatus, SecretsState};

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gio::prelude::CancellableExt;
use shared::{Zeroizing, valid_secret_name};
use tokio::sync::mpsc::UnboundedSender;

pub struct SecretsController {
    inner: Arc<Mutex<SecretsInner>>,
    events: UnboundedSender<SecretsState>,
}

#[derive(Default)]
struct SecretsInner {
    state: SecretsState,
    in_flight: HashSet<String>,
}

const STORE_TIMEOUT: Duration = Duration::from_secs(30);

fn secret_value(secret: &[u8]) -> Result<libsecret::Value, ()> {
    std::str::from_utf8(secret).map_err(|_| ())?;
    let length = isize::try_from(secret.len()).map_err(|_| ())?;
    // SAFETY: libsecret copies exactly `length` bytes into SecretValue's managed memory. The
    // source remains live for this call; a static NUL-terminated content type needs no conversion.
    // `Value::new(&str, ..)` uses glib's ordinary, unscrubbed temporary string allocation.
    let value = unsafe {
        glib::translate::from_glib_full(libsecret::ffi::secret_value_new(
            secret.as_ptr().cast(),
            length,
            c"text/plain;charset=utf-8".as_ptr(),
        ))
    };
    Ok(value)
}

fn store(name: &str, secret: &[u8], cancellable: &gio::Cancellable) -> Result<(), ()> {
    let schema = libsecret::Schema::new(
        "io.github.anasgets111.mantle.Secret",
        libsecret::SchemaFlags::NONE,
        HashMap::from([("name", libsecret::SchemaAttributeType::String)]),
    );
    let attributes = HashMap::from([("name", name)]);
    let value = secret_value(secret)?;
    libsecret::password_store_binary_sync(
        Some(&schema),
        attributes,
        Some(libsecret::COLLECTION_DEFAULT),
        &format!("Mantle: {name}"),
        &value,
        Some(cancellable),
    )
    .map_err(|_| ())
}

impl SecretsController {
    pub fn new(events: UnboundedSender<SecretsState>) -> Self {
        let controller = Self { inner: Arc::new(Mutex::new(SecretsInner::default())), events };
        controller.publish();
        controller
    }

    fn publish(&self) {
        let _ = self.events.send(self.inner.lock().unwrap().state.clone());
    }

    /// The only entry from `SecureSubmit`. A second write to a pending name is refused.
    pub fn store(&self, name: String, secret: Zeroizing<Vec<u8>>) {
        self.store_with(name, secret, STORE_TIMEOUT, store);
    }

    fn store_with<F>(&self, name: String, secret: Zeroizing<Vec<u8>>, timeout: Duration, backend: F)
    where
        F: FnOnce(&str, &[u8], &gio::Cancellable) -> Result<(), ()> + Send + 'static,
    {
        if !valid_secret_name(&name) {
            return;
        }
        {
            let mut inner = self.inner.lock().unwrap();
            if !inner.in_flight.insert(name.clone()) {
                return;
            }
            inner.state.entries.insert(name.clone(), SecretStatus::Pending);
        }
        self.publish();
        let cancellable = gio::Cancellable::new();
        let timer_cancel = cancellable.clone();
        let timer_inner = Arc::clone(&self.inner);
        let timer_events = self.events.clone();
        let timer_name = name.clone();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(timeout) => {
                    timer_cancel.cancel();
                    let mut inner = timer_inner.lock().unwrap();
                    if inner.in_flight.contains(&timer_name)
                        && matches!(inner.state.entries.get(&timer_name), Some(SecretStatus::Pending))
                    {
                        // ponytail: a cancelled service call may ignore cancellation. Keep the
                        // name busy until it returns so no later write can race the old one.
                        // A wedged call blocks this name until the Supervisor restarts.
                        inner.state.entries.insert(timer_name, SecretStatus::TimedOut);
                        let _ = timer_events.send(inner.state.clone());
                    }
                }
                _ = done_rx => {}
            }
        });
        let inner = Arc::clone(&self.inner);
        let events = self.events.clone();
        tokio::task::spawn_blocking(move || {
            let status = if backend(&name, &secret, &cancellable).is_ok() {
                SecretStatus::Stored
            } else {
                SecretStatus::Unavailable
            };
            let mut inner = inner.lock().unwrap();
            inner.in_flight.remove(&name);
            inner.state.entries.insert(name, status);
            let _ = events.send(inner.state.clone());
            let _ = done_tx.send(());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_secret_value_uses_explicit_byte_length() {
        assert_eq!(secret_value(b"a\0b").unwrap().get(), b"a\0b");
        assert!(secret_value(&[0xff]).is_err());
    }

    #[tokio::test]
    async fn a_mock_store_reports_pending_then_stored_without_the_secret_in_state() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SecretsController::new(tx);
        assert!(rx.recv().await.unwrap().entries.is_empty());
        controller.store_with(
            "mail".into(),
            Zeroizing::new(b"marker-secret".to_vec()),
            STORE_TIMEOUT,
            |name, secret, _| {
                assert_eq!(name, "mail");
                assert_eq!(secret, b"marker-secret");
                Ok(())
            },
        );
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Pending));
        let done = rx.recv().await.unwrap();
        assert!(matches!(done.entries["mail"], SecretStatus::Stored));
        assert!(!serde_json::to_string(&done).unwrap().contains("marker-secret"));
    }

    #[tokio::test]
    async fn failure_has_a_fixed_public_code() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SecretsController::new(tx);
        let _ = rx.recv().await;
        controller.store_with("mail".into(), Zeroizing::new(b"private".to_vec()), STORE_TIMEOUT, |_, _, _| Err(()));
        let _ = rx.recv().await;
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Unavailable));
    }

    #[tokio::test]
    async fn a_pending_name_cannot_start_a_second_write() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SecretsController::new(tx);
        let _ = rx.recv().await;
        let (release, blocked) = std::sync::mpsc::channel();
        controller.store_with("mail".into(), Zeroizing::new(b"first".to_vec()), STORE_TIMEOUT, move |_, _, _| {
            blocked.recv().unwrap();
            Ok(())
        });
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Pending));
        let called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&called);
        controller.store_with("mail".into(), Zeroizing::new(b"second".to_vec()), STORE_TIMEOUT, move |_, _, _| {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });
        release.send(()).unwrap();
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Stored));
        assert!(!called.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn timeout_reports_error_but_holds_the_name_until_the_old_worker_returns() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let controller = SecretsController::new(tx);
        let _ = rx.recv().await;
        let (release, blocked) = std::sync::mpsc::channel();
        controller.store_with(
            "mail".into(),
            Zeroizing::new(b"first".to_vec()),
            Duration::from_millis(10),
            move |_, _, cancel| {
                blocked.recv().unwrap();
                assert!(cancel.is_cancelled());
                Ok(())
            },
        );
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Pending));
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::TimedOut));
        let called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&called);
        controller.store_with("mail".into(), Zeroizing::new(b"second".to_vec()), STORE_TIMEOUT, move |_, _, _| {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });
        release.send(()).unwrap();
        assert!(matches!(rx.recv().await.unwrap().entries["mail"], SecretStatus::Stored));
        assert!(!called.load(Ordering::SeqCst));
    }
}
