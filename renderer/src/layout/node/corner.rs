//! Continuous corners: Figma's corner-smoothing model, the one behind Apple's, after the
//! MIT-licensed `figma-squircle` (phamfoo). A corner of radius `r` and smoothing `s` spreads
//! `(1 + s) * r` along each side as a cubic transition, a shorter circular arc and a mirrored
//! transition; `s = 0` is the circular arc.

use std::f32::consts::{FRAC_1_SQRT_2, FRAC_PI_2, FRAC_PI_4, LN_2, SQRT_2};

/// One smoothed corner in its own frame: the corner at the origin, the box towards +x and +y.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Squircle {
    /// How far along each side the corner reaches.
    pub reach: f32,
    /// Three cubics, `[start, c1, c2, end, c1, c2, end, ...]`, from the vertical edge's end
    /// `(0, reach)` to the horizontal edge's `(reach, 0)`.
    pub points: [(f32, f32); 10],
    /// The exponent of the superellipse through this corner's endpoints and diagonal midpoint,
    /// `2` for a circle: the shader's stand-in for the outline.
    pub power: f32,
}

impl Squircle {
    /// `smoothing` in `[0, 1]`, cut so the corner fits in `budget`, the share of the shorter
    /// adjacent side it may use (figma-squircle's `roundingAndSmoothingBudget`): the radius stays
    /// and the smoothing gives way.
    pub fn new(radius: f32, smoothing: f32, budget: f32) -> Self {
        let smoothing = smoothing.min(budget / radius - 1.0).max(0.0);
        let reach = (1.0 + smoothing) * radius;
        let sweep = (1.0 - smoothing) * FRAC_PI_2;
        let arc = (sweep / 2.0).sin() * radius * SQRT_2;
        let beta = FRAC_PI_4 * smoothing;
        let c = radius * (beta / 2.0).tan() * beta.cos();
        let d = c * beta.tan();
        let b = (reach - arc - c - d) / 3.0;
        let a = 2.0 * b;
        let far = reach - a - b - c;
        let k = 4.0 / 3.0 * (sweep / 4.0).tan() * radius;
        // Figma's top-right corner mirrored to the top left, from the horizontal edge down.
        let mut points = [
            (reach, 0.0),
            (reach - a, 0.0),
            (reach - a - b, 0.0),
            (far, d),
            (far - k * beta.cos(), d + k * beta.sin()),
            (d + k * beta.sin(), far - k * beta.cos()),
            (d, far),
            (0.0, far + c),
            (0.0, far + b + c),
            (0.0, reach),
        ];
        points.reverse();
        // The arc's midpoint is on the diagonal, a sagitta short of its chord's.
        let mid = (far + d) / 2.0 - radius * (1.0 - (sweep / 2.0).cos()) * FRAC_1_SQRT_2;
        let power = if smoothing == 0.0 { 2.0 } else { LN_2 / (reach / (reach - mid)).ln() };
        Self { reach, points, power }
    }

    /// The same corner with every point moved by `f`, as an inner border edge scales and shifts it.
    pub fn map(mut self, f: impl Fn((f32, f32)) -> (f32, f32)) -> Self {
        self.points = self.points.map(f);
        self
    }

    /// The outline from `from` to `to` of the way through the three cubics (equal shares of the
    /// parameter, not of the length), either direction, as `[start, c1, c2, end]` pieces.
    pub fn span(&self, from: f32, to: f32) -> Vec<[(f32, f32); 4]> {
        let (lo, hi) = (from.min(to) * 3.0, from.max(to) * 3.0);
        let mut pieces: Vec<_> = (0..3)
            .filter_map(|i| {
                let (t0, t1) = ((lo - i as f32).clamp(0.0, 1.0), (hi - i as f32).clamp(0.0, 1.0));
                (t1 > t0).then(|| split(self.cubic(i), t0, t1))
            })
            .collect();
        if from > to {
            pieces.reverse();
            pieces.iter_mut().for_each(|piece| piece.reverse());
        }
        pieces
    }

    /// The cubic `i` (0..3) as `[start, c1, c2, end]`.
    pub fn cubic(&self, i: usize) -> [(f32, f32); 4] {
        std::array::from_fn(|n| self.points[3 * i + n])
    }

    /// How far the outline is from the vertical edge at distance `y` from the horizontal one.
    pub fn inset(&self, y: f32) -> f32 {
        if y >= self.reach {
            return 0.0;
        }
        // Each cubic falls monotonically in y, so bisect the one that spans `y`.
        let q = self.cubic((0..3).find(|&i| self.cubic(i)[3].1 <= y).unwrap_or(2));
        let at = |t: f32, q: [f32; 4]| {
            let u = 1.0 - t;
            u * u * u * q[0] + 3.0 * u * u * t * q[1] + 3.0 * u * t * t * q[2] + t * t * t * q[3]
        };
        let (mut lo, mut hi) = (0.0, 1.0);
        for _ in 0..24 {
            let t = (lo + hi) / 2.0;
            if at(t, q.map(|p| p.1)) > y { lo = t } else { hi = t }
        }
        at((lo + hi) / 2.0, q.map(|p| p.0))
    }
}

