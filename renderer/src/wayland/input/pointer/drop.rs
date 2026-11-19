//! Drops from other clients onto shell surfaces: `drop_hover` follows a drag, and the innermost
//! `on_drop` node under it takes the payload.

use std::sync::mpsc::{Receiver, TryRecvError};

use smithay_client_toolkit::data_device_manager::data_offer::DragOffer;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_surface::WlSurface;

use super::target::{PressTarget, press_target};
use super::*;
use crate::layout::node::fields::common;
use crate::wayland::surface::TrackedRole;

const URI_LIST: &str = "text/uri-list";
/// What a drop can carry, best first: a file manager's or browser's URIs, then text.
const MIMES: [&str; 3] = [URI_LIST, "text/plain;charset=utf-8", "text/plain"];
/// ponytail: a drop over 256 URIs (or paste's 64 KiB) is refused whole; raise both together.
const MAX_URIS: usize = 256;

/// A drag over one of this shell's surfaces whose offer carries a readable mime, last at `position`.
/// `target` is the `on_drop` node under it; the offer is accepted exactly while there is one.
pub(in crate::wayland) struct DragOver {
    surface_id: String,
    mime: &'static str,
    position: (f64, f64),
    target: Option<PressTarget>,
}

/// A dropped offer being read off the main thread.
pub(in crate::wayland) struct PendingDrop {
    offer: DragOffer,
    surface_id: String,
    target: PressTarget,
    mime: &'static str,
    result: Receiver<Option<Vec<u8>>>,
}

fn on_drop(node: &layout::ResolvedNode) -> Option<Function> {
    pointer::on_drop.read(&node.properties).ok().flatten()
}

/// A dropped payload as `on_drop`'s `(files, uris, text)`, or `None` for one that is not UTF-8, carries
/// nothing, or lists over [`MAX_URIS`]. A `text/uri-list` (RFC 2483) skips comments and malformed
/// lines; `files` keeps the local `file://` URIs as absolute paths.
fn payload(mime: &str, bytes: Vec<u8>) -> Option<(Vec<String>, Vec<String>, Option<String>)> {
    let text = String::from_utf8(bytes).ok()?;
    if mime != URI_LIST {
        return (!text.is_empty()).then_some((Vec::new(), Vec::new(), Some(text)));
    }
    // A comment (`#`) or blank line has no scheme.
    let uris: Vec<String> = text.lines().map(str::trim).filter(|uri| well_formed(uri)).map(str::to_owned).collect();
    if uris.len() > MAX_URIS {
        return None;
    }
    let files = uris.iter().filter_map(|uri| local_path(uri)).collect();
    (!uris.is_empty()).then_some((files, uris, None))
}

/// A `file:` URI's absolute path (RFC 8089): no host or `localhost`, `?query` and `#fragment` cut,
/// then percent-decoded. `None` for another host, a broken escape, NUL or a non-UTF-8 path.
fn local_path(uri: &str) -> Option<String> {
    let (scheme, rest) = uri.split_once(':')?;
    let rest = rest.split(['?', '#']).next()?;
    let path = match rest.strip_prefix("//") {
        Some(authority) => {
            let (host, path) = authority.split_at(authority.find('/')?);
            (host.is_empty() || host.eq_ignore_ascii_case("localhost")).then_some(path)?
        }
        None => rest,
    };
    if !scheme.eq_ignore_ascii_case("file") || !path.starts_with('/') {
        return None;
    }
    let (mut bytes, mut raw) = (Vec::with_capacity(path.len()), path.bytes());
    while let Some(byte) = raw.next() {
        let hex = |digit: Option<u8>| char::from(digit?).to_digit(16);
        bytes.push(if byte == b'%' { (hex(raw.next())? * 16 + hex(raw.next())?) as u8 } else { byte });
    }
    String::from_utf8(bytes).ok().filter(|path| !path.contains('\0'))
}

