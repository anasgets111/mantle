//! Exact Hyprland window addresses for ext capture sources. No title or app-id matching.

use wayland_client::Dispatch;
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{
    ext_foreign_toplevel_handle_v1::{self, ExtForeignToplevelHandleV1},
    ext_foreign_toplevel_list_v1::{self, ExtForeignToplevelListV1},
};
use wayland_protocols::ext::image_capture_source::v1::client::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1;

use super::*;
use protocol::{
    hyprland_toplevel_mapping_manager_v1::HyprlandToplevelMappingManagerV1,
    hyprland_toplevel_window_mapping_handle_v1::{self, HyprlandToplevelWindowMappingHandleV1},
};

// The extension is not in wayland-protocols. Its XML retains the upstream license.
#[allow(
    dead_code,
    non_camel_case_types,
    unused_unsafe,
    unused_variables,
    non_upper_case_globals,
    non_snake_case,
    unused_imports,
    clippy::all
)]
mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;
    use wayland_protocols::ext::foreign_toplevel_list::v1::client::*;
    use wayland_protocols_wlr::foreign_toplevel::v1::client::*;
    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols::ext::foreign_toplevel_list::v1::client::__interfaces::*;
        use wayland_protocols_wlr::foreign_toplevel::v1::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/hyprland-toplevel-mapping-v1.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocols/hyprland-toplevel-mapping-v1.xml");
}

struct Toplevel {
    address: Option<String>,
    mapping: Option<HyprlandToplevelWindowMappingHandleV1>,
}

pub(super) struct Windows {
    pub(super) sources: ExtForeignToplevelImageCaptureSourceManagerV1,
    mapping: HyprlandToplevelMappingManagerV1,
    _list: ExtForeignToplevelListV1,
    toplevels: HashMap<ExtForeignToplevelHandleV1, Toplevel>,
}

impl Windows {
    pub(super) fn bind(globals: &GlobalList, qh: &QueueHandle<App>) -> Option<Self> {
        // ponytail: Hyprland only. Niri needs a toplevel capture source; wlr needs an exact
        // bridge from the supervisor's connection-local IDs. Add backends when those exist.
        let mapping = globals.bind(qh, 1..=1, ()).ok()?;
        let sources = globals.bind(qh, 1..=1, ()).ok()?;
        let list = globals.bind(qh, 1..=1, ()).ok()?;
        Some(Self { mapping, sources, _list: list, toplevels: HashMap::new() })
    }

    pub(super) fn handle(&self, address: &str) -> Option<ExtForeignToplevelHandleV1> {
        self.toplevels
            .iter()
            .find_map(|(handle, row)| (row.address.as_deref() == Some(address)).then(|| handle.clone()))
    }
}

fn address(hi: u32, low: u32) -> String {
    format!("0x{:x}", (u64::from(hi) << 32) | u64::from(low))
}

impl Dispatch<ExtForeignToplevelListV1, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } => {
                let Some(windows) = state.captures.backend.windows.as_mut() else {
                    toplevel.destroy();
                    return;
                };
                let mapping = windows.mapping.get_window_for_toplevel(&toplevel, qh, toplevel.clone());
                windows.toplevels.insert(toplevel, Toplevel { address: None, mapping: Some(mapping) });
            }
            ext_foreign_toplevel_list_v1::Event::Finished => {
                if let Some(windows) = state.captures.backend.windows.take() {
                    for (handle, row) in windows.toplevels {
                        if let Some(mapping) = row.mapping {
                            mapping.destroy();
                        }
                        handle.destroy();
                    }
                    windows.mapping.destroy();
                    windows.sources.destroy();
                }
                proxy.destroy();
                let ids: Vec<_> = state
                    .captures
                    .sources
                    .iter()
                    .filter_map(|(id, source)| matches!(source.target, CaptureTarget::Window(_)).then_some(*id))
                    .collect();
                for id in ids {
                    state.captures.stop(id, &mut state.capture_cache);
                }
            }
            _ => {}
        }
    }
    wayland_client::event_created_child!(App, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if !matches!(event, ext_foreign_toplevel_handle_v1::Event::Closed) {
            return;
        }
        let row = state.captures.backend.windows.as_mut().and_then(|windows| windows.toplevels.remove(proxy));
        if let Some(row) = row {
            if let Some(mapping) = row.mapping {
                mapping.destroy();
            }
            if let Some(address) = row.address {
                let ids: Vec<_> = state
                    .captures
                    .sources
                    .iter()
                    .filter_map(|(id, source)| (source.target == CaptureTarget::Window(address.clone())).then_some(*id))
                    .collect();
                for id in ids {
                    state.captures.stop(id, &mut state.capture_cache);
                }
            }
        }
        proxy.destroy();
    }
}

impl Dispatch<HyprlandToplevelWindowMappingHandleV1, ExtForeignToplevelHandleV1> for App {
    fn event(
        state: &mut Self,
        proxy: &HyprlandToplevelWindowMappingHandleV1,
        event: hyprland_toplevel_window_mapping_handle_v1::Event,
        handle: &ExtForeignToplevelHandleV1,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let row = state.captures.backend.windows.as_mut().and_then(|windows| windows.toplevels.get_mut(handle));
        let Some(row) = row else { return };
        if row.mapping.as_ref() != Some(proxy) {
            return;
        }
        row.mapping = None;
        proxy.destroy();
        if let hyprland_toplevel_window_mapping_handle_v1::Event::WindowAddress { address_hi, address: low } = event {
            let name = address(address_hi, low);
            row.address = Some(name.clone());
            let ids: Vec<_> = state
                .captures
                .sources
                .iter()
                .filter_map(|(id, source)| {
                    (source.target == CaptureTarget::Window(name.clone()) && !source.in_flight && !source.failed)
                        .then_some(*id)
                })
                .collect();
            for id in ids {
                state.request_when_due(id);
            }
        }
    }
}

delegate_noop!(App: HyprlandToplevelMappingManagerV1);
delegate_noop!(App: ExtForeignToplevelImageCaptureSourceManagerV1);

#[cfg(test)]
mod tests {
    use super::address;

    #[test]
    fn mapping_preserves_all_64_address_bits() {
        assert_eq!(address(0x12345678, 0x9abcdef0), "0x123456789abcdef0");
        assert_eq!(address(0, 0xa11ce), "0xa11ce");
    }
}
