use std::path::Path;
use std::time::{Duration, Instant};

use femtovg::renderer::OpenGl;
use femtovg::rgb::FromSlice;
use femtovg::{Canvas, ErrorKind, ImageFlags, ImageId, ImageSource};
use shared::warn;

use super::decode::premultiply;
use super::{CacheKey, Decoded, GifDelta, ImageCache, Slot};
use crate::layout::scene::NodeId;

/// A landed GIF's replay state (ADR-0235): the base frame's own pixels, kept to re-upload on loop
/// wrap, and the rects later frames change, applied to the live texture in [`ImageCache::showing`]
/// as its shown index advances.
pub(super) struct Animation {
    base: Vec<u8>,
    width: u32,
    height: u32,
    delays: Vec<Duration>,
    deltas: Vec<GifDelta>,
    /// The frame index on the slot's texture.
    current: usize,
}

/// Uploads a decode or warns once per path when filling its slot; `Failed` stops the next frame
/// asking again.
pub(super) fn upload_or_log(canvas: &mut Canvas<OpenGl>, path: &Path, decoded: Result<Decoded, String>) -> Slot {
    let result = decoded.and_then(|decoded| {
        // Host delta bytes counted beside the texture's, or nothing reclaims them: `trim` is the
        // only thing that drops an entry, a shown source is pinned against it either way, and an
        // animation nobody draws held its whole ceiling for the life of the process (ADR-0235).
        let bytes = decoded.base.len() + decoded.deltas.iter().map(|delta| delta.pixels.len()).sum::<usize>();
        let natural = decoded.natural;
        upload(canvas, decoded).map(|(image, anim)| Slot::Ready {
            image,
            bytes,
            clock: Clock::default(),
            anim,
            natural,
        })
    });
    match result {
        Ok(slot) => slot,
        Err(err) => {
            if super::first_failure(&path.to_string_lossy()) {
                warn!("{}: {err}", path.display());
            }
            Slot::Failed
        }
    }
}

/// An animation's place in its loop: running since `since`, or held at `held` while `image.playing`
/// is false. Starts held at zero, so a source first drawn paused shows its first frame.
#[derive(Default)]
pub(super) struct Clock {
    pub(super) since: Option<Instant>,
    pub(super) held: Duration,
}

impl Clock {
    /// Time into the animation at `now`, holding or resuming as `playing` asks.
    pub(super) fn elapsed(&mut self, playing: bool, now: Instant) -> Duration {
        match (self.since, playing) {
            (Some(since), true) => now.saturating_duration_since(since),
            (Some(since), false) => {
                self.held = now.saturating_duration_since(since);
                self.since = None;
                self.held
            }
            (None, true) => {
                self.since = Some(now.checked_sub(self.held).unwrap_or(now));
                self.held
            }
            (None, false) => self.held,
        }
    }
}

/// The frame showing `elapsed` after the first, and how long until the next. A still is frame 0
/// with nothing owed; anything else loops. A zero-delay frame is skipped rather than held, which
/// is why `decode_gif` floors what the file asks for.
fn frame_at(delays: &[Duration], elapsed: Duration) -> (usize, Option<Duration>) {
    let total: Duration = delays.iter().sum();
    if total.is_zero() {
        return (0, None);
    }
    let mut at = Duration::from_nanos((elapsed.as_nanos() % total.as_nanos()) as u64);
    for (index, delay) in delays.iter().enumerate() {
        if at < *delay {
            return (index, Some(*delay - at));
        }
        at -= *delay;
    }
    unreachable!("the remainder is under the total, so some frame holds it")
}

/// [`Animation::deltas`] to replay to move the shown frame from `current` to `target`, and whether
/// the base has to go back up first. A loop wrap (`target < current`) always does, since nothing
/// else undoes a later frame's rects; moving forward replays only what changed since `current`.
fn replay_steps(current: usize, target: usize) -> (bool, std::ops::Range<usize>) {
    if target >= current { (false, current..target) } else { (true, 0..target) }
}