/// A scheme (RFC 3986 `ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) ":"`) and no space or control character.
fn well_formed(uri: &str) -> bool {
    let Some((scheme, _)) = uri.split_once(':') else { return false };
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        && !uri.chars().any(|c| c.is_whitespace() || c.is_control())
}

impl App {
    /// Enter (with `surface`) or motion: every `drop_hover` on the hit path turns on, and the offer is
    /// accepted as a copy exactly while an `on_drop` node is under it.
    pub(in crate::wayland::input) fn drag_moved(&mut self, surface: Option<&WlSurface>, position: (f64, f64)) {
        let Some(offer) = self.data_device.as_ref().and_then(|device| device.data().drag_offer()) else { return };
        if let Some(surface) = surface {
            let mime = offer.with_mime_types(|types| MIMES.into_iter().find(|mime| types.iter().any(|t| t == mime)));
            let index =
                self.index_of_surface(surface).filter(|&i| !matches!(self.surfaces[i].role, TrackedRole::Lock { .. }));
            self.drag_over = mime.zip(index).map(|(mime, i)| DragOver {
                surface_id: self.surfaces[i].surface_id.clone(),
                mime,
                position,
                target: None,
            });
        }
        let Some(over) = self.drag_over.as_mut() else { return };
        over.position = position;
        let tree = self.client.scene().surface(&over.surface_id);
        let point = layout::hit::LogicalPoint::from_position(position);
        let path = tree.map(|tree| layout::hit::hit_path(tree, point)).unwrap_or_default();
        let signals: Vec<_> =
            path.iter().filter_map(|node| common::drop_hover.read(&node.properties).ok().flatten()).collect();
        let target = press_target(&path, point, 0, on_drop);
        if target.is_some() != over.target.is_some() {
            let action = if target.is_some() { DndAction::Copy } else { DndAction::empty() };
            offer.set_actions(action, action);
            offer.accept_mime_type(offer.serial, target.is_some().then(|| over.mime.to_owned()));
        }
        over.target = target;
        crate::lua::signal::write_drop_hover(self.client.lua(), &signals);
    }

    /// A pass moved nodes under a still drag: hit-test again where it rests, so `drop_hover`, the
    /// accept and the drop target follow a reload or a slid row the way `hover` does.
    pub(in crate::wayland) fn refresh_drag_after_layout(&mut self) {
        if let Some(position) = self.drag_over.as_ref().map(|over| over.position) {
            self.drag_moved(None, position);
        }
    }

    /// A torn-down role gets no leave.
    pub(in crate::wayland) fn drag_surface_destroyed(&mut self, surface_id: &str) {
        if self.drag_over.as_ref().is_some_and(|over| over.surface_id == surface_id) {
            self.drag_left();
        }
    }

    /// Losing the keyboard capability takes the data device, and no leave follows: an undropped
    /// offer goes with it.
    pub(in crate::wayland) fn drop_data_device(&mut self) {
        let offer = self.data_device.take().and_then(|device| device.data().drag_offer());
        if let Some(offer) = offer.filter(|offer| !offer.dropped) {
            offer.destroy();
        }
        self.drag_left();
    }

    /// Leave, a drop, or the data device going away: every `drop_hover` turns off.
    pub(in crate::wayland) fn drag_left(&mut self) {
        self.drag_over = None;
        crate::lua::signal::write_drop_hover(self.client.lua(), &[]);
    }

