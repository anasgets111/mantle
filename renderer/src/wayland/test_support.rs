use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use wayland_client::EventQueue;
use wayland_client::globals::registry_queue_init;

use super::*;

type WireLog = Arc<Mutex<Vec<(u32, u16, u32)>>>;
type RectLog = Arc<Mutex<Vec<(u32, [i32; 4])>>>;

pub(in crate::wayland) struct MockWayland {
    pub(in crate::wayland) requests: WireLog,
    /// Every opcode-1 request with four `int` args, by object: what `wl_region.add` sends.
    rects: RectLog,
    lock_id: Arc<AtomicU32>,
    writer: Mutex<UnixStream>,
    /// Every frame the app queued for the Supervisor.
    outbound: Mutex<tokio::sync::mpsc::UnboundedReceiver<RendererFrame>>,
}

impl MockWayland {
    pub(in crate::wayland) fn commits_for(&self, surface_id: u32) -> usize {
        self.requests.lock().unwrap().iter().filter(|&&(id, op, _)| id == surface_id && op == 6).count()
    }

    pub(in crate::wayland) fn blur_regions_for(&self, effect_id: u32) -> Vec<u32> {
        let reqs = self.requests.lock().unwrap();
        reqs.iter().filter_map(|&(id, op, arg)| (id == effect_id && op == 1).then_some(arg)).collect()
    }

    /// The rects of `effect_id`'s last `set_blur_region`; without `delete_id` no region id is reused.
    pub(in crate::wayland) fn last_blur_rects(&self, effect_id: u32) -> Vec<[i32; 4]> {
        let region = *self.blur_regions_for(effect_id).last().expect("a region was set");
        self.rects.lock().unwrap().iter().filter(|(id, _)| *id == region).map(|&(_, r)| r).collect()
    }

    /// Sends one event, `payload` already wire-encoded.
    pub(in crate::wayland) fn send(&self, object_id: u32, opcode: u16, payload: &[u8]) {
        let mut msg = Vec::new();
        push_wire_msg(&mut msg, object_id, opcode, payload);
        self.writer.lock().unwrap().write_all(&msg).unwrap();
    }

    pub(in crate::wayland) fn send_lock_finished(&self) {
        self.send(self.lock_object(), 1, &[]);
    }

    /// The compositor grants the last lock asked for.
    pub(in crate::wayland) fn send_lock_locked(&self) {
        self.send(self.lock_object(), 0, &[]);
    }

    /// The `LockReport`s queued since the last call.
    pub(in crate::wayland) fn lock_reports(&self) -> Vec<LockReport> {
        let mut outbound = self.outbound.lock().unwrap();
        std::iter::from_fn(|| outbound.try_recv().ok())
            .filter_map(|frame| match frame {
                RendererFrame::LockReport(report) => Some(report),
                _ => None,
            })
            .collect()
    }

    pub(in crate::wayland) fn lock_object(&self) -> u32 {
        let id = self.lock_id.load(Ordering::Acquire);
        assert_ne!(id, 0, "no lock was asked for");
        id
    }
}

fn push_wire_str(buf: &mut Vec<u8>, s: &str) {
    buf.extend_from_slice(&((s.len() + 1) as u32).to_ne_bytes());
    buf.extend_from_slice(s.as_bytes());
    buf.resize(buf.len() + 1 + (4 - (buf.len() + 1) % 4) % 4, 0);
}

fn push_wire_msg(buf: &mut Vec<u8>, object_id: u32, opcode: u16, payload: &[u8]) {
    let size = ((8 + payload.len()) as u32) << 16 | u32::from(opcode);
    buf.extend_from_slice(&object_id.to_ne_bytes());
    buf.extend_from_slice(&size.to_ne_bytes());
    buf.extend_from_slice(payload);
}