/// The part of `cubic` from parameter `t0` to `t1`, by de Casteljau.
fn split(cubic: [(f32, f32); 4], t0: f32, t1: f32) -> [(f32, f32); 4] {
    let lerp = |a: (f32, f32), b: (f32, f32), t: f32| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
    // The piece up to `t`, then the tail of that from `t0 / t`.
    let head = |c: [(f32, f32); 4], t: f32| {
        let (ab, bc, cd) = (lerp(c[0], c[1], t), lerp(c[1], c[2], t), lerp(c[2], c[3], t));
        let (abc, bcd) = (lerp(ab, bc, t), lerp(bc, cd, t));
        [c[0], ab, abc, lerp(abc, bcd, t)]
    };
    let c = head(cubic, t1);
    if t0 <= 0.0 {
        return c;
    }
    let mut back = head([c[3], c[2], c[1], c[0]], 1.0 - t0 / t1);
    back.reverse();
    back
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::Radii;

    fn near(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
    }

    /// Smoothing 0 is the circle: one quarter-turn cubic with the 4/3 tan(pi/8) handle, the
    /// transitions collapsed to points.
    #[test]
    fn no_smoothing_is_the_circular_quarter() {
        let c = Squircle::new(10.0, 0.0, 100.0);
        let k = 4.0 / 3.0 * (std::f32::consts::FRAC_PI_8).tan() * 10.0;
        let expected = [(0.0, 10.0), (0.0, 10.0), (0.0, 10.0), (0.0, 10.0), (0.0, 10.0 - k), (10.0 - k, 0.0)];
        assert!(c.points.iter().zip(expected).all(|(p, e)| near(*p, e)), "{:?}", c.points);
        assert!(near(c.points[6], (10.0, 0.0)) && near(c.points[9], (10.0, 0.0)));
        assert_eq!((c.reach, c.power), (10.0, 2.0));
    }

    /// Radius 20 at smoothing 0.6, worked from figma-squircle's formulas by hand: p = 32, arc
    /// 8.7403, c 4.2782, d 2.1799, b 5.6005, a 11.2010. Its top-right corner starts at
    /// (width - 32, 0) with handles a and a + b along the edge and ends the first cubic at
    /// (a + b + c, d) past the start; this frame is mirrored and walks the other way.
    #[test]
    fn smoothing_matches_the_figma_squircle_reference() {
        let c = Squircle::new(20.0, 0.6, 100.0);
        assert_eq!(c.reach, 32.0);
        let expected = [
            (0.0, 32.0),
            (0.0, 20.7989),
            (0.0, 15.1984),
            (2.1799, 10.9202),
            (4.0973, 7.1569),
            (7.1569, 4.0973),
            (10.9202, 2.1799),
            (15.1984, 0.0),
            (20.7989, 0.0),
            (32.0, 0.0),
        ];
        assert!(c.points.iter().zip(expected).all(|(p, e)| near(*p, e)), "{:?}", c.points);
    }

    /// The radius stays and the smoothing gives way when the side is short: 20 at 0.6 wants 32 but
    /// 24 is all there is, so smoothing becomes 0.2; at 20 it is the circle.
    #[test]
    fn smoothing_is_cut_to_the_budget() {
        assert_eq!(Squircle::new(20.0, 0.6, 24.0).reach, 24.0);
        let flat = Squircle::new(20.0, 0.6, 20.0);
        assert_eq!((flat.reach, flat.power), (20.0, 2.0));
        // Corners share a side by radius: 10 and 30 on a 40 px side leave 10 its quarter.
        let boxed = Radii([10.0, 30.0, 10.0, 30.0], 1.0).squircles(40.0, 100.0);
        assert_eq!(boxed.map(|c| c.unwrap().reach), [10.0, 30.0, 10.0, 30.0]);
        let one = Radii([10.0, 0.0, 10.0, 0.0], 0.6).squircles(100.0, 100.0);
        assert_eq!((one[1], one[3], one[0].unwrap().reach), (None, None, 16.0), "a square corner has no outline");
        assert_eq!(Radii::from(10.0).squircles(100.0, 100.0), [None; 4], "smoothing 0 keeps the circular paths");
    }

    /// Pieces of a span meet end to end, either way round, and `inset` reads the same curve: at
    /// radius 16 and smoothing 1 the outline is 1.007 px down at 12 px along (python reference).
    #[test]
    fn spans_join_up_and_inset_follows_the_outline() {
        let c = Squircle::new(16.0, 1.0, 32.0);
        let whole = c.span(0.0, 1.0);
        assert!(near(whole[0][0], (0.0, 32.0)) && near(whole[2][3], (32.0, 0.0)));
        let (a, b) = (c.span(0.0, 0.4), c.span(0.4, 1.0));
        assert!(near(a.last().unwrap()[3], b[0][0]), "split at 0.4 is continuous");
        let back = c.span(1.0, 0.0);
        assert!(near(back[0][0], (32.0, 0.0)) && near(back[2][3], (0.0, 32.0)));
        assert!((c.inset(1.007) - 12.0).abs() < 0.05, "{}", c.inset(1.007));
        assert_eq!(c.inset(40.0), 0.0);
        assert!((c.inset(0.0) - 32.0).abs() < 1e-3);
    }
}