/// Canvas-dependent half of a load: one texture, from the base frame, plus the later frames' rects
/// kept on the CPU for [`ImageCache::showing`] to replay (ADR-0235).
///
/// Uploads premultiplied, whatever the decoder produced (ADR-0184). `image` and `gif` hand back
/// straight alpha and `resvg` hands back premultiplied, and passing that difference on as a femtovg
/// flag was enough while femtovg was the only thing sampling these textures. A config shader
/// samples them directly, and cannot be handed two conventions: it would have to know which decoder
/// produced its endpoint, which is an engine detail with no business in a config's `main()`.
///
/// Multiplying after the sample would not do instead. A texture lookup filters between texels
/// first, so a straight-alpha edge interpolates colour the alpha was meant to hide, and no later
/// multiply recovers it. Premultiplying the buffer is one pass over pixels that are about to be
/// copied to the GPU anyway.
///
/// An animation's texture waits for a draw without a node: each `image` node draws its own.
fn upload(canvas: &mut Canvas<OpenGl>, decoded: Decoded) -> Result<(Option<ImageId>, Option<Animation>), String> {
    let Decoded { mut base, delays, mut deltas, width, height, premultiplied, .. } = decoded;
    if !premultiplied {
        premultiply(&mut base);
        for delta in &mut deltas {
            premultiply(&mut delta.pixels);
        }
    }
    if !deltas.is_empty() {
        return Ok((None, Some(Animation { base, width, height, delays, deltas, current: 0 })));
    }
    let source = ImageSource::from(femtovg::imgref::Img::new(base.as_rgba(), width as usize, height as usize));
    Ok((Some(canvas.create_image(source, ImageFlags::PREMULTIPLIED).map_err(femtovg_error)?), None))
}

/// ponytail: femtovg 0.27's `ErrorKind` all prints "canvas error"; use `Display` once it doesn't.
fn femtovg_error(err: ErrorKind) -> String {
    format!("{err:?}")
}

/// One `image` node's playback of one file: its clock, kept while the node is in a tree, and its
/// texture of the drawn slot with the frame on it, kept while a painted list draws it. Pausing one
/// node never holds another's frame, even within one paint.
pub(super) struct NodeClock {
    path: std::path::PathBuf,
    pub(super) clock: Clock,
    pub(super) texture: Option<NodeTexture>,
}

pub(super) struct NodeTexture {
    key: CacheKey,
    pub(super) image: ImageId,
    pub(super) current: usize,
    bytes: usize,
}

/// Node textures one slot may have; past it, or past the texture budget, a node keeps its clock
/// but draws the slot's shared texture, where nodes may show each other's frame.
///
/// ponytail: 8, since each copy costs a whole texture. Upgrade: one texture per distinct frame,
/// shared by the nodes showing it.
const NODE_TEXTURES_PER_SLOT: usize = 8;

/// Node textures of `key`'s slot.
fn node_textures(clocks: &super::HashMap<NodeId, Vec<NodeClock>>, key: &CacheKey) -> usize {
    clocks.values().flatten().filter(|own| own.texture.as_ref().is_some_and(|texture| texture.key == *key)).count()
}

/// A texture of `anim`'s first frame.
fn first_frame(canvas: &mut Canvas<OpenGl>, anim: &Animation) -> Option<ImageId> {
    let base = femtovg::imgref::Img::new(anim.base.as_rgba(), anim.width as usize, anim.height as usize);
    canvas.create_image(ImageSource::from(base), ImageFlags::PREMULTIPLIED).ok()
}

/// Puts frame `target` on `id`, which shows frame `current`.
fn replay(canvas: &mut Canvas<OpenGl>, id: ImageId, anim: &Animation, current: &mut usize, target: usize) {
    if target == *current {
        return;
    }
    let (reset, catch_up) = replay_steps(*current, target);
    if reset {
        let source = ImageSource::from(femtovg::imgref::Img::new(
            anim.base.as_rgba(),
            anim.width as usize,
            anim.height as usize,
        ));
        let _ = canvas.update_image(id, source, 0, 0);
    }
    // A change the crop cut away entirely extracts to nothing and uploads nothing.
    for delta in anim.deltas[catch_up].iter().filter(|delta| !delta.pixels.is_empty()) {
        let (left, top, width, height) = delta.rect;
        let source =
            ImageSource::from(femtovg::imgref::Img::new(delta.pixels.as_rgba(), width as usize, height as usize));
        let _ = canvas.update_image(id, source, left as usize, top as usize);
    }
    *current = target;
}

