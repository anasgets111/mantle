//! `palette` global (ADR-0249). Renderer-local, unlike `process.run`: a palette has nothing to
//! outlive a Renderer respawn.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};

use mlua::{Function, Lua, Table};

use super::luacats::{As, lua_class, lua_fn, lua_shape, spelled};
use shared::warn;

use crate::image::quantize::quantize_file;
use crate::image::thumbnails;

const DEFAULT_DEPTH: i64 = 3;
/// A freedesktop "normal" thumbnail's edge, so the default hits a thumbnail already on disk.
const DEFAULT_RESCALE: i64 = 128;

/// A call's id and swatches, `None` on failure.
type PaletteResult = (u64, Option<Vec<PaletteSwatch>>);

// `share` is the fraction of counted pixels, so a config can weigh colourfulness against coverage.
lua_shape! {
    #[record = "PaletteSwatch"]
    struct PaletteSwatch {
        /// `#RRGGBB`.
        color: String as crate::layout::node::prop::Color,
        /// Fraction of the counted (non-transparent) pixels, 0 to 1.
        share: f64,
    }
}

/// `palette.quantize`'s `opts`, read field by field against its own ranges.
struct Options;

spelled!(Options => "{ depth?: integer, rescale?: integer }");

#[derive(Clone)]
pub struct PaletteRegistry(Rc<RefCell<Inner>>);

struct Inner {
    next_id: u64,
    pending: HashMap<u64, Function>,
    result_tx: Sender<PaletteResult>,
    results: Receiver<PaletteResult>,
    /// `None` under `mantle check`, which runs no loop to wake.
    waker: Option<crate::wake::Waker>,
    cache_root: Option<PathBuf>,
}

impl PaletteRegistry {
    pub fn new(waker: Option<crate::wake::Waker>) -> Self {
        Self::with_cache_root(waker, thumbnails::cache_dir())
    }

    fn with_cache_root(waker: Option<crate::wake::Waker>, cache_root: Option<PathBuf>) -> Self {
        let (result_tx, results) = std::sync::mpsc::channel();
        PaletteRegistry(Rc::new(RefCell::new(Inner {
            next_id: 0,
            pending: HashMap::new(),
            result_tx,
            results,
            waker,
            cache_root,
        })))
    }

    fn quantize(&self, path: String, depth: u8, rescale: u32, cb: Function) -> PaletteHandle {
        let mut inner = self.0.borrow_mut();
        let id = inner.next_id;
        inner.next_id += 1;
        inner.pending.insert(id, cb);
        let (tx, waker, cache_root) = (inner.result_tx.clone(), inner.waker.clone(), inner.cache_root.clone());
        // ponytail: a thread per call, outside the image pool's `Budget`, so a quantize can briefly
        // push decodes past `DECODE_POOL_BYTES`. Share the `Budget` if that is ever measured.
        std::thread::spawn(move || {
            let swatches = match quantize_file(Path::new(&path), depth, rescale, cache_root.as_deref()) {
                Ok(buckets) => {
                    let total: u32 = buckets.iter().map(|(count, _)| count).sum();
                    Some(
                        buckets
                            .into_iter()
                            .map(|(count, [r, g, b])| PaletteSwatch {
                                color: format!("#{r:02X}{g:02X}{b:02X}"),
                                share: f64::from(count) / f64::from(total),
                            })
                            .collect(),
                    )
                }
                Err(err) => {
                    warn!("palette.quantize({path}): {err}");
                    None
                }
            };
            let _ = tx.send((id, swatches));
            if let Some(waker) = waker {
                waker.wake();
            }
        });
        PaletteHandle { id, registry: self.clone() }
    }

    /// Runs each finished call's callback once; a cancelled id is dropped.
    pub fn poll(&self) {
        let results: Vec<PaletteResult> = std::iter::from_fn(|| self.0.borrow().results.try_recv().ok()).collect();
        for (id, swatches) in results {
            let Some(cb) = self.0.borrow_mut().pending.remove(&id) else { continue };
            if let Err(err) = cb.call::<()>(swatches) {
                warn!("palette.quantize(id={id}): callback raised an error: {err}");
            }
        }
    }
}

pub struct PaletteHandle {
    id: u64,
    registry: PaletteRegistry,
}

lua_class! {
    impl PaletteHandle {
        /// Drops the callback. The decode still finishes.
        fn cancel(_lua, this) {
            this.registry.0.borrow_mut().pending.remove(&this.id);
            Ok(())
        }
    }
}

fn opt(opts: &Option<Table>, key: &str, default: i64) -> mlua::Result<i64> {
    Ok(match opts {
        Some(opts) => opts.get::<Option<i64>>(key)?.unwrap_or(default),
        None => default,
    })
}

