//! `palette` global (ADR-0249). Renderer-local, unlike `process.run`: a palette has nothing to
//! outlive a Renderer respawn.

use std::path::{Path, PathBuf};

use mlua::{Function, Lua, Table, Value};

use super::calls::Calls;
use super::luacats::{As, lua_class, lua_fn, lua_shape, spelled};
use shared::warn;

use crate::image::quantize::{Method, quantize_file};
use crate::image::thumbnails;
use crate::layout::node::preview_for_error;
use crate::lua::call_logged;
use crate::lua::marshal::{expected_one_of, out_of_range};

const DEFAULT_DEPTH: i64 = 3;
/// A freedesktop "normal" thumbnail's edge, so the default hits a thumbnail already on disk.
const DEFAULT_RESCALE: i64 = 128;

/// A call's swatches, `None` on failure.
type PaletteResult = Option<Vec<PaletteSwatch>>;

// `share` is the fraction of counted pixels, so a config can weigh colourfulness against coverage.
lua_shape! {
    #[record = "PaletteSwatch"]
    pub(crate) struct PaletteSwatch {
        /// `#RRGGBB`.
        color: String as crate::layout::node::prop::Color,
        /// Fraction of the counted (non-transparent) pixels, 0 to 1.
        share: f64,
    }
}

/// `palette.quantize`'s `opts`, read field by field against its own ranges.
struct Options;