fn spawn_mock_wayland_server(mut stream: UnixStream, wire: Option<(WireLog, RectLog, Arc<AtomicU32>)>) {
    std::thread::spawn(move || {
        let mut req = [0u8; 24]; // `wl_display.get_registry(2)` + `wl_display.sync(3)`
        if stream.read_exact(&mut req).is_err() {
            return;
        }
        let registry_id = u32::from_ne_bytes(req[8..12].try_into().unwrap());
        let callback_id = u32::from_ne_bytes(req[20..24].try_into().unwrap());
        let all_globals = [
            (1u32, "wl_compositor", 4u32),
            (2, "zwlr_layer_shell_v1", 1),
            (3, "wl_shm", 1),
            (4, "xdg_wm_base", 3),
            (5, "wl_output", 4),
            (6, "ext_background_effect_manager_v1", 1),
            (7, "ext_session_lock_manager_v1", 1),
            (8, "wl_seat", 7),
            (9, "wl_data_device_manager", 3),
            (10, "wp_fractional_scale_manager_v1", 1),
            (11, "wp_viewporter", 1),
            (12, "wp_single_pixel_buffer_manager_v1", 1),
        ];
        let mut reply = Vec::new();
        for &(name, iface, version) in &all_globals[..if wire.is_some() { all_globals.len() } else { 4 }] {
            let mut payload = name.to_ne_bytes().to_vec();
            push_wire_str(&mut payload, iface);
            payload.extend_from_slice(&version.to_ne_bytes());
            push_wire_msg(&mut reply, registry_id, 0, &payload);
        }
        push_wire_msg(&mut reply, callback_id, 0, &0u32.to_ne_bytes());
        push_wire_msg(&mut reply, 1, 1, &callback_id.to_ne_bytes());
        if stream.write_all(&reply).is_err() {
            return;
        }
        let Some((requests, rects, lock_id)) = wire else {
            let mut discard = [0u8; 256];
            while matches!(stream.read(&mut discard), Ok(n) if n > 0) {}
            return;
        };
        let mut lock_mgr_id = None;
        let mut hdr = [0u8; 8];
        while stream.read_exact(&mut hdr).is_ok() {
            let object_id = u32::from_ne_bytes(hdr[0..4].try_into().unwrap());
            let word = u32::from_ne_bytes(hdr[4..8].try_into().unwrap());
            let opcode = (word & 0xffff) as u16;
            let mut payload = vec![0u8; ((word >> 16) as usize).saturating_sub(8)];
            if !payload.is_empty() && stream.read_exact(&mut payload).is_err() {
                return;
            }
            let arg0 = payload.get(0..4).map_or(0, |b| u32::from_ne_bytes(b.try_into().unwrap()));
            let mut out = Vec::new();
            if object_id == 1 && opcode == 0 {
                push_wire_msg(&mut out, arg0, 0, &0u32.to_ne_bytes());
                push_wire_msg(&mut out, 1, 1, &arg0.to_ne_bytes());
            } else if object_id == registry_id && opcode == 0 && payload.len() >= 16 {
                let slen = u32::from_ne_bytes(payload[4..8].try_into().unwrap()) as usize;
                let new_id = u32::from_ne_bytes(payload[payload.len() - 4..].try_into().unwrap());
                match payload.get(8..8 + slen.saturating_sub(1)).and_then(|b| std::str::from_utf8(b).ok()) {
                    Some("wl_output") => {
                        let mut name = Vec::new();
                        push_wire_str(&mut name, "TEST");
                        push_wire_msg(&mut out, new_id, 4, &name);
                        push_wire_msg(&mut out, new_id, 2, &[]);
                    }
                    Some("ext_background_effect_manager_v1") => {
                        push_wire_msg(&mut out, new_id, 0, &1u32.to_ne_bytes());
                    }
                    Some("ext_session_lock_manager_v1") => lock_mgr_id = Some(new_id),
                    _ => {}
                }
            } else if lock_mgr_id == Some(object_id) && opcode == 1 {
                lock_id.store(arg0, Ordering::Release);
            }
            if opcode == 1 && payload.len() == 16 {
                let arg = |i: usize| i32::from_ne_bytes(payload[i * 4..i * 4 + 4].try_into().unwrap());
                rects.lock().unwrap().push((object_id, [arg(0), arg(1), arg(2), arg(3)]));
            }
            requests.lock().unwrap().push((object_id, opcode, arg0));
            if !out.is_empty() && stream.write_all(&out).is_err() {
                return;
            }
        }
    });
}

impl App {
    fn build_test_app(
        source: &str,
        with_wire: bool,
    ) -> (Self, tempfile::TempDir, EventQueue<App>, Option<MockWayland>) {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let (outbound_tx, outbound) = tokio::sync::mpsc::unbounded_channel();
        let mock = with_wire.then(|| MockWayland {
            requests: Arc::default(),
            rects: Arc::default(),
            lock_id: Arc::default(),
            writer: Mutex::new(server_stream.try_clone().unwrap()),
            outbound: Mutex::new(outbound),
        });
        spawn_mock_wayland_server(
            server_stream,
            mock.as_ref().map(|m| (Arc::clone(&m.requests), Arc::clone(&m.rects), Arc::clone(&m.lock_id))),
        );
        let conn = Connection::from_socket(client_stream).unwrap();
        let (globals, mut event_queue) = registry_queue_init::<App>(&conn).unwrap();
        let qh = event_queue.handle();

        let mandatory = (
            CompositorState::bind(&globals, &qh).unwrap(),
            LayerShell::bind(&globals, &qh).unwrap(),
            Shm::bind(&globals, &qh).unwrap(),
        );
        let waker = crate::wake::Waker::new().unwrap();
        let (dir, _path, client, _rx) = crate::socket::client::tests::client_for(source);
        let mut app = App::new(conn, &globals, &qh, mandatory, client, ShapingHandle::spawn(), outbound_tx, 0, waker);

        if mock.is_some() {
            event_queue.roundtrip(&mut app).unwrap();
        }
        let outputs = vec![OutputGeometry {
            name: "TEST".to_string(),
            size: layout::LogicalSize { width: 1920.0, height: 1080.0 },
        }];
        let specs = app.client.run_startup_evaluation().unwrap();
        let instances = expand_instances(&specs, &outputs);
        app.client.set_instances(instances.clone());
        assert!(app.client.apply_instances());
        app.create_surfaces(&qh, &specs, &instances);
        app.startup_complete = true;
        (app, dir, event_queue, mock)
    }

    pub(in crate::wayland) fn test_app(source: &str) -> (Self, tempfile::TempDir) {
        let (app, dir, _, _) = Self::build_test_app(source, false);
        (app, dir)
    }

    /// The config's global `log` sequence, which test handlers append to.
    pub(in crate::wayland) fn lua_log(&self) -> Vec<String> {
        self.client.lua().load("return log").eval().unwrap()
    }

    /// Same as [`App::test_app`], with `wl_output`, `ext_background_effect_manager_v1` (Blur),
    /// `ext_session_lock_manager_v1`, a `wl_seat` that announces nothing, `wl_data_device_manager`,
    /// and a wire request log synchronized via `event_queue.roundtrip`.
    pub(in crate::wayland) fn test_app_with_wire(
        source: &str,
    ) -> (Self, tempfile::TempDir, EventQueue<App>, MockWayland) {
        let (app, dir, queue, mock) = Self::build_test_app(source, true);
        (app, dir, queue, mock.unwrap())
    }
}