impl ImageCache {
    /// The texture `key` is showing now, replaying its animation's deltas onto it if the elapsed
    /// time has moved its frame on, and the repaint its successor owes (ADR-0235). An `image`
    /// node draws an animation on its own clock and texture, charged to the texture budget; a
    /// paused one holds its frame and owes none. Other draws share the slot's. `None` for a
    /// pending or failed slot.
    pub(super) fn showing(
        &mut self,
        canvas: &mut Canvas<OpenGl>,
        key: &CacheKey,
        node_clock: Option<(NodeId, bool)>,
    ) -> Option<ImageId> {
        // Only a node still without a texture for `key` asks what the GPU holds.
        let has_texture = |own: &NodeClock| own.texture.as_ref().is_some_and(|texture| texture.key == *key);
        let gpu_bytes = node_clock
            .filter(|(node, _)| !self.clocks.get(node).is_some_and(|clocks| clocks.iter().any(has_texture)))
            .map(|_| self.gpu_bytes());
        let Some(Slot::Ready { image, clock, anim, .. }) = self.entries.get_mut(key).map(|entry| &mut entry.slot)
        else {
            return None;
        };
        // A still, or an animation with nothing to replay, never reads a clock.
        let Some(anim) = anim.as_mut().filter(|anim| !anim.deltas.is_empty()) else {
            return *image;
        };
        let now = Instant::now();
        let (own_id, target, next, playing) = match node_clock {
            None => {
                let (target, next) = frame_at(&anim.delays, clock.elapsed(true, now));
                (None, target, next, true)
            }
            Some((node, playing)) => {
                let sharing = node_textures(&self.clocks, key);
                let clocks = self.clocks.entry(node).or_default();
                let own = match clocks.iter().position(|own| own.path == key.path) {
                    Some(index) => &mut clocks[index],
                    None => {
                        clocks.push(NodeClock { path: key.path.clone(), clock: Clock::default(), texture: None });
                        clocks.last_mut()?
                    }
                };
                let (target, next) = frame_at(&anim.delays, own.clock.elapsed(playing, now));
                // A resize or rewrite keeps the clock and replaces the texture.
                if let Some(stale) = own.texture.take_if(|texture| texture.key != *key) {
                    self.evicted.push(stale.image);
                    self.resident_bytes -= stale.bytes;
                }
                let bytes = anim.base.len();
                if own.texture.is_none()
                    && sharing < NODE_TEXTURES_PER_SLOT
                    && gpu_bytes.is_some_and(|gpu| gpu + bytes <= self.texture_budget)
                    && let Some(image) = first_frame(canvas, anim)
                {
                    self.resident_bytes += bytes;
                    own.texture = Some(NodeTexture { key: key.clone(), image, current: 0, bytes });
                }
                let own_id = own.texture.as_mut().map(|texture| {
                    replay(canvas, texture.image, anim, &mut texture.current, target);
                    texture.image
                });
                (own_id, target, next, playing)
            }
        };
        let id = match own_id {
            Some(id) => id,
            None => {
                let id = match *image {
                    Some(id) => id,
                    None => *image.insert(first_frame(canvas, anim)?),
                };
                let mut current = anim.current;
                replay(canvas, id, anim, &mut current, target);
                anim.current = current;
                id
            }
        };
        if let Some(next) = next.filter(|_| playing) {
            // The soonest wins: one pass can draw a refused tile and a 10 fps GIF.
            let due = now + next;
            self.deferred = Some(self.deferred.map_or(due, |owed| owed.min(due)));
        }
        Some(id)
    }

    /// Texture bytes on the GPU: slots' and nodes'. Not `resident_bytes`, which also counts an
    /// animation's frames kept on the CPU and would refuse every large GIF a node texture.
    fn gpu_bytes(&self) -> usize {
        let slots = self.entries.values().filter_map(|entry| match &entry.slot {
            Slot::Ready { image: Some(_), anim, bytes, .. } => {
                Some(anim.as_ref().map_or(*bytes, |anim| anim.base.len()))
            }
            _ => None,
        });
        let nodes = self.clocks.values().flatten().filter_map(|own| own.texture.as_ref().map(|texture| texture.bytes));
        slots.chain(nodes).sum()
    }

