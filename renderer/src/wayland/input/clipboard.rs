//! Wayland selection for plain-field copy and bounded paste into either field kind.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use shared::{Zeroize, Zeroizing};
use smithay_client_toolkit::data_device_manager::data_device::DataDeviceHandler;
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler};
use smithay_client_toolkit::data_device_manager::{ReadPipe, WritePipe};
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::protocol::wl_surface::WlSurface;

use super::*;

const MIME: &str = "text/plain;charset=utf-8";
const FALLBACK_MIME: &str = "text/plain";
const MAX_TEXT_BYTES: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_WRITERS: usize = 4;

pub(in crate::wayland) struct ClipboardSource {
    source: CopyPasteSource,
    text: Arc<str>,
}

#[derive(Clone, PartialEq, Eq)]
enum PasteTarget {
    Plain { surface_id: String, id: layout::scene::NodeId },
    Masked(FocusedField),
}

pub(in crate::wayland) struct PendingPaste {
    target: PasteTarget,
    focus: Option<String>,
    revision: u64,
    offer: wayland_client::protocol::wl_data_offer::WlDataOffer,
    result: std::sync::mpsc::Receiver<Option<Zeroizing<Vec<u8>>>>,
}

/// The offered text, refused whole for a control character; a `multiline` field takes newlines, as `\n`.
fn read_offer(mut pipe: ReadPipe, multiline: bool) -> Option<Zeroizing<Vec<u8>>> {
    use std::os::fd::AsFd;
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut bytes = shared::SecureBuffer::new();
    let mut chunk = Zeroizing::new([0u8; 4096]);
    loop {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return None;
        }
        let mut fds = [nix::poll::PollFd::new(pipe.as_fd(), nix::poll::PollFlags::POLLIN)];
        if nix::poll::poll(&mut fds, crate::wake::poll_timeout(timeout)).ok()? == 0 {
            return None;
        }
        let n = pipe.read(&mut chunk[..]).ok()?;
        if n == 0 {
            let text = std::str::from_utf8(bytes.expose_secret()).ok()?;
            let text = if multiline { crate::lua::focus::normalize_newlines(text) } else { text.into() };
            let result =
                (!crate::lua::focus::refuses(&text, multiline)).then(|| Zeroizing::new(text.as_bytes().to_vec()));
            drop(text);
            bytes.zeroize();
            return result;
        }
        if bytes.len() + n > MAX_TEXT_BYTES {
            return None;
        }
        bytes.push_bytes(&chunk[..n]);
        chunk.zeroize();
    }
}

fn selected_text(field: &FocusedTextField) -> Option<Arc<str>> {
    let (a, b) = field.selection;
    let (from, to) = (a.min(b), a.max(b));
    (from < to).then(|| Arc::from(&field.buffer[from..to]))
}

fn write_copy(mut fd: WritePipe, text: Arc<str>) {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use std::os::fd::AsFd;
    let Ok(flags) = fcntl(&fd, FcntlArg::F_GETFL) else { return };
    if fcntl(&fd, FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK)).is_err() {
        return;
    }
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut remaining = text.as_bytes();
    while !remaining.is_empty() {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return;
        }
        let mut fds = [nix::poll::PollFd::new(fd.as_fd(), nix::poll::PollFlags::POLLOUT)];
        if nix::poll::poll(&mut fds, crate::wake::poll_timeout(timeout)).ok() != Some(1) {
            return;
        }
        match fd.write(&remaining[..remaining.len().min(4096)]) {
            Ok(0) | Err(_) => return,
            Ok(n) => remaining = &remaining[n..],
        }
    }
}

fn claim_writer(active: &AtomicUsize) -> bool {
    active.try_update(Ordering::AcqRel, Ordering::Acquire, |count| (count < MAX_WRITERS).then_some(count + 1)).is_ok()
}

struct WriterPermit(Arc<AtomicUsize>);

impl Drop for WriterPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl App {
    /// Whether the selection reached the clipboard.
    pub(super) fn copy_selection(&mut self, serial: u32) -> bool {
        if self.focused_secure_submit.is_some() {
            return false;
        }
        let Some(field) = self.focused_text_field.as_ref().filter(|field| self.text_field_takes_keys(field)) else {
            return false;
        };
        let Some(text) = selected_text(field) else { return false };
        let (Some(manager), Some(device)) = (&self.data_device_manager, &self.data_device) else { return false };
        let source = manager.create_copy_paste_source(&self.queue_handle, [MIME, FALLBACK_MIME]);
        source.set_selection(device, serial);
        self.clipboard_sources.push(ClipboardSource { source, text });
        true
    }

    /// Ctrl+X: the copy, then one erase of the selection, so one undo step and one `on_change`.
    pub(super) fn cut_selection(&mut self, serial: u32) {
        if self.copy_selection(serial) {
            self.apply_plain_action_inner(
                super::keyboard::KeyAction::Erase(super::keyboard::Motion::Left),
                None,
                false,
            );
        }
    }

