//! `palette.scheme` and `palette.score`: Material 3 colour roles from a seed, and M3's seed ranking
//! of `palette.quantize`'s swatches. Pure and synchronous: one scheme took 0.53 ms in a debug test build.

use std::marker::PhantomData;

use material_colors::color::Rgb;
use material_colors::dynamic_color::{DynamicScheme, Platform, SpecVersion, Variant as M3Variant};
use material_colors::hct::Hct;
use material_colors::scheme::Scheme;
use material_colors::utils::math::{difference_degrees, sanitize_degrees_int};
use mlua::{IntoLua, Lua, Table, Value};

use super::luacats::{As, LuaType, lua_fn, spelled};
use super::palette::PaletteSwatch;
use crate::layout::node::prop::Color;

/// The crate's variants but `Cmf`, which takes a second source colour.
const VARIANTS: [(&str, M3Variant); 9] = [
    ("tonal_spot", M3Variant::TonalSpot),
    ("vibrant", M3Variant::Vibrant),
    ("expressive", M3Variant::Expressive),
    ("fidelity", M3Variant::Fidelity),
    ("content", M3Variant::Content),
    ("neutral", M3Variant::Neutral),
    ("monochrome", M3Variant::Monochrome),
    ("rainbow", M3Variant::Rainbow),
    ("fruit_salad", M3Variant::FruitSalad),
];

/// `palette.scheme`'s `opts`.
struct Options;

spelled!(Options => format!(
    r#"{{ dark?: boolean, variant?: {}, contrast?: number, [string]: "no such property" }}"#,
    VARIANTS.map(|(name, _)| format!("\"{name}\"")).join("|")
));

/// The 49 roles `material_colors::scheme::Scheme` names, each a `#RRGGBB` string.
pub(crate) struct Roles(Scheme);

impl LuaType for Roles {
    fn lua() -> String {
        "PaletteScheme".to_string()
    }
    #[cfg(test)]
    fn classes(out: &mut Vec<String>) {
        let keys: Vec<_> = generate(Rgb::new(0, 0, 0), M3Variant::TonalSpot, false, 0.0)
            .0
            .into_iter()
            .map(|(role, _)| (role, false, Color::lua(), ""))
            .collect();
        let stub = super::luacats::shape_stub("record", "PaletteScheme", "Material 3 colour roles.\n", &keys);
        if !out.contains(&stub) {
            out.push(stub);
        }
    }
}

impl IntoLua for Roles {
    fn into_lua(self, lua: &Lua) -> mlua::Result<Value> {
        let table = lua.create_table()?;
        for (role, rgb) in self.0 {
            table.set(role, hex(rgb))?;
        }
        Ok(Value::Table(table))
    }
}

fn hex(rgb: Rgb) -> String {
    format!("#{:06X}", rgb.as_u32())
}

/// `#RRGGBB`; alpha has no place in a seed.
fn parse(color: &str) -> Option<Rgb> {
    let digits = color.strip_prefix('#').filter(|d| d.len() == 6 && d.bytes().all(|b| b.is_ascii_hexdigit()))?;
    u32::from_str_radix(digits, 16).ok().map(Rgb::from_u32)
}

/// The phone platform at spec 2021, `material_colors`' default and matugen's.
fn generate(seed: Rgb, variant: M3Variant, dark: bool, contrast: f64) -> Roles {
    let scheme =
        DynamicScheme::from_spec(Hct::new(seed), variant, dark, Some(contrast), Platform::Phone, SpecVersion::Spec2021);
    Roles(Scheme::from(scheme))
}