    /// Frees the node textures no painted list draws: scrolled out, clipped, or on a surface
    /// that stopped painting, whose clocks stay for [`ImageCache::forget_node_clocks`]. A node
    /// that draws another file now drops the clocks of the files it left.
    pub(super) fn release_undrawn_node_textures(&mut self, drawn: &[(Option<NodeId>, CacheKey)]) {
        let (evicted, resident) = (&mut self.evicted, &mut self.resident_bytes);
        for (node, clocks) in &mut self.clocks {
            let drawn_here =
                |path: &std::path::Path| drawn.iter().any(|(at, key)| *at == Some(*node) && key.path == path);
            let moved_on = clocks.iter().any(|own| drawn_here(&own.path));
            for own in clocks.iter_mut() {
                let kept =
                    |texture: &NodeTexture| drawn.iter().any(|(at, key)| *at == Some(*node) && *key == texture.key);
                if let Some(gone) = own.texture.take_if(|texture| !kept(texture)) {
                    evicted.push(gone.image);
                    *resident -= gone.bytes;
                }
            }
            if moved_on {
                clocks.retain(|own| drawn_here(&own.path));
            }
        }
    }

    /// Drops the playback of nodes no longer in any tree, after a pass.
    pub fn forget_node_clocks(&mut self, live: impl Fn(NodeId) -> bool) {
        let (evicted, resident) = (&mut self.evicted, &mut self.resident_bytes);
        self.clocks.retain(|node, clocks| {
            live(*node) || {
                for gone in clocks.drain(..).filter_map(|own| own.texture) {
                    evicted.push(gone.image);
                    *resident -= gone.bytes;
                }
                false
            }
        });
    }

