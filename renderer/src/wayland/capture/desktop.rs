//! `effect.shader` with `input = "desktop"`: the output region behind a node, through the capture
//! sources `capture` uses. The capture holds this surface too, so what is behind is unmixed from
//! what the surface last presented there: `capture = ours + (1 - our alpha) * desktop`.

use glow::HasContext;
use smithay_client_toolkit::compositor::SurfaceData;
use smithay_client_toolkit::shell::WaylandSurface;

use super::*;
use crate::image::capture::PendingFrame;
use crate::layout::paint::DisplayList;
use crate::text::snap::PhysicalRect;

/// Frames per second a desktop source may capture at most. Each is a compositor readback, and
/// niri completes one on any redraw of the output, reporting the whole region damaged.
pub(super) const DESKTOP_FPS: f32 = 30.0;

/// The cap once a frame showed no change, until the shell repaints. A change behind a settled
/// glass shows up to 1 / this late.
pub(super) const DESKTOP_IDLE_FPS: f32 = 4.0;

/// Where the surface covers a pixel this opaquely (of 255), nothing behind it shows and the desktop
/// there is unknown. The unmix divides by what is left, 8% here, so a pixel past this reads noise.
const OPAQUE: u8 = 235;

/// A known pixel moving less than this (of 255) per channel, over a clear surface, is not a change:
/// unmixing rounds, and a repaint per rounding step would feed itself.
const SETTLED: u8 = 3;

/// One byte in this many of a capture is compared with the last one's.
/// ponytail: a change of fewer pixels than this reads as none; upgrade path: a rotating offset.
const SAMPLE_STEP: usize = 13;

/// A panel's output name, its top-left there in logical px, its buffer scale and the output's
/// logical size.
type Place = (String, (f32, f32), f32, (f32, f32));

/// A desktop read placed on its output: what to capture, and how its pixels map onto the read.
pub(super) struct Read {
    pub(super) target: CaptureTarget,
    /// The read area on its output, out to whole logical px and cut to the output: the compositor
    /// takes integer regions and scales them to its own buffer pixels.
    pub(super) region: LogicalRect,
    clip: PhysicalRect,
    /// The read's top-left in the region in logical px, and logical px per buffer px.
    origin: (f32, f32),
    step: f32,
    /// What the surface last presented over the read: premultiplied RGBA, GL's bottom row first.
    presented: Vec<u8>,
    /// The paint before `presented`, kept to compare with it.
    was: Vec<u8>,
    /// Captures requested so far, and per pixel of `presented` the count when it last changed.
    /// ponytail: a pixel that changes before every request is never unmixed; upgrade path: a copy of
    /// `presented` per request, unmixed against.
    pub(super) requests: u32,
    changed: Vec<u32>,
    /// Every [`SAMPLE_STEP`]th byte of the last capture.
    sample: Vec<u8>,
}

impl Read {
    fn size(&self) -> (usize, usize) {
        ((self.clip.x1 - self.clip.x0) as usize, (self.clip.y1 - self.clip.y0) as usize)
    }

    /// A capture may still show the paint before its request, and where they differ its unmix is
    /// wrong, multiplied by 1 / (1 - alpha): a pixel is unmixed once it has outlasted one request.
    fn steady(&self, pixel: usize) -> bool {
        self.requests.wrapping_sub(self.changed[pixel]) >= 2
    }

    /// Records `presented` as replacing `was`; a different size starts every pixel over.
    fn note_paint(&mut self) {
        if self.was.len() != self.presented.len() {
            self.changed.clear();
            self.changed.resize(self.presented.len() / 4, self.requests);
            return;
        }
        let pixels = self.presented.as_chunks::<4>().0.iter().zip(self.was.as_chunks::<4>().0);
        for (stamp, (now, was)) in self.changed.iter_mut().zip(pixels) {
            *stamp = if now == was { *stamp } else { self.requests };
        }
    }

    /// Unmixes a landed capture unless it matches the last one (see [`still`]), skipping the unsteady
    /// pixels. Whether the desktop moved, and whether any pixel was skipped.
    fn take(&mut self, known: &mut Vec<u8>, rows: &[u8], size: (u32, u32), stride: usize) -> (bool, bool) {
        let sample: Vec<u8> = rows.iter().copied().step_by(SAMPLE_STEP).collect();
        let skipped = (0..self.changed.len()).any(|pixel| !self.steady(pixel));
        let moved = !still(&sample, &self.sample) && {
            // A skipped pixel must meet the next capture, which must not read as still.
            self.sample = if skipped { Vec::new() } else { sample };
            self.unmix_into(known, rows, size, stride)
        };
        (moved, skipped)
    }