    /// Reads a drop on an `on_drop` node off the main thread, else refuses it. The target decides,
    /// not the action: Hyprland sends `action(move)` only before enter, which SCTK drops, and never
    /// answers `set_actions(copy)`; wlroots and smithay drop only an offer accepted with an action.
    pub(in crate::wayland::input) fn drag_dropped(&mut self) {
        let over = self.drag_over.take();
        self.drag_left();
        let Some(offer) = self.data_device.as_ref().and_then(|device| device.data().drag_offer()) else { return };
        let Some(DragOver { surface_id, mime, target: Some(target), .. }) = over else {
            offer.destroy();
            return;
        };
        let Ok(pipe) = offer.receive(mime.to_owned()) else {
            offer.destroy();
            return;
        };
        let _ = self.conn.flush();
        let (tx, result) = std::sync::mpsc::channel();
        let waker = self.waker.clone();
        // A failed spawn drops `tx`, which `finish_drop` reads as a failed read.
        let _ = std::thread::Builder::new().name("mantle-drop".into()).spawn(move || {
            let _ = tx.send(super::super::clipboard::read_bounded(pipe).map(|bytes| bytes.expose_secret().to_vec()));
            waker.wake();
        });
        if let Some(earlier) = self.pending_drop.replace(PendingDrop { offer, surface_id, target, mime, result }) {
            earlier.offer.destroy();
        }
    }

    /// Lands a read drop: `finish` tells the source it was taken, then `on_drop` runs if its node is
    /// still there. A failed or refused read only destroys the offer.
    pub(in crate::wayland) fn finish_drop(&mut self) {
        let Some(pending) = &self.pending_drop else { return };
        let read = match pending.result.try_recv() {
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
            Ok(read) => read,
        };
        let Some(pending) = self.pending_drop.take() else { return };
        let tree = self.client.scene().surface(&pending.surface_id);
        let node = tree.and_then(|tree| layout::hit::path_to_node(tree, pending.target.id));
        let handler = node.and_then(|path| on_drop(path.last()?));
        // A newer drag's enter destroys the offer in SCTK's slot, superseding this drop.
        let landed = read.and_then(|bytes| payload(pending.mime, bytes)).zip(handler);
        let landed = landed.filter(|_| pending.offer.inner().is_alive());
        if landed.is_some() {
            // No-op below `wl_data_offer` v3, which has no `finish`.
            pending.offer.finish();
        }
        pending.offer.destroy();
        let Some(((files, uris, text), handler)) = landed else { return };
        let PressTarget { rect, at: pointer, .. } = pending.target;
        let dropped = layout::node::Dropped { files, uris, text, rect, pointer };
        let lua = self.client.lua();
        crate::lua::focus::begin_callback(lua, &pending.surface_id, false);
        call_logged(&handler, dropped, format_args!("{}: on_drop", pending.surface_id));
        crate::lua::focus::end_callback(lua);
    }
}

#[cfg(test)]
mod tests {
    use wayland_client::{EventQueue, Proxy};

    use super::*;
    use crate::wayland::test_support::MockWayland;

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_uri_list_keeps_local_files_and_well_formed_uris_and_refuses_the_rest() {
        let local = [
            "file:///tmp/a%20b.png",
            "file://localhost/c",
            "file://LOCALHOST/d",
            "file:/e",
            "file:///f.pdf#p=2",
            "file:///%C3%A9",
        ];
        let other = [
            "https://x/?y",
            "file://remote/x",
            "file://%2Ftmp/r",
            "file:///a%",
            "file:///nul%00",
            "file:///%ff",
            "trash:///t",
            "file:rel",
        ];
        let list = format!(
            "# from a file manager\r\n{}\r\n{}\r\nno scheme\r\n1bad:x\r\nfile:///tab\there\r\nfile:///a b\r\n\r\n",
            local.join("\r\n"),
            other.join("\r\n")
        );
        let (files, uris, text) = payload(URI_LIST, list.into()).unwrap();
        assert_eq!(files, strings(&["/tmp/a b.png", "/c", "/d", "/e", "/f.pdf", "/é"]));
        assert_eq!(uris, strings(&[&local[..], &other[..]].concat()), "every URI with a scheme, as sent");
        assert_eq!(text, None);

        assert!(payload(URI_LIST, b"file:///a\xff".to_vec()).is_none(), "not UTF-8");
        assert!(payload(URI_LIST, b"# only a comment\r\n".to_vec()).is_none(), "nothing dropped");
        let many = |n| (0..n).map(|i| format!("file:///{i}\r\n")).collect::<String>().into_bytes();
        assert_eq!(payload(URI_LIST, many(MAX_URIS)).unwrap().0.len(), MAX_URIS);
        assert!(payload(URI_LIST, many(MAX_URIS + 1)).is_none(), "over the cap is refused whole");

        assert_eq!(payload(MIMES[1], "héllo\n".into()), Some((vec![], vec![], Some("héllo\n".into()))));
        assert!(payload(MIMES[1], Vec::new()).is_none());
    }