    /// Whether any node keeps its own playback, which [`ImageCache::forget_node_clocks`] prunes.
    pub fn has_node_clocks(&self) -> bool {
        !self.clocks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(cache: &mut ImageCache, node: u64) -> &mut NodeClock {
        &mut cache.clocks.get_mut(&NodeId::test(node)).unwrap()[0]
    }

    /// `image.playing` is each node's: its own texture and frame, a paused one owing no repaint;
    /// a dissolve's two files keep two clocks; a scrolled-out node frees its texture but keeps its
    /// time, also across a resize; a node gone from every tree frees all; past the budget a node
    /// shares the slot's texture.
    #[test]
    fn each_node_plays_a_gif_on_its_own_clock_and_texture() {
        use super::super::decode::tests::gif_fixture;
        use super::super::{Fit, ImageRequest, Load};
        use crate::layout::paint::{init_headless_egl, text_painter};
        use gif::DisposalMethod::Keep;

        let Some(instance) = init_headless_egl(8, 8) else { return eprintln!("skipped: no headless EGL") };
        let shaping = crate::text::shaping::ShapingHandle::spawn();
        let Some(mut painter) = text_painter(&instance, &shaping, 8, 8) else {
            return eprintln!("skipped: no text painter");
        };
        let dir = tempfile::tempdir().unwrap();
        let red_then_blue = [((0, 0, 2, 2), [255, 0, 0, 255], Keep), ((0, 0, 2, 2), [0, 0, 255, 255], Keep)];
        let (a, b) = (
            gif_fixture(dir.path(), "a.gif", (2, 2), &red_then_blue),
            gif_fixture(dir.path(), "b.gif", (2, 2), &red_then_blue),
        );
        let mut cache = ImageCache::inline();
        // Room for three 16-byte node textures, though the slot's frames alone take 32 bytes.
        cache.set_texture_budget(48);
        let request = |path, node, playing, edge| ImageRequest {
            path,
            box_px: (edge, edge),
            tint: None,
            fit: Fit::Stretch,
            blur_px: 0,
            wants_size: false,
            clock: Some((NodeId::test(node), playing)),
        };
        // A frame is due within its 50 ms delay; the file's 500 ms stat recheck is not a frame.
        let start = Instant::now();
        let mut draw = |cache: &mut ImageCache, path, node, playing, edge| {
            let id = cache.image(painter.canvas_mut(), &request(path, node, playing, edge), Load::Inline).unwrap();
            (id, cache.take_deferred().is_some_and(|due| due < start + Duration::from_millis(400)))
        };
        let frame = |cache: &mut ImageCache, node| own(cache, node).texture.as_ref().map(|texture| texture.current);

        let (paused, owed) = draw(&mut cache, &a, 1, false, 2);
        assert!(!owed, "paused: no frame owed");
        let (playing, owed) = draw(&mut cache, &a, 2, true, 2);
        assert!(owed && playing != paused, "the other node plays on its own texture");
        assert!(cache.resident_bytes > 48, "admitted on GPU bytes, not the frames kept on the CPU");
        // Held 60 ms in, the second frame, so no timing under load moves it.
        own(&mut cache, 2).clock = Clock { since: None, held: Duration::from_millis(60) };
        assert_eq!(draw(&mut cache, &a, 2, false, 2).0, playing);
        assert_eq!(draw(&mut cache, &a, 1, false, 2), (paused, false), "still paused");
        assert_eq!((frame(&mut cache, 1), frame(&mut cache, 2)), (Some(0), Some(1)), "each its own frame");

        draw(&mut cache, &b, 2, false, 2);
        draw(&mut cache, &a, 2, false, 2);
        assert_eq!((frame(&mut cache, 2), cache.clocks[&NodeId::test(2)].len()), (Some(1), 2), "a dissolve keeps both");

        cache.trim(|| vec![request(&a, 1, false, 2), request(&a, 2, false, 2)]);
        assert_eq!(cache.clocks[&NodeId::test(2)].len(), 1, "the dissolve over, `b`'s clock goes");
        cache.trim(|| vec![request(&a, 1, false, 2)]);
        assert_eq!(frame(&mut cache, 2), None, "scrolled out: the texture goes");
        draw(&mut cache, &a, 2, false, 4);
        assert_eq!(frame(&mut cache, 2), Some(1), "back in and resized: the time stayed");

        cache.set_texture_budget(0);
        let shared = draw(&mut cache, &a, 3, false, 2).0;
        assert!(shared != paused && cache.clocks[&NodeId::test(3)][0].texture.is_none(), "past the budget: shared");

        let resident = cache.resident_bytes;
        cache.forget_node_clocks(|node| node == NodeId::test(1));
        assert_eq!((cache.clocks.len(), cache.resident_bytes), (1, resident - 16), "gone from every tree");
    }

    /// ADR-0233. Each frame's own delay, the zero-delay frame skipped, and a loop rather than a
    /// stop on the last frame. A still owes nothing, which is what keeps a PNG off the poll
    /// loop's timeout.
    #[test]
    fn a_frame_index_follows_each_delay_and_loops() {
        let delays = [Duration::from_millis(100), Duration::ZERO, Duration::from_millis(50)];
        assert_eq!(frame_at(&delays, Duration::ZERO), (0, Some(Duration::from_millis(100))));
        assert_eq!(frame_at(&delays, Duration::from_millis(99)), (0, Some(Duration::from_millis(1))));
        assert_eq!(frame_at(&delays, Duration::from_millis(100)), (2, Some(Duration::from_millis(50))));
        assert_eq!(frame_at(&delays, Duration::from_millis(150)), (0, Some(Duration::from_millis(100))));
        assert_eq!(frame_at(&delays, Duration::from_millis(1_000_000)), (2, Some(Duration::from_millis(50))));
        assert_eq!(frame_at(&[Duration::ZERO], Duration::from_secs(9)), (0, None));
    }

    /// `image.playing`: a pause holds the frame it reached, a first draw paused holds the first
    /// frame, and a resume carries on from where the pause held.
    #[test]
    fn a_paused_clock_holds_its_place_and_resumes_from_it() {
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        let mut clock = Clock::default();
        assert_eq!(clock.elapsed(false, t0 + ms(500)), Duration::ZERO, "never played: the first frame");
        assert_eq!(clock.elapsed(true, t0 + ms(600)), Duration::ZERO, "playing starts from there");
        assert_eq!(clock.elapsed(true, t0 + ms(650)), ms(50));
        assert_eq!(clock.elapsed(false, t0 + ms(700)), ms(100));
        assert_eq!(clock.elapsed(false, t0 + ms(5_000)), ms(100), "held while paused");
        assert_eq!(clock.elapsed(true, t0 + ms(6_000)), ms(100), "resumed where it held");
        assert_eq!(clock.elapsed(true, t0 + ms(6_030)), ms(130));
    }

    /// [`replay_steps`] moving forward replays only what changed; a loop wrap always re-uploads the
    /// base first, since nothing else undoes a later frame's rect.
    #[test]
    fn replay_steps_only_resets_on_a_loop_wrap() {
        assert_eq!(replay_steps(0, 3), (false, 0..3));
        assert_eq!(replay_steps(2, 3), (false, 2..3));
        assert_eq!(replay_steps(2, 2), (false, 2..2), "no movement, nothing to replay");
        assert_eq!(replay_steps(3, 0), (true, 0..0), "wrapping to the base itself replays no delta");
        assert_eq!(replay_steps(3, 1), (true, 0..1), "wrapping past the base replays up to the target");
    }
}