    /// Takes what the read this replaces knew: its paints, and its last capture where the read
    /// lies as it did (a moved one would read as still over a uniform wallpaper).
    pub(super) fn inherit(&mut self, old: Read) {
        let same = (old.clip, old.origin, old.step) == (self.clip, self.origin, self.step) && old.region == self.region;
        (self.presented, self.was, self.requests, self.changed) = (old.presented, old.was, old.requests, old.changed);
        if same {
            self.sample = old.sample;
        } else {
            // A capture in flight for the old region would unmix into the new one.
            self.changed.fill(self.requests);
        }
    }

    /// Unmixes a capture of `size` in `stride`-byte rows into `known`, the desktop at the read's own
    /// size, each read pixel taking the nearest capture pixel. Whether it changed.
    fn unmix_into(&self, known: &mut Vec<u8>, rows: &[u8], (width, height): (u32, u32), stride: usize) -> bool {
        let (w, h) = self.size();
        if self.presented.len() != w * h * 4 || width == 0 || height == 0 || self.region.width <= 0.0 {
            return false;
        }
        if known.len() != w * h * 4 {
            // Stale bytes would outlive the pixels they belonged to.
            known.clear();
            known.resize(w * h * 4, 0);
        }
        let at = |i: usize, origin: f32, per_px: f32, n: u32| {
            (((origin + (i as f32 + 0.5) * self.step) * per_px) as usize).min(n as usize - 1)
        };
        let (per_x, per_y) = (width as f32 / self.region.width, height as f32 / self.region.height);
        let columns: Vec<usize> = (0..w).map(|x| at(x, self.origin.0, per_x, width)).collect();
        let mut changed = false;
        for y in 0..h {
            let captured = rows[at(y, self.origin.1, per_y, height) * stride..].as_chunks::<4>().0;
            let ours = self.presented[(h - 1 - y) * w * 4..][..w * 4].as_chunks::<4>().0;
            let desktop = known[y * w * 4..][..w * 4].as_chunks_mut::<4>().0;
            let row = (h - 1 - y) * w;
            for (i, ((x, f), d)) in columns.iter().zip(ours).zip(desktop).enumerate() {
                changed |= self.steady(row + i) && unmix(&captured[*x], f, d);
            }
        }
        changed
    }
}

/// Unmixes one captured pixel `c` (BGRX) against ours there `f` (premultiplied RGBA) into `d`
/// (premultiplied BGRA, alpha 0 where unknown). Where ours is near opaque nothing behind it shows,
/// so `d` becomes unknown, which a blur weighs as nothing and a reader fills from around; a stale
/// value would freeze. Where the result is not a colour (the capture shows another frame of ours,
/// so the readback does not match it), or it moved within the rounding, `d` keeps its last value.
/// Whether it moved.
fn unmix(c: &[u8; 4], f: &[u8; 4], d: &mut [u8; 4]) -> bool {
    if f[3] >= OPAQUE {
        return std::mem::take(d) != [0; 4];
    }
    let remaining = 1.0 - f32::from(f[3]) / 255.0;
    let margin = 3.0 + 0.5 / remaining;
    let channel = |i: usize| (f32::from(c[i]) - f32::from(f[2 - i])) / remaining;
    let bgr = [channel(0), channel(1), channel(2)];
    if bgr.iter().any(|v| *v < -margin || *v > 255.0 + margin) {
        return false;
    }
    let next = bgr.map(|v| v.clamp(0.0, 255.0) as u8);
    // Rounding is multiplied by 1 / (1 - alpha), and a `d` that followed it would never settle.
    let noise = f32::from(SETTLED) + 2.0 * (1.0 / remaining - 1.0);
    if d[3] != 0 && (0..3).all(|i| f32::from(next[i].abs_diff(d[i])) <= noise) {
        return false;
    }
    *d = [next[0], next[1], next[2], 255];
    true
}

