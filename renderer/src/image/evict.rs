//! Image cache capacity bound, idle texture budget eviction, and stand-in texture selection
//! (ADR-0123, ADR-0182, ADR-0183).

use std::collections::HashSet;

use femtovg::renderer::OpenGl;
use femtovg::{Canvas, ImageId};

use super::{CACHE_CAPACITY, CacheKey, Entry, ImageCache, ImageRequest, Slot};

impl ImageCache {
    /// Holds idle textures to `budget` bytes from here on (ADR-0182). Called whenever the outputs
    /// change, which is the only thing that changes the answer.
    pub fn set_texture_budget(&mut self, budget: usize) {
        self.texture_budget = budget;
    }

    /// Frees last frame's evictions. `layout::paint::canvas::paint_tree` calls this before walking because
    /// femtovg resolves `ImageId` at `flush`, not `fill_path`; mid-walk deletion unbinds a texture
    /// a recorded command still names, drawing blank.
    pub fn release_evicted(&mut self, canvas: &mut Canvas<OpenGl>) {
        self.begin_paint();
        for id in self.evicted.drain(..) {
            canvas.delete_image(id);
        }
    }

    /// Whether a texture was evicted since the last call, which clears it: the cue for the loop to
    /// hand the freed host bytes back (`wayland::trim`).
    pub fn take_freed(&mut self) -> bool {
        std::mem::take(&mut self.freed)
    }

    /// Marks what is asked for from here as this paint's, which the capacity bound never evicts.
    fn begin_paint(&mut self) {
        self.paint_start = self.tick;
    }