    pub(super) fn start_paste(&mut self) {
        if self.paste.is_some() {
            return;
        }
        let target = if self.secure_field_takes_keys() {
            let field = self.focused_secure_submit.as_ref().unwrap();
            PasteTarget::Masked(field.clone())
        } else if let Some(field) = self.focused_text_field.as_ref().filter(|field| self.text_field_takes_keys(field)) {
            PasteTarget::Plain { surface_id: field.surface_id.clone(), id: field.id }
        } else {
            return;
        };
        let Some(device) = &self.data_device else { return };
        let Some(offer) = device.data().selection_offer() else { return };
        let mime = offer.with_mime_types(|types| {
            [MIME, FALLBACK_MIME]
                .into_iter()
                .find(|mime| types.iter().any(|offered| offered == mime))
                .map(str::to_owned)
        });
        let Some(mime) = mime else { return };
        let multiline = match &target {
            PasteTarget::Plain { surface_id, id } => self.field_multiline(surface_id, *id).is_some(),
            PasteTarget::Masked(_) => false,
        };
        let Ok(pipe) = offer.receive(mime) else { return };
        if self.conn.flush().is_err() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let waker = self.waker.clone();
        if std::thread::Builder::new()
            .name("mantle-paste".into())
            .spawn(move || {
                let _ = tx.send(read_offer(pipe, multiline));
                waker.wake();
            })
            .is_err()
        {
            return;
        }
        self.paste = Some(PendingPaste {
            target,
            focus: self.keyboard_focus.clone(),
            revision: self.field_revision,
            offer: offer.inner().clone(),
            result: rx,
        });
    }

    pub(in crate::wayland) fn finish_paste(&mut self) {
        if self.paste.is_none() {
            return;
        }
        self.prune_secure_focus();
        self.prune_text_field_focus();
        let Some(paste) = &self.paste else { return };
        let Ok(result) = paste.result.try_recv() else { return };
        let paste = self.paste.take().unwrap();
        let offer_current = self
            .data_device
            .as_ref()
            .and_then(|device| device.data().selection_offer())
            .is_some_and(|offer| offer.inner() == &paste.offer);
        if !offer_current || self.field_revision != paste.revision || self.keyboard_focus != paste.focus {
            return;
        }
        let Some(bytes) = result else { return };
        let Ok(text) = std::str::from_utf8(&bytes) else { return };
        if text.is_empty() {
            return;
        }
        match paste.target {
            PasteTarget::Masked(field)
                if self.focused_secure_submit.as_ref() == Some(&field) && self.secure_field_takes_keys() =>
            {
                self.push_secure_text(text);
                self.mark_focused_secure_submit_changed();
            }
            PasteTarget::Plain { surface_id, id }
                if self.focused_text_field.as_ref().is_some_and(|field| {
                    field.surface_id == surface_id && field.id == id && self.text_field_takes_keys(field)
                }) =>
            {
                self.apply_plain_action_inner(super::keyboard::KeyAction::Append(text), None, false);
            }
            _ => {}
        }
    }
}

impl DataDeviceHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64, _: &WlSurface) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for App {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

impl DataSourceHandler for App {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: Option<String>) {}
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        fd: WritePipe,
    ) {
        if mime != MIME && mime != FALLBACK_MIME {
            return;
        }
        let Some(copy) = self.clipboard_sources.iter().find(|copy| copy.source.inner() == source) else { return };
        if !claim_writer(&self.clipboard_writers) {
            return;
        }
        let text = Arc::clone(&copy.text);
        let active = Arc::clone(&self.clipboard_writers);
        let thread_active = Arc::clone(&active);
        if std::thread::Builder::new()
            .name("mantle-copy".into())
            .spawn(move || {
                let _permit = WriterPermit(thread_active);
                write_copy(fd, text);
            })
            .is_err()
        {
            active.fetch_sub(1, Ordering::AcqRel);
        }
    }
    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        self.clipboard_sources.retain(|copy| copy.source.inner() != source);
    }
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(bytes: &[u8], multiline: bool) -> Option<Zeroizing<Vec<u8>>> {
        let (read, write) = nix::unistd::pipe().unwrap();
        let data = bytes.to_vec();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::File::from(write);
            let _ = file.write_all(&data);
        });
        let result = read_offer(ReadPipe::from(read), multiline);
        writer.join().unwrap();
        result
    }

    #[test]
    fn paste_accepts_bounded_utf8_and_rejects_invalid_or_multiline_data() {
        assert_eq!(offered("héllo".as_bytes(), false).as_ref().map(|bytes| bytes.as_slice()), Some("héllo".as_bytes()));
        assert!(offered(&[0xff], false).is_none());
        assert!(offered(b"line\nnext", false).is_none());
        assert!(offered(&vec![b'a'; MAX_TEXT_BYTES + 1], false).is_none());
    }

    #[test]
    fn a_multiline_paste_keeps_its_newlines_as_lf_and_still_refuses_other_controls() {
        assert_eq!(offered(b"one\r\ntwo\rthree\n", true).unwrap().as_slice(), b"one\ntwo\nthree\n");
        assert!(offered(b"tab\there", true).is_none());
        assert!(offered(b"line\r\nnext", false).is_none());
    }

    #[test]
    fn selection_copy_requires_a_nonempty_range_and_keeps_large_text() {
        let field = FocusedTextField {
            surface_id: String::new(),
            id: layout::scene::NodeId::test(1),
            buffer: "hello world".into(),
            history: super::keyboard::EditHistory::default(),
            selection: (5, 0),
            typing: true,
            selecting: false,
            span: Default::default(),
            click: None,
            goal_x: None,
            on_change: None,
            on_submit: None,
            on_cancel: None,
            escape: Escape::Clear,
        };
        assert_eq!(selected_text(&field).as_deref(), Some("hello"));
        let mut empty = field.clone();
        empty.selection = (0, 0);
        assert_eq!(selected_text(&empty), None);
        let mut long = field;
        long.buffer = "x".repeat(MAX_TEXT_BYTES + 1);
        long.selection = (0, long.buffer.len());
        assert_eq!(selected_text(&long).as_deref().map(str::len), Some(MAX_TEXT_BYTES + 1));
    }

    #[test]
    fn copy_writer_slots_are_bounded() {
        let active = AtomicUsize::new(0);
        for _ in 0..MAX_WRITERS {
            assert!(claim_writer(&active));
        }
        assert!(!claim_writer(&active));
        active.fetch_sub(1, Ordering::AcqRel);
        assert!(claim_writer(&active));
    }
}
