//! Dominant colours of an image (ADR-0249).

use std::path::Path;

use material_colors::color::Rgb;
use material_colors::quantize::{Quantizer, QuantizerCelebi, QuantizerMap, QuantizerWu};

use super::MAX_DECODE_EDGE;
use super::budget::Charge;
use super::decode::{decode_within_limits, downscale, fit_inside};
use super::thumbnails;

/// Material's quantizers (ADR-0249, ADR-0319).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Method {
    /// Wu, then WSMeans k-means in Lab.
    Celebi,
    /// Wu alone.
    Wu,
}

impl Method {
    pub(crate) const NAMES: [(&str, Method); 2] = [("celebi", Method::Celebi), ("wu", Method::Wu)];
}

/// `(pixel count, colour)` pairs, most common first. `rescale_size` 0 skips the downscale.
pub(crate) fn quantize_file(
    path: &Path,
    depth: u8,
    method: Method,
    rescale_size: u32,
    cache_root: Option<&Path>,
) -> Result<Vec<(u32, Rgb)>, String> {
    let rgba = load_rgba(path, rescale_size, cache_root)?;
    let pixels: Vec<Rgb> = rgba.pixels().filter(|p| p.0[3] != 0).map(|p| Rgb::new(p.0[0], p.0[1], p.0[2])).collect();
    if pixels.is_empty() {
        return Err("no opaque pixels to quantize".to_string());
    }

    let max_colors = 1usize << depth;
    let mut colors: Vec<(u32, Rgb)> = match method {
        Method::Celebi => QuantizerCelebi::quantize(&pixels, max_colors)
            .color_to_count
            .into_iter()
            .map(|(rgb, count)| (count, rgb))
            .collect(),
        Method::Wu => {
            // Wu returns only its palette (every count 0), so each distinct colour joins its nearest entry.
            let mut palette: Vec<(u32, Rgb)> =
                QuantizerWu::quantize(&pixels, max_colors).color_to_count.into_keys().map(|rgb| (0, rgb)).collect();
            for (rgb, count) in QuantizerMap::quantize(&pixels, max_colors).color_to_count {
                let nearest = palette.iter_mut().min_by_key(|(_, p)| distance(*p, rgb)).expect("a non-empty image");
                nearest.0 += count;
            }
            palette.retain(|(count, _)| *count > 0);
            palette
        }
    };
    // The map's order is a hash order; the colour breaks count ties so the result is stable.
    colors.sort_unstable_by_key(|(count, rgb)| (std::cmp::Reverse(*count), rgb.as_u32()));
    Ok(colors)
}

fn distance(a: Rgb, b: Rgb) -> i32 {
    [(a.red, b.red), (a.green, b.green), (a.blue, b.blue)]
        .into_iter()
        .map(|(x, y)| (i32::from(x) - i32::from(y)).pow(2))
        .sum()
}