/// `known` in rows of `width` pixels, each unknown pixel (alpha 0) taking the nearest known one by
/// Manhattan distance, a tie taking either: a shader that reads under opaque paint gets a plausible
/// desktop, not black. Two raster passes, each pixel taking its neighbours' pick.
/// ponytail: copies, so a block shows seams where picks from different edges meet and bands where
/// one edge pixel spreads; upgrade path: interpolate between the edges.
fn filled(known: &[u8], width: usize) -> Vec<u8> {
    let mut out = known.to_vec();
    if width == 0 {
        return out;
    }
    // Distance to each pixel's pick; MAX until it has one.
    let mut far: Vec<u32> = known.as_chunks::<4>().0.iter().map(|p| if p[3] == 0 { u32::MAX } else { 0 }).collect();
    if !far.contains(&u32::MAX) {
        return out;
    }
    let height = far.len() / width;
    let mut take = |far: &mut [u32], to: usize, from: usize| {
        if far[from].saturating_add(1) < far[to] {
            far[to] = far[from] + 1;
            out.copy_within(from * 4..from * 4 + 4, to * 4);
        }
    };
    for y in 0..height {
        for x in 0..width {
            let p = y * width + x;
            if far[p] == 0 {
                continue;
            }
            if x > 0 {
                take(&mut far, p, p - 1);
            }
            if y > 0 {
                take(&mut far, p, p - width);
            }
        }
    }
    for y in (0..height).rev() {
        for x in (0..width).rev() {
            let p = y * width + x;
            if far[p] == 0 {
                continue;
            }
            if x + 1 < width {
                take(&mut far, p, p + 1);
            }
            if y + 1 < height {
                take(&mut far, p, p + width);
            }
        }
    }
    out
}

/// Whether `sample` is the last capture's within rounding: nothing is new behind, and unmixing
/// would only wobble the desktop with its own rounding, which repaints.
fn still(sample: &[u8], last: &[u8]) -> bool {
    sample.len() == last.len() && sample.iter().zip(last).all(|(a, b)| a.abs_diff(*b) <= SETTLED)
}

/// Unmixes a desktop source's landed frame, staging the desktop only when it changed: a frame that
/// shows nothing but this surface's own last repaint must not cause another. Whether it changed.
pub(super) fn stage(
    cache: &mut CaptureCache,
    id: NodeId,
    negotiated: &mut NegotiatedBuffer,
    known: &mut Vec<u8>,
    read: &mut Read,
) -> bool {
    let Some((rows, size, stride)) = negotiated.rows() else { return false };
    let (moved, skipped) = read.take(known, rows, size, stride);
    if !moved {
        return skipped;
    }
    let (width, height) = (read.size().0 as u32, read.size().1 as u32);
    cache.stage(
        id,
        PendingFrame {
            has_alpha: true,
            width,
            height,
            y_offset: 0,
            pixels: filled(known, width as usize),
            damage: Vec::new(),
        },
    );
    true
}

impl CaptureRegistry {
    /// Reads back what this paint drew over each placed desktop read in `list`, before its swap:
    /// what the next capture there shows of this surface, which the unmix takes out.
    ///
    /// ponytail: a synchronous `glReadPixels` per paint of such a surface. Upgrade path: a pixel
    /// buffer object read one paint later.
    pub(in crate::wayland) fn read_presented(
        &mut self,
        gl: Option<&glow::Context>,
        list: &DisplayList,
        surface: PhysicalRect,
    ) {
        let (false, Some(gl)) = (self.desktops.is_empty(), gl) else { return };
        let mut reads = Vec::new();
        list.desktop_reads(surface, &mut reads);
        let height = surface.y1;
        for (node, clip) in reads {
            let Some(read) = self.desktops.get_mut(&node).filter(|read| read.clip == clip) else { continue };
            let (width, rows) = read.size();
            std::mem::swap(&mut read.presented, &mut read.was);
            read.presented.resize(width * rows * 4, 0);
            // SAFETY: `paint_surface` made this surface current and `execute` flushed into it;
            // `presented` holds the whole RGBA area.
            unsafe {
                let into = glow::PixelPackData::Slice(Some(&mut read.presented));
                let (width, rows) = (width as i32, rows as i32);
                gl.read_pixels(clip.x0, height - clip.y1, width, rows, glow::RGBA, glow::UNSIGNED_BYTE, into);
            }
            read.note_paint();
        }
    }
}

