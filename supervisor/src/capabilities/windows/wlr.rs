//! `windows`' fallback implementor for compositors without niri/Hyprland IPC:
//! `zwlr_foreign_toplevel_management_v1` on its own dedicated Wayland connection, hand-dispatched
//! like `idle::notify`. `id` is this connection's own creation-order counter.
//!
//! ponytail: outputs are bound once at connect time; a monitor hotplugged afterward reports no
//! name for windows that later enter it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shared::{debug, error};
use tokio::sync::mpsc::UnboundedSender;
use wayland_client::globals::{BindError, GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_handle_v1::{
    self, ZwlrForeignToplevelHandleV1,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::zwlr_foreign_toplevel_manager_v1::{
    self, ZwlrForeignToplevelManagerV1,
};

use super::controller::{StatePublisher, WindowEntry, WindowsState};

/// Bound for connection setup, like `idle::notify`'s `IDLE_NOTIFY_SETUP_TIMEOUT`.
const WLR_SETUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Live connection: the seat `activate` requires, and the id-to-handle table writes look up.
pub struct Wlr {
    connection: Connection,
    seat: WlSeat,
    handles: Arc<Mutex<HashMap<String, ZwlrForeignToplevelHandleV1>>>,
}

pub type Handle = Arc<Wlr>;

/// Establishes the dedicated connection, binds `zwlr_foreign_toplevel_manager_v1` if advertised,
/// and spawns its dispatch thread. `None` when the global is absent (this compositor has no
/// implementor here) or the connection fails.
pub async fn connect(events: UnboundedSender<super::WindowsSignal>, state: Arc<Mutex<WindowsState>>) -> Option<Handle> {
    match tokio::time::timeout(WLR_SETUP_TIMEOUT, tokio::task::spawn_blocking(move || connect_blocking(events, state)))
        .await
    {
        Ok(Ok(handle)) => handle,
        Ok(Err(err)) => {
            error!("wlr-foreign-toplevel connection setup task panicked: {err}");
            None
        }
        Err(_) => {
            error!(
                "dedicated Wayland connection setup did not complete within {WLR_SETUP_TIMEOUT:?} \
                 (possible compositor stall); window reporting disabled for this run"
            );
            None
        }
    }
}

/// Dispatch target for the separate connection, keyed by this connection's own id counter.
struct ThreadState {
    outputs: HashMap<wayland_client::backend::ObjectId, String>,
    rows: std::collections::BTreeMap<u64, WindowEntry>,
    /// Outputs each toplevel sits on, oldest first; `row.output` names the first.
    entered_outputs: HashMap<u64, Vec<wayland_client::backend::ObjectId>>,
    handle_ids: HashMap<ZwlrForeignToplevelHandleV1, u64>,
    handles: Arc<Mutex<HashMap<String, ZwlrForeignToplevelHandleV1>>>,
    next_id: u64,
    publisher: StatePublisher,
}

impl ThreadState {
    fn publish(&mut self) {
        self.publisher.publish(self.rows.values().cloned().collect());
    }
}

/// Required by [`registry_queue_init`]; a hotplugged output is not tracked (see the module doc).
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ThreadState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(ThreadState: ignore WlSeat);

impl Dispatch<WlOutput, ()> for ThreadState {
    fn event(
        state: &mut Self,
        proxy: &WlOutput,
        event: wl_output::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            state.outputs.insert(proxy.id(), name);
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for ThreadState {
    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event else { return };
        let id = state.next_id;
        state.next_id += 1;
        state.handle_ids.insert(toplevel.clone(), id);
        state.handles.lock().expect("wlr handles mutex poisoned").insert(id.to_string(), toplevel);
        state.rows.insert(
            id,
            WindowEntry {
                id: id.to_string(),
                title: String::new(),
                app_id: String::new(),
                workspace_id: None,
                output: None,
                focused: false,
                floating: None,
                fullscreen: Some(false),
                minimized: Some(false),
                maximized: Some(false),
            },
        );
    }

    wayland_client::event_created_child!(ThreadState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

/// `state`/`done`/`closed` carry the wire-mandated "atomic batch" contract: accumulate on every
/// other event, publish once `done` says the batch is complete.
impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for ThreadState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        let Some(&id) = state.handle_ids.get(proxy) else { return };
        match event {
            zwlr_foreign_toplevel_handle_v1::Event::Title { title } => {
                if let Some(row) = state.rows.get_mut(&id) {
                    row.title = title;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::AppId { app_id } => {
                if let Some(row) = state.rows.get_mut(&id) {
                    row.app_id = app_id;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::OutputEnter { output } => {
                state.entered_outputs.entry(id).or_default().push(output.id());
                if let Some(row) = state.rows.get_mut(&id)
                    && row.output.is_none()
                {
                    // First output wins: `WindowEntry.output` is one connector, not a list.
                    row.output = state.outputs.get(&output.id()).cloned();
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::OutputLeave { output } => {
                if let Some(entered) = state.entered_outputs.get_mut(&id) {
                    entered.retain(|entry| *entry != output.id());
                }
                let still_entered = state.entered_outputs.get(&id).and_then(|entered| entered.first());
                let next = still_entered.and_then(|output_id| state.outputs.get(output_id)).cloned();
                if let Some(row) = state.rows.get_mut(&id) {
                    row.output = next;
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::State { state: bits } => {
                let (words, _) = bits.as_chunks::<4>();
                let flags: Vec<u32> = words.iter().copied().map(u32::from_ne_bytes).collect();
                let has = |wanted: zwlr_foreign_toplevel_handle_v1::State| flags.contains(&(wanted as u32));
                if let Some(row) = state.rows.get_mut(&id) {
                    row.focused = has(zwlr_foreign_toplevel_handle_v1::State::Activated);
                    row.maximized = Some(has(zwlr_foreign_toplevel_handle_v1::State::Maximized));
                    row.minimized = Some(has(zwlr_foreign_toplevel_handle_v1::State::Minimized));
                    row.fullscreen = Some(has(zwlr_foreign_toplevel_handle_v1::State::Fullscreen));
                }
            }
            zwlr_foreign_toplevel_handle_v1::Event::Done => state.publish(),
            zwlr_foreign_toplevel_handle_v1::Event::Closed => {
                state.rows.remove(&id);
                state.entered_outputs.remove(&id);
                state.handle_ids.remove(proxy);
                state.handles.lock().expect("wlr handles mutex poisoned").remove(&id.to_string());
                proxy.destroy();
                state.publish();
            }
            _ => {}
        }
    }
}

fn connect_blocking(events: UnboundedSender<super::WindowsSignal>, state: Arc<Mutex<WindowsState>>) -> Option<Handle> {
    let connection = Connection::connect_to_env().ok()?;
    let (globals, mut event_queue) = registry_queue_init::<ThreadState>(&connection).ok()?;
    let qh = event_queue.handle();

    let _manager: ZwlrForeignToplevelManagerV1 = match globals.bind(&qh, 1..=3, ()) {
        Ok(manager) => manager,
        Err(BindError::NotPresent) => {
            debug!("zwlr_foreign_toplevel_manager_v1 is not advertised; window reporting disabled for this run");
            return None;
        }
        Err(err) => {
            error!("failed to bind zwlr_foreign_toplevel_manager_v1: {err}");
            return None;
        }
    };
    let seat: WlSeat = globals.bind(&qh, 1..=1, ()).ok()?;
    for global in globals.contents().clone_list().into_iter().filter(|global| global.interface == "wl_output") {
        let _: WlOutput = globals.registry().bind(global.name, global.version.min(4), &qh, ());
    }

    let handles = Arc::new(Mutex::new(HashMap::new()));
    let mut thread_state = ThreadState {
        outputs: HashMap::new(),
        rows: std::collections::BTreeMap::new(),
        entered_outputs: HashMap::new(),
        handle_ids: HashMap::new(),
        handles: Arc::clone(&handles),
        next_id: 0,
        publisher: StatePublisher::new(state, events, "wlr_foreign_toplevel"),
    };
    // Output names arrive before any toplevel does, so the first `output_enter` can resolve one.
    event_queue.roundtrip(&mut thread_state).ok()?;

    let handle = Arc::new(Wlr { connection, seat, handles });
    std::thread::spawn(move || {
        loop {
            if event_queue.blocking_dispatch(&mut thread_state).is_err() {
                debug!("zwlr_foreign_toplevel_manager_v1 dispatch thread exiting");
                break;
            }
        }
    });
    Some(handle)
}

fn with_handle(handle: &Handle, id: &str, action: &str, f: impl FnOnce(&ZwlrForeignToplevelHandleV1)) {
    let Some(toplevel) = handle.handles.lock().expect("wlr handles mutex poisoned").get(id).cloned() else {
        debug!("{action}({id:?}) has no live zwlr_foreign_toplevel_handle_v1; ignored");
        return;
    };
    f(&toplevel);
    if let Err(err) = handle.connection.flush() {
        error!("failed to flush {action}({id:?}): {err}");
    }
}

pub fn activate(handle: &Handle, id: &str) {
    with_handle(handle, id, "focus", |toplevel| toplevel.activate(&handle.seat));
}

pub fn close(handle: &Handle, id: &str) {
    with_handle(handle, id, "close", ZwlrForeignToplevelHandleV1::close);
}

pub fn set_fullscreen(handle: &Handle, id: &str, fullscreen: bool) {
    with_handle(handle, id, "set_fullscreen", |toplevel| {
        if fullscreen { toplevel.set_fullscreen(None) } else { toplevel.unset_fullscreen() }
    });
}

pub fn set_minimized(handle: &Handle, id: &str, minimized: bool) {
    with_handle(handle, id, "set_minimized", |toplevel| {
        if minimized { toplevel.set_minimized() } else { toplevel.unset_minimized() }
    });
}

pub fn set_maximized(handle: &Handle, id: &str, maximized: bool) {
    with_handle(handle, id, "set_maximized", |toplevel| {
        if maximized { toplevel.set_maximized() } else { toplevel.unset_maximized() }
    });
}