/// Prefers a thumbnail already on disk that covers `rescale_size`.
fn load_rgba(path: &Path, rescale_size: u32, cache_root: Option<&Path>) -> Result<::image::RgbaImage, String> {
    if rescale_size > 0
        && let Some(cache_root) = cache_root
        && let Some((raw, width, height)) = thumbnails::Slot::for_file(cache_root, path, (rescale_size, rescale_size))
            .and_then(|slot| slot.read_valid())
        && let Some(image) = ::image::RgbaImage::from_raw(width, height, raw)
    {
        return Ok(image);
    }
    let (decoded, _permit) = decode_within_limits(path, MAX_DECODE_EDGE, Charge::Free, &|| true)?;
    if rescale_size > 0 && decoded.width().max(decoded.height()) > rescale_size {
        return downscale(&decoded, fit_inside(decoded.width(), decoded.height(), rescale_size));
    }
    Ok(decoded.into_rgba8())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(hex: u32) -> Rgb {
        Rgb::from_u32(hex)
    }

    fn save(name: &str, image: ::image::RgbaImage) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        image.save(&path).unwrap();
        (dir, path)
    }

    #[test]
    fn a_solid_image_is_one_colour_under_either_method() {
        let (_dir, path) = save("solid.png", ::image::RgbaImage::from_pixel(4, 2, ::image::Rgba([10, 20, 30, 255])));
        for (_, method) in Method::NAMES {
            assert_eq!(quantize_file(&path, 3, method, 0, None).unwrap(), vec![(8, rgb(0x0A141E))], "{method:?}");
        }
    }

    #[test]
    fn two_colours_come_back_most_common_first_with_ties_by_colour() {
        let mut image = ::image::RgbaImage::from_pixel(4, 2, ::image::Rgba([0, 0, 255, 255]));
        for x in 0..3 {
            image.put_pixel(x, 0, ::image::Rgba([255, 0, 0, 255]));
        }
        let (_dir, path) = save("two.png", image);
        for (_, method) in Method::NAMES {
            let colors = quantize_file(&path, 3, method, 0, None).unwrap();
            assert_eq!(colors, vec![(5, rgb(0x0000FF)), (3, rgb(0xFF0000))], "{method:?}");
        }

        let tied = ::image::RgbaImage::from_fn(4, 2, |x, _| {
            ::image::Rgba(if x < 2 { [255, 0, 0, 255] } else { [0, 0, 255, 255] })
        });
        let (_dir, path) = save("tied.png", tied);
        let colors = quantize_file(&path, 3, Method::Wu, 0, None).unwrap();
        assert_eq!(colors, vec![(4, rgb(0x0000FF)), (4, rgb(0xFF0000))]);
    }

    #[test]
    fn depth_zero_merges_a_busy_image_into_one_colour() {
        let noisy =
            ::image::RgbaImage::from_fn(16, 16, |x, y| ::image::Rgba([(x * 16) as u8, (y * 16) as u8, 90, 255]));
        let (_dir, path) = save("noisy.png", noisy);
        for (_, method) in Method::NAMES {
            let colors = quantize_file(&path, 0, method, 0, None).unwrap();
            assert_eq!(colors.len(), 1, "{method:?}");
            assert_eq!(colors[0].0, 256, "{method:?}");
        }
    }

    #[test]
    fn depth_caps_the_colour_count() {
        let noisy = ::image::RgbaImage::from_fn(32, 32, |x, y| ::image::Rgba([(x * 8) as u8, (y * 8) as u8, 90, 255]));
        let (_dir, path) = save("noisy.png", noisy);
        for (_, method) in Method::NAMES {
            assert!(quantize_file(&path, 2, method, 0, None).unwrap().len() <= 4, "{method:?}");
        }
    }

    #[test]
    fn fully_transparent_pixels_are_excluded_from_the_average() {
        let mut image = ::image::RgbaImage::from_pixel(4, 4, ::image::Rgba([0, 0, 0, 0]));
        for y in 1..3 {
            for x in 1..3 {
                image.put_pixel(x, y, ::image::Rgba([100, 150, 200, 255]));
            }
        }
        let (_dir, path) = save("border.png", image);

        let colors = quantize_file(&path, 0, Method::Celebi, 0, None).unwrap();
        assert_eq!(colors, vec![(4, rgb(0x6496C8))], "the transparent border must not count");
    }

    #[test]
    fn rescale_bounds_decode_cost_without_changing_a_uniform_colors_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallpaper.png");
        ::image::RgbaImage::from_pixel(512, 512, ::image::Rgba([50, 60, 70, 255])).save(&path).unwrap();

        let colors = quantize_file(&path, 0, Method::Celebi, 64, None).unwrap();
        assert_eq!(colors[0].1, rgb(0x323C46));
        assert!(colors[0].0 <= 64 * 64, "a 64px rescale must not decode at full 512px resolution");
    }

    #[test]
    fn a_missing_source_fails_cleanly() {
        assert!(quantize_file(Path::new("/nonexistent/wall.png"), 3, Method::Celebi, 128, None).is_err());
    }

    #[test]
    fn a_source_past_the_decode_edge_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.png");
        ::image::RgbaImage::from_pixel(MAX_DECODE_EDGE + 1, 1, ::image::Rgba([1, 2, 3, 255])).save(&path).unwrap();
        assert!(quantize_file(&path, 3, Method::Celebi, 0, None).is_err());
    }

    #[test]
    fn a_covering_thumbnail_is_read_instead_of_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("wall.png");
        ::image::RgbaImage::from_pixel(2000, 2000, ::image::Rgba([1, 2, 3, 255])).save(&source).unwrap();
        let cache = dir.path().join("cache");
        let slot = thumbnails::Slot::for_file(&cache, &source, (128, 128)).unwrap();
        slot.write(&[9, 8, 7, 255].repeat(4), 2, 2).unwrap();

        let colors = quantize_file(&source, 0, Method::Celebi, 128, Some(&cache)).unwrap();
        assert_eq!(
            colors,
            vec![(4, rgb(0x090807))],
            "the thumbnail's pixels must win over the (differently coloured) source"
        );
    }
}