impl App {
    /// Before `execute`: places each desktop read in surface `index`'s `list` on its output and
    /// names its capture target, so the paint waits for a frame there. A read no capture can serve
    /// is dropped and paints from the surface's own pixels.
    pub(in crate::wayland) fn place_desktop_reads(&mut self, index: usize, list: &DisplayList) {
        let mut reads = Vec::new();
        let tracked = &self.surfaces[index];
        let (width, height) = tracked.scale.physical_size(tracked.configured_size);
        list.desktop_reads(PhysicalRect { x0: 0, y0: 0, x1: width as i32, y1: height as i32 }, &mut reads);
        if reads.is_empty() {
            return;
        }
        let place = self.captures.backend.wlr.as_ref().and_then(|_| self.desktop_place(index));
        for (node, clip) in reads {
            let old = self.captures.desktops.remove(&node);
            let Some((output, (ox, oy), scale, (ow, oh))) = &place else { continue };
            let (x0, y0) = (ox + clip.x0 as f32 / scale, oy + clip.y0 as f32 / scale);
            let (x1, y1) = (ox + clip.x1 as f32 / scale, oy + clip.y1 as f32 / scale);
            let (rx, ry) = (x0.floor().max(0.0), y0.floor().max(0.0));
            let region = LogicalRect { x: rx, y: ry, width: x1.ceil().min(*ow) - rx, height: y1.ceil().min(*oh) - ry };
            if region.width < 1.0 || region.height < 1.0 {
                continue;
            }
            let target = CaptureTarget::Output(output.clone());
            self.capture_cache.set_target(node, target.clone());
            let (origin, step) = ((x0 - rx, y0 - ry), 1.0 / scale);
            let mut read = Read {
                target,
                region,
                clip,
                origin,
                step,
                presented: vec![],
                was: vec![],
                requests: 2,
                changed: vec![],
                sample: vec![],
            };
            if let Some(old) = old {
                read.inherit(old);
            }
            self.captures.desktops.insert(node, read);
        }
    }

    /// Where a desktop read on surface `index` lies. `None` for other roles than `panel`, whose
    /// position no protocol tells a client.
    fn desktop_place(&self, index: usize) -> Option<Place> {
        let surface = &self.surfaces[index];
        let super::super::surface::TrackedRole::Panel { output, layer, spec, .. } = &surface.role else { return None };
        // `output = "active"` names none: the compositor's pick is the output the surface entered.
        let entered = || layer.as_ref()?.wl_surface().data::<SurfaceData<()>>()?.outputs().next();
        let output = output.clone().or_else(entered)?;
        let (index, info) = self
            .output_state
            .outputs()
            .enumerate()
            .find(|(_, candidate)| *candidate == output)
            .and_then(|(index, output)| Some((index, self.output_state.info(&output)?)))?;
        let (width, height) = info.logical_size?;
        let (size, output_size) = (surface.configured_size, (width as f32, height as f32));
        let origin = super::super::layer::layer_origin(spec, (size.0 as f32, size.1 as f32), output_size);
        let scale = surface.scale.factor_120() as f32 / 120.0;
        Some((super::super::output::output_name(index, info.name.as_deref()), origin, scale, output_size))
    }

