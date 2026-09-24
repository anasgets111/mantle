//! Median cut (ADR-0249).

use std::path::Path;

use super::MAX_DECODE_EDGE;
use super::budget::Charge;
use super::decode::decode_within_limits;
use super::thumbnails;

type Rgb = [u8; 3];

struct Bucket {
    pixels: Vec<Rgb>,
}

impl Bucket {
    fn widest_channel(&self) -> (usize, u8) {
        let mut min = [u8::MAX; 3];
        let mut max = [0u8; 3];
        for pixel in &self.pixels {
            for c in 0..3 {
                min[c] = min[c].min(pixel[c]);
                max[c] = max[c].max(pixel[c]);
            }
        }
        let ranges = [max[0] - min[0], max[1] - min[1], max[2] - min[2]];
        // `max_by_key` keeps the last of equal maxima, so a tie prefers green, then red, then blue.
        let channel = [2, 0, 1].into_iter().max_by_key(|&c| ranges[c]).unwrap();
        (channel, ranges[channel])
    }

    /// Leaves a bucket of one colour whole, so an image with few colours returns fewer than
    /// `2^depth` rather than duplicates.
    fn split(mut self) -> Result<(Bucket, Bucket), Bucket> {
        if self.pixels.len() < 2 {
            return Err(self);
        }
        let (channel, range) = self.widest_channel();
        if range == 0 {
            return Err(self);
        }
        self.pixels.sort_unstable_by_key(|p| p[channel]);
        let right = self.pixels.split_off(self.pixels.len() / 2);
        Ok((Bucket { pixels: self.pixels }, Bucket { pixels: right }))
    }

    fn mean(&self) -> Rgb {
        let mut sum = [0u64; 3];
        for pixel in &self.pixels {
            for c in 0..3 {
                sum[c] += u64::from(pixel[c]);
            }
        }
        let n = self.pixels.len() as u64;
        [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]
    }
}

/// `(pixel count, colour)` pairs, most common first. `rescale_size` 0 skips the downscale.
pub(crate) fn quantize_file(
    path: &Path,
    depth: u8,
    rescale_size: u32,
    cache_root: Option<&Path>,
) -> Result<Vec<(u32, Rgb)>, String> {
    let rgba = load_rgba(path, rescale_size, cache_root)?;
    let pixels: Vec<Rgb> = rgba.pixels().filter(|p| p.0[3] != 0).map(|p| [p.0[0], p.0[1], p.0[2]]).collect();
    if pixels.is_empty() {
        return Err("no opaque pixels to quantize".to_string());
    }

    let mut buckets = vec![Bucket { pixels }];
    for _ in 0..depth {
        let mut next = Vec::with_capacity(buckets.len() * 2);
        for bucket in buckets {
            match bucket.split() {
                Ok((left, right)) => {
                    next.push(left);
                    next.push(right);
                }
                Err(unsplit) => next.push(unsplit),
            }
        }
        buckets = next;
    }

    let mut colors: Vec<(u32, Rgb)> = buckets.into_iter().map(|b| (b.pixels.len() as u32, b.mean())).collect();
    colors.sort_unstable_by_key(|(count, _)| std::cmp::Reverse(*count));
    Ok(colors)
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
    let decoded = if rescale_size > 0 && decoded.width().max(decoded.height()) > rescale_size {
        decoded.thumbnail(rescale_size, rescale_size)
    } else {
        decoded
    };
    Ok(decoded.into_rgba8())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_zero_averages_the_whole_image() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("solid.png");
        ::image::RgbaImage::from_pixel(4, 2, ::image::Rgba([10, 20, 30, 255])).save(&path).unwrap();

        let colors = quantize_file(&path, 0, 0, None).unwrap();
        assert_eq!(colors, vec![(8, [10, 20, 30])]);
    }

    #[test]
    fn depth_one_on_a_two_color_image_splits_along_the_widest_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("halves.png");
        let mut image = ::image::RgbaImage::new(4, 2);
        for y in 0..2 {
            for x in 0..4 {
                let pixel = if x < 2 { [255, 0, 0, 255] } else { [0, 0, 255, 255] };
                image.put_pixel(x, y, ::image::Rgba(pixel));
            }
        }
        image.save(&path).unwrap();

        let mut colors = quantize_file(&path, 1, 0, None).unwrap();
        colors.sort_unstable_by_key(|(_, rgb)| *rgb);
        assert_eq!(colors, vec![(4, [0, 0, 255]), (4, [255, 0, 0])]);
    }

    #[test]
    fn fully_transparent_pixels_are_excluded_from_the_average() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("border.png");
        let mut image = ::image::RgbaImage::from_pixel(4, 4, ::image::Rgba([0, 0, 0, 0]));
        for y in 1..3 {
            for x in 1..3 {
                image.put_pixel(x, y, ::image::Rgba([100, 150, 200, 255]));
            }
        }
        image.save(&path).unwrap();

        let colors = quantize_file(&path, 0, 0, None).unwrap();
        assert_eq!(colors, vec![(4, [100, 150, 200])], "the transparent border must not skew the average toward black");
    }

    #[test]
    fn rescale_bounds_decode_cost_without_changing_a_uniform_colors_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wallpaper.png");
        ::image::RgbaImage::from_pixel(512, 512, ::image::Rgba([50, 60, 70, 255])).save(&path).unwrap();

        let colors = quantize_file(&path, 0, 64, None).unwrap();
        assert_eq!(colors[0].1, [50, 60, 70]);
        assert!(colors[0].0 <= 64 * 64, "a 64px rescale must not decode at full 512px resolution");
    }

    #[test]
    fn a_missing_source_fails_cleanly() {
        assert!(quantize_file(Path::new("/nonexistent/wall.png"), 3, 128, None).is_err());
    }

    #[test]
    fn a_source_past_the_decode_edge_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.png");
        ::image::RgbaImage::from_pixel(MAX_DECODE_EDGE + 1, 1, ::image::Rgba([1, 2, 3, 255])).save(&path).unwrap();
        assert!(quantize_file(&path, 3, 0, None).is_err());
    }

    #[test]
    fn a_covering_thumbnail_is_read_instead_of_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("wall.png");
        ::image::RgbaImage::from_pixel(2000, 2000, ::image::Rgba([1, 2, 3, 255])).save(&source).unwrap();
        let cache = dir.path().join("cache");
        let slot = thumbnails::Slot::for_file(&cache, &source, (128, 128)).unwrap();
        slot.write(&[9, 8, 7, 255].repeat(4), 2, 2).unwrap();

        let colors = quantize_file(&source, 0, 128, Some(&cache)).unwrap();
        assert_eq!(
            colors,
            vec![(4, [9, 8, 7])],
            "the thumbnail's pixels must win over the (differently coloured) source"
        );
    }
}