pub fn register(lua: &Lua, registry: PaletteRegistry) -> mlua::Result<()> {
    super::luacats::lua_table!(lua, palette)?;
    lua_fn!(
        lua,
        /// Extracts an image's dominant colours off the Lua thread (ADR-0249). `cb` gets them most common
        /// first, or `nil` on failure (logged). `cb` runs unbudgeted.
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#palettequantize)
        fn palette.quantize(
            _lua,
            /// A local raster file; no SVG or URL.
            path: String,
            /// `depth` 0 to 8, default 3: up to `2^depth` colours. `rescale` caps the longest edge before
            /// counting, default 128, `0` for full size. Out of range raises.
            opts: As<Option<Table>, Option<Options>>,
            cb: fn(swatches: Option<Vec<PaletteSwatch>>),
        ) -> PaletteHandle {
            let (opts, cb) = (opts.0, cb.0);
            if let Some(opts) = &opts {
                super::marshal::only_keys(opts, &["depth", "rescale"])
                    .map_err(|detail| mlua::Error::runtime(format!("palette.quantize: options: {detail}")))?;
            }
            let depth = opt(&opts, "depth", DEFAULT_DEPTH)?;
            let rescale = opt(&opts, "rescale", DEFAULT_RESCALE)?;
            let (Ok(depth @ 0..=8), Ok(rescale)) = (u8::try_from(depth), u32::try_from(rescale)) else {
                return Err(mlua::Error::runtime(format!(
                    "palette.quantize: depth must be 0..=8 and rescale not negative, got {depth} and {rescale}"
                )));
            };
            Ok(registry.quantize(path, depth, rescale, cb))
        }
    )
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// A registry pointed at a tempdir cache root, never the real `$HOME/.cache`.
    fn lua_with_palette() -> (Lua, PaletteRegistry) {
        let lua = Lua::new();
        let cache = tempfile::tempdir().unwrap();
        let registry = PaletteRegistry::with_cache_root(None, Some(cache.path().to_path_buf()));
        register(&lua, registry.clone()).unwrap();
        (lua, registry)
    }

    /// Polls until the background thread's result lands or a generous timeout trips; keeps the
    /// test from hanging silently if `poll` regresses.
    fn poll_until_dispatched(registry: &PaletteRegistry, probe: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !probe() {
            assert!(Instant::now() < deadline, "palette.quantize callback never fired");
            registry.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn quantize_returns_a_handle_and_the_callback_fires_with_swatches() {
        let (lua, registry) = lua_with_palette();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("solid.png");
        ::image::RgbaImage::from_pixel(4, 4, ::image::Rgba([10, 20, 30, 255])).save(&path).unwrap();

        lua.load(format!(r#"handle = palette.quantize({path:?}, nil, function(colors) probe = colors end)"#))
            .exec()
            .unwrap();
        let is_userdata: bool = lua.load("return type(handle) == \"userdata\"").eval().unwrap();
        assert!(is_userdata, "palette.quantize must return a userdata handle");

        poll_until_dispatched(&registry, || lua.load("return probe ~= nil").eval().unwrap());
        let (color, share): (String, f64) = lua.load("return probe[1].color, probe[1].share").eval().unwrap();
        assert_eq!(
            (color.as_str(), share, lua.load("return #probe").eval::<i64>().unwrap()),
            ("#0A141E", 1.0, 1),
            "nil opts use the defaults, and a solid image yields one colour"
        );
    }

    #[test]
    fn a_decode_failure_calls_back_with_nil() {
        let (lua, registry) = lua_with_palette();

        lua.load(r#"handle = palette.quantize("/nonexistent/wall.png", nil, function(colors) probe = { called = true, is_nil = colors == nil } end)"#)
            .exec()
            .unwrap();

        poll_until_dispatched(&registry, || lua.load("return probe ~= nil").eval().unwrap());
        let is_nil: bool = lua.load("return probe.is_nil").eval().unwrap();
        assert!(is_nil, "a decode failure must call back with nil, not raise or hang");
    }

    #[test]
    fn cancel_suppresses_the_callback() {
        let (lua, registry) = lua_with_palette();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("solid.png");
        ::image::RgbaImage::from_pixel(4, 4, ::image::Rgba([1, 2, 3, 255])).save(&path).unwrap();

        lua.load(format!(r#"handle = palette.quantize({path:?}, nil, function(colors) probe = colors end)"#))
            .exec()
            .unwrap();
        lua.load("handle:cancel()").exec().unwrap();

        // Give the background thread every chance to finish and enqueue its result before the
        // assertion, so this proves suppression rather than a race that never reached `poll`.
        std::thread::sleep(Duration::from_millis(200));
        registry.poll();
        let is_nil: bool = lua.load("return probe == nil").eval().unwrap();
        assert!(is_nil, "a cancelled handle's callback must never run");
    }

    #[test]
    fn depth_out_of_range_is_a_config_error() {
        let (lua, _registry) = lua_with_palette();
        let err = lua.load(r#"palette.quantize("x.png", { depth = 9 }, function() end)"#).exec().unwrap_err();
        assert!(err.to_string().contains("0..=8"), "expected the depth range in the error, got: {err}");
    }
}