    /// Adds a capture for each placed desktop read a surface still draws, and drops the rest.
    pub(super) fn desktop_captures(&mut self, wanted: &mut HashMap<NodeId, CaptureNode>) {
        if self.captures.desktops.is_empty() {
            return;
        }
        let mut reads = Vec::new();
        for surface in &self.surfaces {
            if let Some((_, list)) = &surface.last_painted {
                list.desktop_reads(crate::layout::paint::UNCLIPPED, &mut reads);
            }
        }
        self.captures.desktops.retain(|node, _| reads.iter().any(|(read, _)| read == node));
        for (&node, read) in &self.captures.desktops {
            let (target, region, live) = (read.target.clone(), Some(read.region), Some(DESKTOP_FPS));
            wanted.insert(node, CaptureNode { node, target, live, paint_cursor: false, region });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The desktop is BGR (10, 20, 30) under ours, grey, at each alpha around [`OPAQUE`].
    #[test]
    fn the_desktop_is_unmixed_where_the_surface_lets_it_show() {
        for (alpha, seen) in [(0, true), (128, true), (234, true), (235, false), (255, false)] {
            let grey = alpha / 2;
            let rest = 1.0 - f32::from(alpha) / 255.0;
            let c = [10.0, 20.0, 30.0].map(|v: f32| (f32::from(grey) + v * rest).round() as u8);
            let mut d = [0; 4];
            assert_eq!(unmix(&[c[0], c[1], c[2], 255], &[grey, grey, grey, alpha], &mut d), seen, "alpha {alpha}");
            if seen {
                // Rounding the capture to a byte is amplified by 1 / (1 - alpha).
                let slack = (0.5 / rest).ceil() as u8 + 1;
                assert!(d.iter().zip([10, 20, 30, 255]).all(|(a, b)| a.abs_diff(b) <= slack), "{alpha}: {d:?}");
            } else {
                assert_eq!(d, [0; 4], "alpha {alpha}: nothing behind it shows");
            }
        }
    }

    #[test]
    fn an_opaque_block_and_unknown_rows_fill_from_the_nearest_known_pixel_in_2d() {
        // A known gradient 12x6 with rows 0 and 5 unknown and an opaque block over x 2..10, y 2..4.
        let px = |x: u8, y: u8| [x, y, 7, 255];
        let block = |x: usize, y: usize| y == 0 || y == 5 || ((2..10).contains(&x) && (2..4).contains(&y));
        let known: Vec<u8> = (0..72)
            .flat_map(|p| if block(p % 12, p / 12) { [0; 4] } else { px((p % 12) as u8, (p / 12) as u8) })
            .collect();
        let out = filled(&known, 12);
        let row = |y: usize| out[y * 48..][..48].to_vec();
        // Not a streak of each edge pixel: away from the block's ends, where an edge column is as
        // near as a row, it copies the rows above and below it.
        let middle = [px(3, 1), px(4, 1), px(5, 1), px(6, 1), px(7, 1), px(8, 1)];
        assert_eq!(row(2)[12..36], middle.concat());
        let middle = [px(3, 4), px(4, 4), px(5, 4), px(6, 4), px(7, 4), px(8, 4)];
        assert_eq!(row(3)[12..36], middle.concat());
        assert_eq!(row(0), row(1), "a row with no known pixel copies the one below");
        assert_eq!(row(5), row(4), "and above");
    }

    #[test]
    fn a_pixel_under_an_opaque_one_becomes_unknown_once() {
        let mut d = [10, 20, 30, 255];
        assert!(unmix(&[1, 2, 3, 255], &[9, 9, 9, 255], &mut d), "known to unknown is a change");
        assert_eq!(d, [0; 4]);
        assert!(!unmix(&[1, 2, 3, 255], &[9, 9, 9, 255], &mut d), "and only once");
        assert!(unmix(&[10, 20, 30, 255], &[0; 4], &mut d), "clear again, it is seen again");
    }

    /// A one-row read of `width` pixels over a capture the same size.
    fn bare(presented: Vec<u8>, width: usize) -> Read {
        Read {
            target: CaptureTarget::Output("DP-1".into()),
            region: LogicalRect { x: 0.0, y: 0.0, width: width as f32, height: 1.0 },
            clip: PhysicalRect { x0: 0, y0: 0, x1: width as i32, y1: 1 },
            origin: (0.0, 0.0),
            step: 1.0,
            presented,
            was: Vec::new(),
            requests: 2,
            changed: vec![0; width],
            sample: Vec::new(),
        }
    }

    /// A bending shader over a desktop that never moves, behind text of every alpha, through the real
    /// gates: each paint presents what the last unmix gave (the desktop one pixel over, held to 88%),
    /// the capture shows it a few levels off, the same each time, and the unmix must stop changing.
    #[test]
    fn captures_settle_over_a_still_desktop_under_a_bending_shader() {
        let desktop = [30u8, 200, 90, 255, 10, 120, 60, 180];
        let text = [0u8, 0, 0, 0, 128, 200, 255, 0];
        let mut read = bare(vec![0; 32], 8);
        let mut known = vec![0u8; 32];
        let mut changes = Vec::new();
        for _ in 0..16 {
            let seen = known.clone();
            let (mut f, mut rows) = (vec![0u8; 32], vec![255u8; 32]);
            for i in 0..8 {
                let next = &seen[(i + 1) % 8 * 4..][..4];
                let shown = if next[3] == 0 { 0.0 } else { 0.88 };
                let t = f32::from(text[i]) / 255.0;
                let (a, v) = (t + (1.0 - t) * shown, (1.0 - t) * shown * f32::from(next[0]) / 255.0);
                let byte = |v: f32| (v * 255.0).round() as u8;
                let c = (i32::from(byte(v + (1.0 - a) * f32::from(desktop[i]) / 255.0)) + (i % 3) as i32 - 1) as u8;
                f[i * 4..][..4].copy_from_slice(&[byte(v), byte(v), byte(v), byte(a)]);
                rows[i * 4..][..3].fill(c);
            }
            read.was = std::mem::replace(&mut read.presented, f);
            read.note_paint();
            read.requests += 1;
            changes.push(read.take(&mut known, &rows, (8, 1), 32).0);
        }
        assert!(changes[10..].iter().all(|moved| !moved), "still changing: {changes:?}");
    }

    /// A pixel is unmixed once its presented value has outlasted one request: one that changes
    /// between every two requests is, one that changes before every request is not; a skipped pixel
    /// is unmixed by the next capture, and never reads as still, even of identical rows.
    #[test]
    fn a_pixel_is_unmixed_once_it_has_outlasted_a_request() {
        for (every, unmixed) in [(2, true), (1, false)] {
            let mut read = bare(vec![0; 4], 1);
            let mut known = vec![0u8; 4];
            for round in 0..8u8 {
                if round % every == 0 {
                    read.was = std::mem::replace(&mut read.presented, vec![0, 0, 0, 40 + round % 3 * 40]);
                    read.note_paint();
                }
                read.requests += 1;
                let c = (100.0 * f32::from(255 - read.presented[3]) / 255.0) as u8;
                read.take(&mut known, &[c, c, c, 255], (1, 1), 4);
            }
            assert_eq!(known[0].abs_diff(100) <= 3, unmixed, "every {every}: {known:?}");
        }
        let mut read = bare(vec![0; 8], 2);
        read.changed = vec![2, 0];
        let (mut known, rows) = (vec![0u8; 8], [50, 50, 50, 255].repeat(2));
        assert_eq!(read.take(&mut known, &rows, (2, 1), 8), (true, true));
        assert_eq!(known, [0, 0, 0, 0, 50, 50, 50, 255], "the unsteady one is left alone");
        read.requests += 2;
        assert_eq!(read.take(&mut known, &rows, (2, 1), 8), (true, false), "not still while one was skipped");
        assert_eq!(known[..4], [50, 50, 50, 255]);
        assert_eq!(read.take(&mut known, &rows, (2, 1), 8), (false, false), "the same rows are still");
    }

    #[test]
    fn a_paint_of_another_size_starts_every_pixel_over() {
        let mut read = bare(vec![1; 8], 2);
        read.was = vec![1; 4];
        read.note_paint();
        assert_eq!(read.changed, [2, 2], "unsteady until a request has passed");
    }

    #[test]
    fn a_replaced_read_keeps_its_paints_and_its_sample_only_where_it_lies_the_same() {
        let old = || Read { changed: vec![9], sample: vec![5], ..bare(vec![7; 4], 1) };
        let mut same = bare(vec![], 1);
        same.inherit(old());
        assert_eq!((same.presented, same.changed, same.sample), (vec![7; 4], vec![9], vec![5]));
        let moved: [fn(&mut Read); 4] =
            [|r| r.origin = (0.5, 0.0), |r| r.step = 2.0, |r| r.clip.x1 = 2, |r| r.region.width = 2.0];
        for change in moved {
            let mut read = bare(vec![], 1);
            change(&mut read);
            read.inherit(old());
            assert_eq!((read.presented.len(), read.sample.len()), (4, 0), "a moved read starts its sample over");
            assert_eq!(read.changed, [2], "and every pixel unsteady");
        }
    }

    /// A capture of another frame of ours, or rounding noise, must not feed a repaint loop.
    #[test]
    fn an_impossible_or_rounding_only_change_keeps_the_last_desktop() {
        let clear = [0, 0, 0, 0];
        let mut d = [10, 20, 30, 255];
        // Ours is half black; a desktop of 10 would capture as 5, 250 cannot come from it.
        assert!(!unmix(&[250, 10, 15, 255], &[0, 0, 0, 128], &mut d));
        assert_eq!(d, [10, 20, 30, 255]);
        assert!(!unmix(&[13, 23, 33, 255], &clear, &mut d), "3 is within rounding");
        assert!(unmix(&[17, 27, 37, 255], &clear, &mut d), "4 past the last is a change");
        // Half black over: 13 reads as 26, so a move of 5 levels is noise and 6 is not.
        assert!(!unmix(&[13, 13, 13, 255], &[0, 0, 0, 128], &mut [21, 21, 21, 255]));
        assert!(unmix(&[13, 13, 13, 255], &[0, 0, 0, 128], &mut [20, 20, 20, 255]));
    }

    /// A 2x1 read at scale 1.5 is 1.33x0.67 logical px, out to a 2x1 region the compositor returns
    /// at 3x2 buffer px in 16-byte rows; ours is clear on the left and opaque on the right.
    #[test]
    fn a_capture_of_another_size_or_stride_maps_onto_the_read() {
        let mut read = Read { origin: (0.2, 0.1), step: 1.0 / 1.5, ..bare(vec![0, 0, 0, 0, 255, 255, 255, 255], 2) };
        let mut rows = vec![0; 32];
        for (x, value) in [(0, 40), (1, 80), (2, 120)] {
            for row in [0, 16] {
                rows[row + x * 4..row + x * 4 + 4].copy_from_slice(&[value, value, value, 255]);
            }
        }
        let mut known = vec![9; 64];
        assert!(read.unmix_into(&mut known, &rows, (3, 2), 16));
        assert_eq!(known, [40, 40, 40, 255, 0, 0, 0, 0], "a stale size is cleared, not kept under opaque");
        assert!(!read.unmix_into(&mut known, &rows, (3, 2), 16), "the same capture settles");
        let presented = std::mem::take(&mut read.presented);
        assert!(!read.unmix_into(&mut known, &rows, (3, 2), 16), "nothing presented yet");
        read.presented = presented.iter().map(|_| 0).collect();
        read.step = 2.0;
        assert!(read.unmix_into(&mut known, &rows, (3, 2), 16), "past the capture clamps to its edge");
        assert_eq!(known[4..], [120, 120, 120, 255]);
    }

    /// Each request counts toward the read's steadiness; a landed capture that moved the desktop
    /// keeps the source's rate, and one that shows nothing new drops it to the idle rate.
    #[test]
    fn a_desktop_source_counts_requests_and_idles_once_a_capture_shows_no_change() {
        use super::super::tests::{ID, Wire, id};
        let region = Some(LogicalRect { x: 0.0, y: 0.0, width: 2.0, height: 1.0 });
        let target = CaptureTarget::Output("TEST".into());
        let node = CaptureNode { node: ID, target, live: Some(f32::INFINITY), paint_cursor: false, region };
        let mut w = Wire::with(Protocol::Wlr, &node);
        w.app.captures.desktops.insert(ID, bare(vec![0; 8], 2));
        w.source_mut().desktop = Some(Vec::new());
        let land = |w: &mut Wire, pixels: Option<[u8; 8]>| {
            let frame = id(w.wlr().frame.as_ref());
            // wlr `buffer` (XRGB 2x1, 8-byte rows), `buffer_done`, then `ready`.
            w.send(frame, 0, &[wl_shm::Format::Xrgb8888 as u32, 2, 1, 8]);
            w.send(frame, 6, &[]);
            if let (Some(pixels), Some(Proto::Wlr(wlr))) = (pixels, &mut w.source_mut().proto) {
                wlr.buffer.as_mut().unwrap().canvas_for_test()[..8].copy_from_slice(&pixels);
            }
            w.send(frame, 2, &[0, 0, 0]);
        };
        w.app.request_frame(ID);
        w.settle();
        assert_eq!(w.app.captures.desktops[&ID].requests, 3);
        land(&mut w, Some([10, 20, 30, 255, 40, 50, 60, 255]));
        assert_eq!(w.source().live, Some(f32::INFINITY), "the desktop moved");
        assert_eq!(w.app.capture_cache.poll(), [ID]);
        assert_eq!(w.app.captures.desktops[&ID].requests, 4, "the next request");
        land(&mut w, None);
        assert_eq!(w.source().live, Some(DESKTOP_IDLE_FPS), "the same pixels again");
        assert!(w.app.capture_cache.poll().is_empty(), "stage nothing");
    }
}