spelled!(Options => r#"{ depth?: integer, method?: "celebi"|"wu", rescale?: integer, [string]: "no such property" }"#);

#[derive(Clone)]
pub struct PaletteRegistry {
    calls: Calls<PaletteResult>,
    cache_root: Option<PathBuf>,
}

impl PaletteRegistry {
    pub fn new(waker: Option<crate::wake::Waker>) -> Self {
        Self::with_cache_root(waker, thumbnails::cache_dir())
    }

    fn with_cache_root(waker: Option<crate::wake::Waker>, cache_root: Option<PathBuf>) -> Self {
        Self { calls: Calls::new(waker, false), cache_root }
    }

    fn quantize(&self, path: String, depth: u8, method: Method, rescale: u32, on_done: Function) -> PaletteHandle {
        let cache_root = self.cache_root.clone();
        let (id, job) = self.calls.submit(Some(on_done), move || {
            match quantize_file(Path::new(&path), depth, method, rescale, cache_root.as_deref()) {
                Ok(buckets) => {
                    let total: u32 = buckets.iter().map(|(count, _)| count).sum();
                    Some(
                        buckets
                            .into_iter()
                            .map(|(count, rgb)| PaletteSwatch {
                                color: format!("#{:06X}", rgb.as_u32()),
                                share: f64::from(count) / f64::from(total),
                            })
                            .collect(),
                    )
                }
                Err(err) => {
                    warn!("palette.quantize({path}): {err}");
                    None
                }
            }
        });
        if let Some(job) = job {
            // ponytail: a thread per call, outside the image pool's `Budget`, so a quantize can briefly
            // push decodes past `DECODE_POOL_BYTES`. Share the `Budget` if that is ever measured.
            std::thread::spawn(job);
        }
        PaletteHandle { id, calls: self.calls.clone() }
    }

    /// Drops every pending callback at an evaluation boundary; their decodes still finish and are discarded.
    pub fn clear(&self) {
        self.calls.clear();
    }

    /// Runs each finished call's callback once; a cancelled id is dropped.
    pub fn poll(&self) {
        self.calls.poll(|id, on_done, swatches| {
            call_logged(on_done, swatches, format_args!("palette.quantize(id={id}): callback"));
        });
    }
}

pub struct PaletteHandle {
    id: u64,
    calls: Calls<PaletteResult>,
}

lua_class! {
    impl PaletteHandle {
        /// Drops the callback. The decode still finishes.
        fn cancel(_lua, this) {
            this.calls.cancel(this.id);
            Ok(())
        }
    }
}

fn method(opts: &Option<Table>) -> mlua::Result<Method> {
    let value = match opts {
        Some(opts) => opts.get::<Value>("method")?,
        None => Value::Nil,
    };
    let valid = Method::NAMES.map(|(name, _)| name);
    match value {
        Value::Nil => Ok(Method::Celebi),
        Value::String(name) => {
            Method::NAMES.into_iter().find(|(known, _)| name.as_bytes() == known.as_bytes()).map(|(_, m)| m).ok_or_else(
                || {
                    let got = preview_for_error(&Value::String(name.clone()));
                    let hint = expected_one_of(&valid, &name.to_string_lossy(), &got);
                    mlua::Error::runtime(format!("palette.quantize: options: `method` is not a method, {hint}"))
                },
            )
        }
        other => Err(mlua::Error::runtime(format!(
            "palette.quantize: options: `method` must be a string, got {}",
            preview_for_error(&other)
        ))),
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
        /// Extracts an image's dominant colours off the Lua thread (ADR-0249). `on_done` gets them most common
        /// first, or `nil` on failure (logged). `on_done` runs unbudgeted.
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#palettequantize)
        fn palette.quantize(
            _lua,
            /// A local raster file, a path or `~/` under `$HOME`; no SVG or URL.
            path: String,
            /// `depth` 0 to 8, default 3: up to `2^depth` colours. `method` `"celebi"` (default) or `"wu"`. `rescale` caps the longest edge before
            /// counting, default 128, `0` for full size. Out of range raises.
            opts: As<Option<Table>, Option<Options>>,
            on_done: fn(swatches: Option<Vec<PaletteSwatch>>),
        ) -> PaletteHandle {
            let (opts, on_done) = (opts.0, on_done.0);
            if let Some(opts) = &opts {
                super::marshal::only_keys(opts, &["depth", "method", "rescale"])
                    .map_err(|detail| mlua::Error::runtime(format!("palette.quantize: options: {detail}")))?;
            }
            let depth = opt(&opts, "depth", DEFAULT_DEPTH)?;
            let method = method(&opts)?;
            let rescale = opt(&opts, "rescale", DEFAULT_RESCALE)?;
            let Ok(depth @ 0..=8) = u8::try_from(depth) else {
                let range = out_of_range(0, 8, format!("integer {depth}"));
                return Err(mlua::Error::runtime(format!("palette.quantize: options: `depth` {range}")));
            };
            let Ok(rescale) = u32::try_from(rescale) else {
                let range = out_of_range(0, u32::MAX, format!("integer {rescale}"));
                return Err(mlua::Error::runtime(format!("palette.quantize: options: `rescale` {range}")));
            };
            let path = super::marshal::expand_path("palette.quantize", &path)?;
            Ok(registry.quantize(path, depth, method, rescale, on_done))
        }
    )?;
    super::scheme::register(lua)
}

#[cfg(test)]
mod tests {
    use super::super::calls::poll_until;
    use super::*;

    /// A registry pointed at a tempdir cache root, never the real `$HOME/.cache`.
    fn lua_with_palette() -> (Lua, PaletteRegistry) {
        let lua = Lua::new();
        let cache = tempfile::tempdir().unwrap();
        let registry = PaletteRegistry::with_cache_root(None, Some(cache.path().to_path_buf()));
        register(&lua, registry.clone()).unwrap();
        (lua, registry)
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

        poll_until(|| registry.poll(), || lua.load("return probe ~= nil").eval().unwrap());
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

        lua.load(r#"handle = palette.quantize("/nonexistent/wall.png", nil, function(colors) probe = { is_nil = colors == nil } end)"#)
            .exec()
            .unwrap();

        poll_until(|| registry.poll(), || lua.load("return probe ~= nil").eval().unwrap());
        let is_nil: bool = lua.load("return probe.is_nil").eval().unwrap();
        assert!(is_nil, "a decode failure must call back with nil, not raise or hang");
    }

    /// Both a handle's `cancel` and the evaluation boundary's `clear`.
    #[test]
    fn cancel_or_a_new_evaluation_suppresses_the_callback() {
        let cancels: [fn(&Lua, &PaletteRegistry); 2] =
            [|lua, _| lua.load("handle:cancel()").exec().unwrap(), |_, registry| registry.clear()];
        for cancel in cancels {
            let (lua, registry) = lua_with_palette();
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("solid.png");
            ::image::RgbaImage::from_pixel(4, 4, ::image::Rgba([1, 2, 3, 255])).save(&path).unwrap();

            lua.load(format!(r#"handle = palette.quantize({path:?}, nil, function(colors) probe = colors end)"#))
                .exec()
                .unwrap();
            registry.calls.replay(|| cancel(&lua, &registry));
            registry.poll();
            let is_nil: bool = lua.load("return probe == nil").eval().unwrap();
            assert!(is_nil, "a cancelled callback must never run");
        }
    }

    #[test]
    fn method_picks_the_quantizer_and_a_refused_option_is_a_config_error() {
        let (lua, _registry) = lua_with_palette();
        for name in ["celebi", "wu"] {
            lua.load(format!(r#"palette.quantize("x.png", {{ method = "{name}" }}, function() end)"#)).exec().unwrap();
        }
        for (opts, needle) in
            [(r#"method = "median""#, "`celebi`, `wu`"), ("depth = 9", "must be within [0, 8], got integer 9")]
        {
            let err =
                lua.load(format!(r#"palette.quantize("x.png", {{ {opts} }}, function() end)"#)).exec().unwrap_err();
            assert!(err.to_string().contains(needle), "{opts}: expected {needle:?} in the error, got: {err}");
        }
    }
}