    /// `mode = "spring"` removes the `on_drop` node and slides the spring-loaded one under x = 50.
    const DROP_CONFIG: &str = r##"
        log = {}
        mode = state("mode", "both")
        local function zone(name, on_drop)
            return rect { width = 100, height = 40, background = "#111111", drop_hover = drop_hover(name), on_drop = on_drop }
        end
        return panel { id = "bar", layer = "top", output = "active", width = 200, height = 40,
            child = row { children = computed({ mode }, function(m)
                if m == "spring" then return { zone("spring") } end
                return { zone("files", function(drop)
                    log[#log + 1] = table.concat(drop.files, ",") .. "|" .. #drop.uris .. "|" .. drop.pointer.x .. "|" .. drop.rect.width
                end), zone("spring") }
            end) } }
    "##;

    struct Wire {
        app: App,
        _dir: tempfile::TempDir,
        queue: EventQueue<App>,
        mock: MockWayland,
    }

    impl Wire {
        fn new() -> Self {
            let (mut app, _dir, queue, mock) = App::test_app_with_wire(DROP_CONFIG);
            let seat = app.seat_state.seats().next().unwrap();
            app.data_device = app.data_device_manager.as_ref().map(|m| m.get_data_device(&queue.handle(), &seat));
            Self { app, _dir, queue, mock }
        }

        fn device(&self) -> u32 {
            self.app.data_device.as_ref().unwrap().inner().id().protocol_id()
        }

        fn send(&mut self, object: u32, opcode: u16, args: &[u32]) {
            self.mock.send(object, opcode, &args.iter().flat_map(|arg| arg.to_ne_bytes()).collect::<Vec<_>>());
            // The second flushes what the handlers sent during the first, so the request log holds it.
            self.queue.roundtrip(&mut self.app).unwrap();
            self.queue.roundtrip(&mut self.app).unwrap();
        }

        /// A drag entering the bar at `x` as Hyprland sends it: every source action, then `action(move)`
        /// before enter, which SCTK drops, and none after. Server ids count up from `0xff00_0000`.
        fn enter(&mut self, offer: u32, mimes: &[&str], x: f64) {
            let device = self.device();
            self.mock.send(device, 0, &offer.to_ne_bytes()); // wl_data_device.data_offer
            for mime in mimes {
                let mut arg = ((mime.len() + 1) as u32).to_ne_bytes().to_vec();
                arg.extend(mime.as_bytes());
                arg.resize(arg.len() + 1 + (4 - (arg.len() + 1) % 4) % 4, 0);
                self.mock.send(offer, 0, &arg); // wl_data_offer.offer
            }
            self.mock.send(offer, 1, &7u32.to_ne_bytes()); // wl_data_offer.source_actions
            self.mock.send(offer, 2, &DndAction::Move.bits().to_ne_bytes()); // wl_data_offer.action
            let surface = self.app.surfaces[0].role.wl_surface().unwrap().id().protocol_id();
            self.send(device, 1, &[5, surface, fixed(x), fixed(20.0), offer]);
        }

        fn motion(&mut self, x: f64) {
            let device = self.device();
            self.send(device, 3, &[0, fixed(x), fixed(20.0)]);
        }

        /// Drop then the compositor's leave.
        fn drop(&mut self) {
            let device = self.device();
            self.send(device, 4, &[]);
            self.send(device, 2, &[]);
        }

        /// The reader's answer lands. The mock closes the pipe unread, so it is stood in for, as
        /// `start_paste`'s is in its tests.
        fn land(&mut self, read: Option<&[u8]>) {
            let (tx, result) = std::sync::mpsc::channel();
            tx.send(read.map(<[u8]>::to_vec)).unwrap();
            self.app.pending_drop.as_mut().unwrap().result = result;
            self.app.finish_drop();
            self.queue.roundtrip(&mut self.app).unwrap();
        }

        fn layout(&mut self, mode: &str) {
            self.app.client.lua().load(format!("mode:set({mode:?})")).exec().unwrap();
            assert!(self.app.client.re_resolve_if_dirty());
            self.app.refresh_hover_after_layout();
            self.queue.roundtrip(&mut self.app).unwrap();
        }

        fn requests(&self, object: u32, opcode: u16) -> Vec<u32> {
            let log = self.mock.requests.lock().unwrap();
            log.iter().filter(|&&(id, op, _)| id == object && op == opcode).map(|&(_, _, arg)| arg).collect()
        }

        fn hovered(&self) -> (bool, bool) {
            let lua = self.app.client.lua();
            lua.load(r#"return drop_hover("files"):get(), drop_hover("spring"):get()"#).eval().unwrap()
        }
    }

    fn fixed(value: f64) -> u32 {
        (value * 256.0) as i32 as u32
    }

    const SET_ACTIONS: u16 = 4;
    const ACCEPT: u16 = 0;
    const RECEIVE: u16 = 1;
    const DESTROY: u16 = 2;
    const FINISH: u16 = 3;
    const FILE: &[u8] = b"file:///tmp/a%20b.png\r\nhttps://example.com/\r\n";

    /// Hyprland and niri both hand layer surfaces the drag; the shell accepts it only over `on_drop`.
    #[test]
    fn a_drag_is_accepted_only_over_an_on_drop_node_and_drop_hover_follows_it_off_the_surface() {
        let mut wire = Wire::new();
        let offer = 0xff00_0000;
        wire.enter(offer, &["application/x-gtk-local-dnd", "text/plain;charset=utf-8", URI_LIST], 50.0);
        assert_eq!(wire.requests(offer, SET_ACTIONS), [DndAction::Copy.bits()]);
        assert_eq!(wire.requests(offer, ACCEPT), [5], "accepted at the enter serial");
        assert_eq!(wire.hovered(), (true, false));

        wire.motion(150.0);
        assert_eq!(wire.requests(offer, SET_ACTIONS), [1, 0], "a node with only drop_hover takes no drop");
        assert_eq!(wire.hovered(), (false, true), "but its drop_hover still turns on, for spring-loading");
        wire.motion(160.0);
        assert_eq!(wire.requests(offer, SET_ACTIONS).len(), 2, "motion inside one node sends nothing");
        // A pass hit-tests again where the drag last moved, not where it entered.
        wire.layout("spring");
        assert_eq!(wire.hovered(), (false, false), "x = 160 is past the one node left");
        wire.layout("both");

        // Layout moving the target out from under a still drag un-accepts it, as a motion would.
        wire.motion(50.0);
        wire.layout("spring");
        assert_eq!(wire.requests(offer, SET_ACTIONS), [1, 0, 1, 0]);
        assert_eq!(wire.hovered(), (false, true));

        let device = wire.device();
        wire.send(device, 2, &[]); // wl_data_device.leave
        assert_eq!(wire.hovered(), (false, false));
        assert_eq!(
            (wire.requests(offer, SET_ACTIONS).len(), wire.requests(offer, ACCEPT).len()),
            (4, 4),
            "leave sends nothing"
        );

        // A drag carrying only an image is not one the shell can read: nothing reacts.
        wire.layout("both");
        let image = 0xff00_0001;
        wire.enter(image, &["image/png"], 50.0);
        assert!(wire.requests(image, SET_ACTIONS).is_empty() && wire.requests(image, ACCEPT).is_empty());
        assert_eq!(wire.hovered(), (false, false));
    }

    #[test]
    fn a_drop_on_an_on_drop_node_is_read_then_finished_and_one_elsewhere_is_refused() {
        let mut wire = Wire::new();
        // Released over the spring-loaded node: refused, never read.
        let refused = 0xff00_0000;
        wire.enter(refused, &[URI_LIST], 150.0);
        wire.drop();
        assert!(wire.requests(refused, RECEIVE).is_empty());
        assert_eq!(wire.requests(refused, DESTROY).len(), 1);
        assert!(wire.app.pending_drop.is_none());

        // Hyprland's only action is `move`, before enter: the target alone decides.
        let offer = 0xff00_0001;
        wire.enter(offer, &[URI_LIST], 50.0);
        wire.drop();
        assert_eq!(wire.hovered(), (false, false), "drop_hover ends at the release");
        assert_eq!(wire.requests(offer, RECEIVE).len(), 1);
        assert!(wire.requests(offer, DESTROY).is_empty(), "a dropped offer outlives the leave until it is read");
        wire.land(Some(FILE));
        assert_eq!((wire.requests(offer, FINISH).len(), wire.requests(offer, DESTROY).len()), (1, 1));
        assert_eq!(wire.app.lua_log(), ["/tmp/a b.png|2|50.0|100.0"]);
        wire.app.finish_drop();
        assert_eq!(wire.app.lua_log().len(), 1, "on_drop runs once");

        // An oversize, slow or broken read refuses the drop: no `finish`, no `on_drop`.
        let failed = 0xff00_0002;
        wire.enter(failed, &[URI_LIST], 50.0);
        assert_eq!(wire.requests(offer, DESTROY).len(), 1, "SCTK's destroy at the next enter sends nothing more");
        wire.drop();
        wire.land(None);
        assert_eq!((wire.requests(failed, FINISH).len(), wire.requests(failed, DESTROY).len()), (0, 1));

        // A new drag entering before the read lands destroys the dropped offer: no `finish` on it, no `on_drop`.
        let superseded = 0xff00_0003;
        wire.enter(superseded, &[URI_LIST], 50.0);
        wire.drop();
        wire.enter(0xff00_0004, &[URI_LIST], 50.0);
        wire.land(Some(FILE));
        assert!(wire.requests(superseded, FINISH).is_empty());

        // The target gone by the time the read lands, here by a reload: no `finish`, no `on_drop`.
        let orphan = 0xff00_0005;
        wire.enter(orphan, &[URI_LIST], 50.0);
        wire.drop();
        wire.layout("spring");
        wire.land(Some(FILE));
        assert_eq!((wire.requests(orphan, FINISH).len(), wire.requests(orphan, DESTROY).len()), (0, 1));
        assert_eq!(wire.app.lua_log().len(), 1);
    }

    /// Neither losing the data device nor a role teardown sends a leave.
    #[test]
    fn a_lost_data_device_or_a_closed_surface_ends_the_drag() {
        let mut wire = Wire::new();
        wire.enter(0xff00_0000, &[URI_LIST], 50.0);
        wire.app.drop_data_device();
        wire.queue.roundtrip(&mut wire.app).unwrap();
        assert_eq!(wire.requests(0xff00_0000, DESTROY).len(), 1, "an undropped offer dies with its device");
        assert_eq!(wire.hovered(), (false, false));

        let mut wire = Wire::new();
        wire.enter(0xff00_0000, &[URI_LIST], 50.0);
        wire.app.drop_role_object(0);
        assert_eq!(wire.hovered(), (false, false));
        assert!(wire.app.drag_over.is_none());
    }
}
