//! Per-surface buffer scale and the optional fractional-scale/viewport pair.

use wayland_client::globals::GlobalList;
use wayland_client::protocol::wl_surface;
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

use super::App;

pub(super) struct ScaleGlobals {
    fractional: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    viewporter: wp_viewporter::WpViewporter,
}

pub(super) struct SurfaceScale {
    /// The paint scale in 120ths: the fractional preference with a viewport, otherwise the integer
    /// buffer scale times 120.
    factor_120: u32,
    fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    viewport: Option<wp_viewport::WpViewport>,
}

impl ScaleGlobals {
    pub(super) fn bind(globals: &GlobalList, qh: &QueueHandle<App>) -> Option<Self> {
        Some(Self { fractional: globals.bind(qh, 1..=1, ()).ok()?, viewporter: globals.bind(qh, 1..=1, ()).ok()? })
    }

    pub(super) fn surface(&self, surface: &wl_surface::WlSurface, qh: &QueueHandle<App>) -> SurfaceScale {
        SurfaceScale {
            factor_120: 120,
            fractional: Some(self.fractional.get_fractional_scale(surface, qh, surface.clone())),
            viewport: Some(self.viewporter.get_viewport(surface, qh, ())),
        }
    }
}

impl SurfaceScale {
    pub(super) fn integer() -> Self {
        Self { factor_120: 120, fractional: None, viewport: None }
    }

    pub(super) fn fractional(&self) -> bool {
        self.viewport.is_some()
    }

    pub(super) fn factor_120(&self) -> u32 {
        self.factor_120
    }

    /// `wl_surface.set_buffer_scale`'s argument: one under a viewport, which does the scaling.
    pub(super) fn buffer_scale(&self) -> i32 {
        if self.fractional() { 1 } else { (self.factor_120 / 120) as i32 }
    }

    /// Takes the integer fallback's scale, answering whether it changed.
    pub(super) fn set_integer(&mut self, buffer_scale: i32) -> bool {
        let factor_120 = (buffer_scale.max(1) as u32).saturating_mul(120);
        std::mem::replace(&mut self.factor_120, factor_120) != factor_120
    }

    pub(super) fn physical_size(&self, logical: (u32, u32)) -> (u32, u32) {
        // An integer-scaled buffer edge must be a multiple of the buffer scale, an empty one too.
        let least = u128::from(self.buffer_scale() as u32);
        let edge =
            |n: u32| ((u128::from(n) * u128::from(self.factor_120) + 60) / 120).clamp(least, i32::MAX as u128) as u32;
        (edge(logical.0), edge(logical.1))
    }

    pub(super) fn set_destination(&self, logical: (u32, u32)) {
        if let Some(viewport) = &self.viewport {
            viewport.set_destination(
                logical.0.clamp(1, i32::MAX as u32) as i32,
                logical.1.clamp(1, i32::MAX as u32) as i32,
            );
        }
    }

    pub(super) fn destroy(&mut self) {
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        if let Some(fractional) = self.fractional.take() {
            fractional.destroy();
        }
        self.factor_120 = 120;
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, wl_surface::WlSurface> for App {
    fn event(
        state: &mut Self,
        proxy: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        surface: &wl_surface::WlSurface,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event
            && let Some(index) = state.index_of_surface(surface)
            && state.surfaces[index].scale.fractional.as_ref() == Some(proxy)
            && scale > 0
            && std::mem::replace(&mut state.surfaces[index].scale.factor_120, scale) != scale
        {
            state.surfaces[index].mark_stale();
        }
    }
}

impl Dispatch<wp_viewport::WpViewport, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wp_viewport::WpViewport,
        _event: wp_viewport::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
        _event: wp_fractional_scale_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wp_viewporter::WpViewporter,
        _event: wp_viewporter::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_edges_round_half_away_from_zero() {
        let mut scale = SurfaceScale::integer();
        // A fractional preference, as the event handler stores it.
        scale.factor_120 = 180;
        assert_eq!(scale.physical_size((1, 3)), (2, 5));
        scale.factor_120 = 120;
        assert_eq!(scale.physical_size((0, 0)), (1, 1));
        assert!(scale.set_integer(2) && !scale.set_integer(2));
        assert_eq!((scale.buffer_scale(), scale.factor_120()), (2, 240));
        assert_eq!(scale.physical_size((31, 17)), (62, 34));
        assert_eq!(scale.physical_size((0, 3)), (2, 6), "an empty edge is still a whole buffer pixel");
    }
}