    /// Evicts idle textures to [`ImageCache::set_texture_budget`], oldest ask first (ADR-0123). The
    /// early return over-estimates on purpose -- idle bytes never exceed resident -- so an untouched
    /// cache skips the walk and [`victims`] measures the idle half. `pinned` contains the image
    /// requests from mapped surfaces' last display lists, collected by `wayland::App`
    /// after paint. Compute it lazily because list walks matter only when evicting. Icons are not
    /// pinned: lists carry theme names, not paths; an icon costs a few KB and one inline reraster.
    /// Node textures no list draws are freed here too, whatever the budget. Pending decodes no
    /// list names are cancelled the same way, whatever the budget.
    ///
    /// ponytail: so while any node texture exists, every paint walks every painted list. Upgrade:
    /// skip the walk when the lists and the node set are unchanged since the last one.
    pub fn trim<'a>(&mut self, pinned: impl FnOnce() -> Vec<ImageRequest<'a>>) {
        let over = self.resident_bytes > self.texture_budget;
        let pending = self.entries.values().any(|entry| matches!(entry.slot, Slot::Pending));
        if !over && !pending && !self.clocks.values().flatten().any(|own| own.texture.is_some()) {
            return;
        }
        let mut asked = HashSet::new();
        let pinned = pinned()
            .into_iter()
            .map(|request| {
                let wanted = self.key_for(&request);
                asked.insert(wanted.clone());
                let drawn = if self
                    .entries
                    .get(&wanted)
                    .is_some_and(|entry| matches!(entry.slot, Slot::Ready { .. } | Slot::Failed))
                {
                    wanted
                } else {
                    self.stand_in_key(&wanted).unwrap_or(wanted)
                };
                (request.clock.map(|(node, _)| node), drawn)
            })
            .collect::<Vec<_>>();
        // A decode no mapped surface names is scrolled off, hidden or replaced; one asked since
        // `paint_start` belongs to a surface that has not recorded its list yet (ADR-0185).
        let stale = self
            .entries
            .iter()
            .filter(|(key, entry)| {
                matches!(entry.slot, Slot::Pending) && entry.last_hit <= self.paint_start && !asked.contains(*key)
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        stale.iter().for_each(|key| self.evict(key));
        self.release_undrawn_node_textures(&pinned);
        if !over {
            return;
        }
        let pinned = pinned.into_iter().map(|(_, key)| key).collect::<Vec<_>>();
        let candidates = self.entries.iter().filter_map(|(key, entry)| match entry.slot {
            Slot::Ready { bytes, .. } => Some((key.clone(), bytes, entry.last_hit)),
            Slot::Pending | Slot::Failed => None,
        });
        for key in victims(candidates, self.texture_budget, &pinned) {
            self.evict(&key);
        }
    }

    pub(super) fn stand_in_key(&self, wanted: &CacheKey) -> Option<CacheKey> {
        if wanted.blur_px != 0 {
            return None;
        }
        self.entries
            .iter()
            .filter_map(|(key, entry)| {
                (matches!(entry.slot, Slot::Ready { .. }) && compatible_stand_in(wanted, key)).then_some(key)
            })
            .max_by_key(|key| (u64::from(key.box_px.0) * u64::from(key.box_px.1), key.box_px))
            .cloned()
    }

    /// A ready texture at another physical box while a background decode catches up. Its fitted
    /// rect still uses the new logical box; the caller must not treat it as an exact source hit.
    pub(crate) fn stand_in(&mut self, canvas: &mut Canvas<OpenGl>, request: &ImageRequest) -> Option<ImageId> {
        let wanted = self.key_for(request);
        if self.entries.get(&wanted).is_some_and(|entry| matches!(entry.slot, Slot::Failed)) {
            return None;
        }
        let key = self.stand_in_key(&wanted)?;
        self.tick += 1;
        self.entries.get_mut(&key)?.last_hit = self.tick;
        self.showing(canvas, &key, request.clock)
    }

    /// Evicts before inserting, keeping the map within [`CACHE_CAPACITY`]. Queue textures for
    /// [`ImageCache::release_evicted`]: femtovg frees nothing by `ImageId` until told to, so losing
    /// the id leaks the GPU allocation permanently.
    pub(super) fn insert(&mut self, key: CacheKey, slot: Slot) -> &mut Entry {
        // Its own tick, so an insert that did not come through `image` still orders after every
        // earlier one and `last_hit` never ties. That makes the scan below fall back to insertion
        // order exactly where nothing has been asked for twice.
        self.tick += 1;
        while self.entries.len() >= CACHE_CAPACITY {
            // Least recently *asked for*, not oldest inserted (ADR-0183). This bound is the one
            // eviction path that never sees the pin list, and the oldest insert is typically the
            // wallpaper: put up first and asked for on every frame since, so a picker filling the
            // map evicted the one texture certain to be on screen. `image` bumps `last_hit` on
            // every hit, so whatever a mapped surface drew this frame is the newest thing here.
            //
            // ponytail: a linear scan of at most `CACHE_CAPACITY` entries, on the insert that hits
            // the bound and not on the others. A heap would order it in log time and would have to
            // be reordered on every hit, which is the common case; this is the rarer one.
            //
            // Never one this paint asked for: with more images visible than the cap, evicting them
            // re-decodes every one on each repaint. The map exceeds the cap until they go unused;
            // `trim` still bounds the bytes.
            let Some(coldest) = self
                .entries
                .iter()
                .filter(|(_, entry)| entry.last_hit <= self.paint_start)
                .min_by_key(|(_, entry)| entry.last_hit)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.evict(&coldest);
        }
        match &slot {
            Slot::Ready { bytes, .. } => self.resident_bytes += *bytes,
            Slot::Failed => self.failed_total += 1,
            Slot::Pending => {}
        }
        self.entries.entry(key).insert_entry(Entry { slot, last_hit: self.tick, text: false, sized: false }).into_mut()
    }

    /// Drops one entry, queues its texture for [`ImageCache::release_evicted`], and subtracts its
    /// bytes.
    pub(super) fn evict(&mut self, key: &CacheKey) {
        if let Some(entry) = self.entries.remove(key) {
            if !self.entries.keys().any(|other| other.path == key.path) {
                self.versions.forget(&key.path);
            }
            match &entry.slot {
                Slot::Ready { image, bytes, .. } => {
                    self.evicted.extend(*image);
                    self.evicted_total += 1;
                    self.freed = true;
                    self.resident_bytes -= *bytes;
                }
                // Its decode is still queued or running; stop it being spent on a slot that has
                // just gone away. Say so: a surface may be showing a `retain` cover while it waits
                // for exactly this file, and cancelling in silence leaves it waiting for a result
                // no worker will ever send (ADR-0185).
                Slot::Pending => {
                    self.unwant(key);
                    self.cancelled.push(key.path.clone());
                }
                Slot::Failed => {}
            }
        }
    }
}

/// Evictions from `(key, bytes, last_hit)` to bring the *idle* bytes under `budget`: oldest ask
/// first, skip `pinned` path/box pairs, stop at budget or when unpinned candidates end. Pure so the
/// policy is testable without an `ImageId`, which femtovg cannot make outside a canvas.
///
/// Idle, not total: those are the only bytes eviction returns, so charging a pin against the budget
/// would evict every idle entry on the way to a figure the pin alone already exceeds.
fn victims(
    candidates: impl Iterator<Item = (CacheKey, usize, u64)>,
    budget: usize,
    pinned: &[CacheKey],
) -> Vec<CacheKey> {
    let mut idle: Vec<(CacheKey, usize, u64)> = candidates.filter(|(key, _, _)| !pinned.contains(key)).collect();
    idle.sort_by_key(|(_, _, last_hit)| *last_hit);
    let mut resident: usize = idle.iter().map(|(_, bytes, _)| *bytes).sum();
    let mut out = Vec::new();
    for (key, bytes, _) in idle {
        if resident <= budget {
            break;
        }
        resident -= bytes;
        out.push(key);
    }
    out
}

fn compatible_stand_in(wanted: &CacheKey, candidate: &CacheKey) -> bool {
    if wanted.path != candidate.path
        || wanted.version != candidate.version
        || wanted.tint != candidate.tint
        || wanted.cropped != candidate.cropped
        || wanted.blur_px != candidate.blur_px
        || wanted.font_generation != candidate.font_generation
        || wanted.box_px == candidate.box_px
    {
        return false;
    }
    if !wanted.cropped {
        return true;
    }
    let ((a, b), (c, d)) = (wanted.box_px, candidate.box_px);
    let (a, b, c, d) = (u128::from(a), u128::from(b), u128::from(c), u128::from(d));
    2 * (a * d).abs_diff(b * c) <= a + b + c + d + 3
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::image::tests::{PIL_2X2_RGBA_PNG, key};
    use crate::image::texture::Clock;
    use crate::image::{FileVersion, Fit, Load, Loaded};

    #[test]
    fn trim_cancels_pending_decodes_no_list_names_and_reports_them() {
        let mut cache = ImageCache::inline();
        let request = |path: &'static str| ImageRequest {
            path: Path::new(path),
            box_px: (8, 8),
            tint: None,
            fit: Fit::Contain,
            blur_px: 0,
            wants_size: false,
            clock: None,
        };
        let [shown, gone, fresh] = ["/shown.png", "/gone.png", "/fresh.png"].map(|path| cache.key_for(&request(path)));
        for key in [&shown, &gone] {
            assert!(cache.admit(key));
            cache.insert(key.clone(), Slot::Pending);
        }
        cache.begin_paint();
        assert!(cache.admit(&fresh));
        cache.insert(fresh.clone(), Slot::Pending);

        cache.trim(|| vec![request("/shown.png")]);

        let wanted = cache.pool.wanted.lock().unwrap().clone();
        assert!(wanted.contains(&shown) && wanted.contains(&fresh), "named, or asked this paint: kept");
        assert!(!wanted.contains(&gone), "a worker must skip what no list names");
        assert!(!cache.entries.contains_key(&gone) && cache.entries.contains_key(&shown));
        assert_eq!(cache.poll(), vec![gone.path.clone()], "the cancellation owes its lists a repaint (ADR-0185)");
    }

    #[test]
    fn a_temporary_texture_keeps_source_identity_and_crop_geometry() {
        let wanted = CacheKey { box_px: (300, 150), cropped: true, ..key("/x.png", 18, FileVersion::default()) };
        let half = CacheKey { box_px: (150, 75), ..wanted.clone() };
        assert!(compatible_stand_in(&wanted, &half));
        assert!(!compatible_stand_in(&wanted, &CacheKey { box_px: (150, 150), ..half.clone() }));
        assert!(!compatible_stand_in(&wanted, &CacheKey { tint: Some(0xff00ff), ..half.clone() }));
        assert!(!compatible_stand_in(&wanted, &CacheKey { blur_px: 2, ..half.clone() }));
        assert!(!compatible_stand_in(&wanted, &CacheKey { cropped: false, ..half.clone() }));
        assert!(!compatible_stand_in(&wanted, &CacheKey { path: "/other.png".into(), ..half.clone() }));
        assert!(!compatible_stand_in(
            &wanted,
            &CacheKey { version: FileVersion { len: 1, ..FileVersion::default() }, ..half.clone() }
        ));
        let uncropped = CacheKey { cropped: false, ..wanted };
        assert!(compatible_stand_in(&uncropped, &CacheKey { cropped: false, ..half }));
    }

    #[test]
    fn a_pending_size_can_draw_a_ready_size_until_its_decode_lands() {
        use crate::layout::paint::{init_headless_egl, text_painter};
        use crate::text::shaping::ShapingHandle;

        let Some(instance) = init_headless_egl(64, 64) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 64, 64) else { return };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("picture.png");
        std::fs::write(&path, PIL_2X2_RGBA_PNG).unwrap();
        let mut cache = ImageCache::new();
        if cache.pool.workers == 0 {
            return;
        }
        let small = ImageRequest {
            path: &path,
            box_px: (16, 16),
            tint: None,
            fit: Fit::Contain,
            blur_px: 0,
            wants_size: false,
            clock: None,
        };
        let large = ImageRequest { box_px: (32, 32), ..small };
        let first = cache.image(painter.canvas_mut(), &small, Load::Inline).unwrap();
        assert!(cache.image(painter.canvas_mut(), &large, Load::Background).is_none());
        assert_eq!(cache.stand_in(painter.canvas_mut(), &large), Some(first));
        assert!(cache.stand_in(painter.canvas_mut(), &ImageRequest { fit: Fit::Cover, ..large }).is_none());
        // `image_status`: the file's own size once decoded; a stand-in is not this size's load.
        assert_eq!(cache.loaded(&small), Some(Loaded::Ready(Some((2, 2)))));
        // Decoded without its size, as a thumbnail hit nobody asked the size of: a `status` asks.
        let small_key = cache.key_for(&small);
        if let Some(Slot::Ready { natural, .. }) = cache.entries.get_mut(&small_key).map(|entry| &mut entry.slot) {
            *natural = None;
        }
        let sized = ImageRequest { wants_size: true, ..small };
        let deadline = Instant::now() + Duration::from_secs(5);
        while cache.loaded(&sized) != Some(Loaded::Ready(Some((2, 2)))) {
            assert!(Instant::now() < deadline, "the header read never landed");
            cache.poll();
            std::thread::yield_now();
        }
        assert_eq!(cache.loaded(&large), None);
        let missing = dir.path().join("missing.png");
        let gone = ImageRequest { path: &missing, ..small };
        assert!(cache.image(painter.canvas_mut(), &gone, Load::Inline).is_none());
        assert_eq!(cache.loaded(&gone), Some(Loaded::Failed));
    }

    #[test]
    fn evicting_a_texture_asks_for_a_trim_once_and_evicting_a_failure_does_not() {
        use crate::layout::paint::{init_headless_egl, text_painter};
        use crate::text::shaping::ShapingHandle;

        let Some(instance) = init_headless_egl(8, 8) else { return };
        let shaping = ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 8, 8) else { return };
        let image =
            painter.canvas_mut().create_image_empty(1, 1, femtovg::PixelFormat::Rgba8, femtovg::ImageFlags::empty());
        let v = FileVersion::default();
        let (ready, failed) = (key("/w/ready.png", 8, v), key("/w/failed.png", 8, v));
        let mut cache = ImageCache::new();
        cache.insert(
            ready.clone(),
            Slot::Ready { image: image.ok(), bytes: 4, clock: Clock::default(), anim: None, natural: None },
        );
        cache.insert(failed.clone(), Slot::Failed);
        cache.evict(&failed);
        assert!(!cache.take_freed(), "a failed slot held no memory");
        cache.evict(&ready);
        assert!(cache.take_freed());
        assert!(!cache.take_freed(), "taking clears it");
    }

    /// ADR-0183. The capacity bound never sees the pin list, so it evicts by what has been asked
    /// for least recently rather than by what was inserted first -- otherwise the entry most
    /// certain to be on screen, the wallpaper put up before everything else, is the first to go
    /// when a picker fills the map.
    #[test]
    fn the_capacity_bound_evicts_the_coldest_entry_and_not_the_oldest_one() {
        // Negative entries only: `ImageId` has no public constructor.
        let mut cache = ImageCache::new();
        let first = key("/tmp/0.png", 0, FileVersion::default());
        for n in 0..CACHE_CAPACITY {
            cache.insert(key(format!("/tmp/{n}.png"), 0, FileVersion::default()), Slot::Failed);
        }

        // Asked for again, the way a mapped surface asks for what it draws every frame.
        cache.begin_paint();
        cache.tick += 1;
        let hit = cache.tick;
        cache.entries.get_mut(&first).unwrap().last_hit = hit;

        // Ten more at the bound, so ten entries have to go.
        for n in CACHE_CAPACITY..(CACHE_CAPACITY + 10) {
            cache.insert(key(format!("/tmp/{n}.png"), 0, FileVersion::default()), Slot::Failed);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
        assert_eq!(
            cache.census().failed,
            CACHE_CAPACITY + 10,
            "eviction frees a failed slot, it does not un-fail the load"
        );
        let newest = key(format!("/tmp/{}.png", CACHE_CAPACITY + 9), 0, FileVersion::default());
        assert!(cache.entries.contains_key(&newest));
        assert!(cache.entries.contains_key(&first), "the oldest insert survives, because it is still being asked for");
        for n in 1..=10 {
            assert!(
                !cache.entries.contains_key(&key(format!("/tmp/{n}.png"), 0, FileVersion::default())),
                "the ten coldest go instead, /tmp/{n}.png among them"
            );
        }
    }

    /// A repaint that draws more images than the cap must not evict what it just drew, or every
    /// repaint re-decodes the overflow. Ready slots need a canvas, so `Failed` ones stand in.
    #[test]
    fn a_paint_never_evicts_what_it_drew_and_unused_entries_shrink_back_to_the_cap() {
        let visible = CACHE_CAPACITY + 40;
        let k = |n: usize| key(format!("/tmp/{n}.png"), 0, FileVersion::default());
        let mut cache = ImageCache::new();
        for frame in 0..4 {
            cache.begin_paint();
            for n in 0..visible {
                if cache.entries.contains_key(&k(n)) {
                    cache.tick += 1;
                    let hit = cache.tick;
                    cache.entries.get_mut(&k(n)).unwrap().last_hit = hit;
                } else {
                    cache.insert(k(n), Slot::Failed);
                }
            }
            assert_eq!(cache.entries.len(), visible, "frame {frame}: the visible set stays whole");
            // Failed slots free nothing, so re-decodes show as re-inserts.
            assert_eq!(cache.census().failed, visible, "frame {frame}: nothing was evicted and re-inserted");
        }
        // A different set replaces it: the old one is unused, so the map returns to the cap.
        cache.begin_paint();
        for n in visible..visible + CACHE_CAPACITY {
            cache.insert(k(n), Slot::Failed);
        }
        assert_eq!(cache.entries.len(), CACHE_CAPACITY);
        assert!((visible..visible + CACHE_CAPACITY).all(|n| cache.entries.contains_key(&k(n))));
    }

    #[test]
    fn victims_evict_idle_textures_oldest_first_never_pinned_ones_and_a_pin_is_by_path_and_box() {
        {
            // Three 12 MB wallpapers plus tiles: shown is pinned, tiles were asked after the older
            // wallpaper, so the older wallpaper goes first without touching tiles.
            let mb = 1 << 20;
            let v = FileVersion::default();
            let old = key("/w/old.jpg", 1920, v);
            let older = key("/w/older.jpg", 1920, v);
            let shown = key("/w/shown.jpg", 1920, v);
            let tiles = key("/w/tiles.png", 232, v);
            let candidates = vec![
                (older.clone(), 12 * mb, 1),
                (old.clone(), 12 * mb, 2),
                (tiles.clone(), 7 * mb, 3),
                (shown.clone(), 12 * mb, 4),
            ];
            let pinned = vec![shown.clone()];
            // 31 MB idle against a 19 MB budget: the oldest goes and the rest stay.
            let out = victims(candidates.clone().into_iter(), 19 * mb, &pinned);
            assert_eq!(out, vec![older.clone()]);
            // A tighter budget takes the next oldest, then the tiles, and stops at pinned even while
            // over budget: oversized working sets do not thrash.
            let out = victims(candidates.clone().into_iter(), 5 * mb, &pinned);
            assert_eq!(out, vec![older, old, tiles]);
            // Idle already under budget, nothing moves -- and the pinned 12 MB is not charged for.
            assert!(victims(candidates.into_iter(), 31 * mb, &pinned).is_empty());
        }

        {
            // A cover wallpaper is one screenful and so is the budget on a 3440x1440 output. If the pin
            // counted, every icon would go and the cache would still be over, for nothing back.
            let mb = 1 << 20;
            let v = FileVersion::default();
            let budget = 19 * mb;
            let mut candidates = vec![(key("/w/shown.jpg", 3440, v), budget, 99)];
            candidates.extend((0..11).map(|n| (key(format!("/i/{n}.png"), 32, v), 4 << 10, n)));
            let pinned = vec![candidates[0].0.clone()];

            assert!(
                victims(candidates.into_iter(), budget, &pinned).is_empty(),
                "44 KB of idle icons beside a pin that fills the budget is not a reason to evict any of them"
            );
        }

        {
            let v = FileVersion::default();
            let full = key("/w/a.jpg", 1920, v);
            let tile = key("/w/a.jpg", 232, v);
            let pinned = vec![full.clone()];
            let out = victims(vec![(full.clone(), 10, 1), (tile.clone(), 10, 2)].into_iter(), 5, &pinned);
            assert_eq!(out, vec![tile]);
        }
    }

    #[test]
    fn trim_under_budget_never_asks_for_the_pins() {
        ImageCache::new().trim(|| unreachable!("nothing resident, nothing to walk"));
    }
}