/// Material Color Utilities' `Score.score` with its defaults (4 picks, filtering on, Google Blue
/// fallback), over `(colour, weight)` pairs: only the weights' ratios matter.
fn score(colors: &[(Rgb, f64)]) -> Vec<Rgb> {
    const DESIRED: usize = 4;
    let total: f64 = colors.iter().map(|(_, weight)| weight).sum();
    let mut hue_population = [0.0; 360];
    let hcts: Vec<Hct> = colors
        .iter()
        .map(|&(rgb, weight)| {
            let hct = Hct::new(rgb);
            hue_population[hct.get_hue().floor() as usize] += weight;
            hct
        })
        .collect();
    let mut excited = [0.0; 360];
    for (hue, population) in hue_population.iter().enumerate() {
        for neighbour in hue as i32 - 14..hue as i32 + 16 {
            excited[sanitize_degrees_int(neighbour) as usize] += population / total;
        }
    }
    let mut scored: Vec<(Hct, f64)> = hcts
        .into_iter()
        .filter_map(|hct| {
            let proportion = excited[sanitize_degrees_int(hct.get_hue().round() as i32) as usize];
            if hct.get_chroma() < 5.0 || proportion <= 0.01 {
                return None;
            }
            let chroma_weight = if hct.get_chroma() < 48.0 { 0.1 } else { 0.3 };
            Some((hct, proportion * 100.0 * 0.7 + (hct.get_chroma() - 48.0) * chroma_weight))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    // Widest hue spread first: 90 degrees apart, relaxing to 15 until `DESIRED` fit.
    let mut chosen: Vec<Hct> = Vec::new();
    for spread in (15..=90).rev() {
        chosen.clear();
        for (hct, _) in &scored {
            if chosen.iter().all(|c| difference_degrees(hct.get_hue(), c.get_hue()) >= f64::from(spread)) {
                chosen.push(*hct);
            }
            if chosen.len() >= DESIRED {
                break;
            }
        }
        if chosen.len() >= DESIRED {
            break;
        }
    }
    if chosen.is_empty() {
        return vec![Rgb::from_u32(0x4285F4)];
    }
    chosen.into_iter().map(Rgb::from).collect()
}

fn options(opts: Option<Table>) -> Result<(bool, M3Variant, f64), String> {
    let Some(opts) = opts else { return Ok((false, M3Variant::TonalSpot, 0.0)) };
    super::marshal::only_keys(&opts, &["dark", "variant", "contrast"])?;
    let dark = match opts.get::<Value>("dark").map_err(|e| e.to_string())? {
        Value::Nil => false,
        Value::Boolean(dark) => dark,
        other => return Err(format!("`dark` must be a boolean, got {}", other.type_name())),
    };
    let variant = match opts.get::<Value>("variant").map_err(|e| e.to_string())? {
        Value::Nil => M3Variant::TonalSpot,
        Value::String(name) => {
            VARIANTS.into_iter().find(|(known, _)| name.as_bytes() == known.as_bytes()).map(|(_, v)| v).ok_or_else(
                || {
                    format!(
                        "unknown `variant` `{}`; it takes {}",
                        name.to_string_lossy(),
                        VARIANTS.map(|(name, _)| name).join(", ")
                    )
                },
            )?
        }
        other => return Err(format!("`variant` must be a string, got {}", other.type_name())),
    };
    let contrast = match opts.get::<Value>("contrast").map_err(|e| e.to_string())? {
        Value::Nil => 0.0,
        Value::Integer(n) => n as f64,
        Value::Number(n) => n,
        other => return Err(format!("`contrast` must be a number, got {}", other.type_name())),
    };
    if !(-1.0..=1.0).contains(&contrast) {
        return Err(format!("`contrast` must be -1 to 1, got {contrast}"));
    }
    Ok((dark, variant, contrast))
}

fn swatch(entry: &Table) -> Result<(Rgb, f64), String> {
    let color: String = entry.get("color").map_err(|_| "`color` must be a string".to_string())?;
    let rgb = parse(&color).ok_or_else(|| format!("`color` must be #RRGGBB, got `{color}`"))?;
    let share: f64 = entry.get("share").map_err(|_| "`share` must be a number".to_string())?;
    if !(share.is_finite() && share > 0.0) {
        return Err(format!("`share` must be above 0, got {share}"));
    }
    Ok((rgb, share))
}

pub fn register(lua: &Lua) -> mlua::Result<()> {
    lua_fn!(
        lua,
        /// Material 3 colour roles from one seed colour, as Material Color Utilities builds them.
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#palettescheme)
        fn palette.scheme(
            _lua,
            /// `#RRGGBB`.
            seed: As<String, Color>,
            /// `dark` default `false`; `variant` default `"tonal_spot"`; `contrast` -1 to 1, default 0.
            /// Anything else raises.
            opts: As<Option<Table>, Option<Options>>,
        ) -> Roles {
            let rgb = parse(&seed.0)
                .ok_or_else(|| mlua::Error::runtime(format!("palette.scheme: seed must be #RRGGBB, got `{}`", seed.0)))?;
            let (dark, variant, contrast) =
                options(opts.0).map_err(|detail| mlua::Error::runtime(format!("palette.scheme: options: {detail}")))?;
            Ok(generate(rgb, variant, dark, contrast))
        }
    )?;
    lua_fn!(
        lua,
        /// Up to 4 seed colours ranked by Material 3's Score: chromatic, common and far apart in hue.
        /// `#4285F4` when none qualifies.
        /// [docs](https://anasgets111.github.io/mantle/guide/scripting.html#palettescore)
        fn palette.score(
            _lua,
            /// `palette.quantize`'s swatches; only the ratios of their `share`s count.
            swatches: As<Vec<Table>, Vec<PaletteSwatch>>,
        ) -> As<Vec<String>, Vec<Color>> {
            let colors = swatches
                .0
                .iter()
                .enumerate()
                .map(|(at, entry)| {
                    swatch(entry).map_err(|detail| mlua::Error::runtime(format!("palette.score: swatch {}: {detail}", at + 1)))
                })
                .collect::<mlua::Result<Vec<_>>>()?;
            Ok(As(score(&colors).into_iter().map(hex).collect(), PhantomData))
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> Lua {
        let lua = Lua::new();
        lua.globals().set("palette", lua.create_table().unwrap()).unwrap();
        register(&lua).unwrap();
        lua
    }

    /// Google's own vectors: `SchemeTonalSpotTests.swift` (`test3rdPartyLightScheme`,
    /// `test3rdPartyDarkScheme`) and `testLightContentSchemeFromHighChromaColor`, in
    /// material-color-utilities.
    #[test]
    fn schemes_match_material_color_utilities() {
        let lua = lua();
        let roles = |code: &str| -> Vec<String> {
            let t: Table = lua.load(code).eval().unwrap();
            ["primary", "secondary", "tertiary", "surface", "on_surface"].map(|r| t.get(r).unwrap()).to_vec()
        };
        assert_eq!(
            roles(r##"return palette.scheme("#6750A4")"##),
            ["#65558F", "#625B71", "#7E5260", "#FDF7FF", "#1D1B20"]
        );
        assert_eq!(
            roles(r##"return palette.scheme("#6750a4", { dark = true })"##),
            ["#CFBDFE", "#CBC2DB", "#EFB8C8", "#141218", "#E6E0E9"]
        );
        let content: String =
            lua.load(r##"return palette.scheme("#FA2BEC", { variant = "content" }).primary"##).eval().unwrap();
        assert_eq!(content, "#A7009E");
        let high: String = lua.load(r##"return palette.scheme("#6750A4", { contrast = 1 }).primary"##).eval().unwrap();
        assert_ne!(high, "#65558F", "contrast must reach the scheme");
    }

    /// Upstream `score_test`'s vectors, at the defaults this port fixes.
    #[test]
    fn score_ranks_like_material_color_utilities() {
        let ranked = |pairs: &[(u32, f64)]| -> Vec<String> {
            score(&pairs.iter().map(|&(rgb, weight)| (Rgb::from_u32(rgb), weight)).collect::<Vec<_>>())
                .into_iter()
                .map(hex)
                .collect()
        };
        assert_eq!(ranked(&[(0xFF0000, 1.0), (0x00FF00, 1.0), (0x0000FF, 1.0)]), ["#FF0000", "#00FF00", "#0000FF"]);
        assert_eq!(ranked(&[(0x000000, 1.0), (0xFFFFFF, 1.0), (0x0000FF, 1.0)]), ["#0000FF"]);
        assert_eq!(ranked(&[(0x000000, 1.0)]), ["#4285F4"]);
        assert_eq!(ranked(&[]), ["#4285F4"]);
        assert_eq!(ranked(&[(0x008772, 1.0), (0x318477, 1.0)]), ["#008772"]);
        assert_eq!(
            ranked(&[(0xD33881, 14.0), (0x3205CC, 77.0), (0x0B48CF, 36.0), (0xA08F5D, 81.0)]),
            ["#3205CC", "#A08F5D", "#D33881"]
        );
    }

    #[test]
    fn score_reads_quantize_swatches() {
        let top: String = lua()
            .load(r##"return palette.score({ { color = "#000000", share = 0.5 }, { color = "#0000FF", share = 0.5 } })[1]"##)
            .eval()
            .unwrap();
        assert_eq!(top, "#0000FF");
    }

    #[test]
    fn bad_input_raises_naming_the_problem() {
        let lua = lua();
        for (code, expected) in [
            (r#"palette.scheme("red")"#, "seed must be #RRGGBB, got `red`"),
            (r##"palette.scheme("#6750A4FF")"##, "seed must be #RRGGBB"),
            (r##"palette.scheme("#6750A4", { variant = "loud" })"##, "unknown `variant` `loud`; it takes tonal_spot,"),
            (r##"palette.scheme("#6750A4", { contrast = 2 })"##, "`contrast` must be -1 to 1, got 2"),
            (r##"palette.scheme("#6750A4", { contrast = 0/0 })"##, "`contrast` must be -1 to 1"),
            (r##"palette.scheme("#6750A4", { dark = "yes" })"##, "`dark` must be a boolean, got string"),
            (r##"palette.scheme("#6750A4", { darke = true })"##, "unknown key `darke`"),
            (r#"palette.score({ { color = "blue", share = 1 } })"#, "swatch 1: `color` must be #RRGGBB"),
            (r##"palette.score({ { color = "#0000FF", share = 0 } })"##, "swatch 1: `share` must be above 0"),
        ] {
            let err = lua.load(code).exec().unwrap_err().to_string();
            assert!(err.contains(expected), "`{code}` raised `{err}`, expected `{expected}`");
        }
    }
}
