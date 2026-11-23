//! A dashed border's gaps: the slices of the band [`super::shape::paint_border`] paints that fall
//! between dashes laid along its outer edge, for the offscreen pass to cut out of the solid border.

use femtovg::{Path, Solidity};

use super::shape::Station;
use super::vector_path::{Dashes, MAX_DASHES};

/// What a dashed border paints.
pub(super) enum Cut {
    /// No edge paints, or every dash is zero-length.
    Nothing,
    /// No gap, or past the dash ceiling: the solid border.
    Solid,
    /// The solid border less these gaps, each a run of convex pieces: femtovg antialiases one
    /// convex contour exactly and any other fill over a 2 px fringe, so pieces drawn additively
    /// into a mask meet without a seam and end where the dash does.
    Slices(Vec<Path>),
}

/// Decides how a border of `contours`, from [`super::shape::border_stations`], takes `dash`, and
/// builds its gap slices. Each contour starts the pattern afresh; a gap across its start is one.
pub(super) fn cut(contours: &[Vec<Station>], dash: &Dashes) -> Cut {
    let pairs = dash.lengths.as_chunks::<2>().0;
    let count = |of: fn(&[f64; 2]) -> f64| pairs.iter().filter(|pair| of(pair) > 0.0).count() as f64;
    let (dashes, gaps) = (count(|[on, _]| *on), count(|[_, off]| *off));
    if contours.is_empty() || dashes == 0.0 {
        return Cut::Nothing;
    }
    let lengths: Vec<Vec<f64>> = contours.iter().map(|stations| along(stations)).collect();
    let period: f64 = dash.lengths.iter().sum();
    // A contour holds at most its length in whole periods and a part period at either end.
    let most = lengths.iter().map(|along| along[along.len() - 1] / period + 2.0).sum::<f64>() * dashes.max(gaps);
    if gaps == 0.0 || most > MAX_DASHES {
        return Cut::Solid;
    }
    let mut slices = Vec::new();
    for (c, (stations, along)) in contours.iter().zip(&lengths).enumerate() {
        let normals = normals(stations);
        let others: Vec<_> =
            contours.iter().enumerate().filter(|&(o, _)| o != c).map(|(_, other)| other.as_slice()).collect();
        for span in spans(along[along.len() - 1], pairs, dash.offset) {
            slice(&mut slices, (stations, &others), along, &normals, span);
        }
    }
    Cut::Slices(slices)
}

/// Each station's distance round the outer edge from the first, then the closed contour's length.
fn along(stations: &[Station]) -> Vec<f64> {
    let mut at = vec![0.0];
    for (i, (outer, _)) in stations.iter().enumerate() {
        let next = stations[(i + 1) % stations.len()].0;
        at.push(at[i] + f64::from((next.0 - outer.0).hypot(next.1 - outer.1)));
    }
    at
}

/// How far `p` lies from the band of `stations`, another contour's: 0 inside it.
fn room(p: Point, stations: &[Station]) -> f32 {
    let n = stations.len();
    let edges = |side: fn(&Station) -> Point| (0..n).map(move |k| (side(&stations[k]), side(&stations[(k + 1) % n])));
    let (outer, inner) = (edges(|s| s.0), edges(|s| s.1));
    // Even-odd: inside the band is inside one of its two edges and not the other.
    let crossings = outer
        .clone()
        .chain(inner.clone())
        .filter(|&(a, b)| (a.1 > p.1) != (b.1 > p.1) && p.0 < a.0 + (p.1 - a.1) / (b.1 - a.1) * (b.0 - a.0));
    if crossings.count() % 2 == 1 {
        return 0.0;
    }
    let distance = |(a, b): (Point, Point)| {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy).max(f32::EPSILON)).clamp(0.0, 1.0);
        (a.0 + dx * t - p.0).hypot(a.1 + dy * t - p.1)
    };
    outer.chain(inner).map(distance).fold(f32::INFINITY, f32::min)
}

/// Each station's unit normal to the outer edge, away from the band: across from the previous outer
/// point to the next, past a fan's repeated one, so it turns smoothly round a corner.
fn normals(stations: &[Station]) -> Vec<Point> {
    let n = stations.len();
    let across: Vec<Point> = (0..n)
        .map(|k| {
            let outer = stations[k].0;
            let prev = (1..n).map(|back| stations[(k + n - back) % n].0).find(|&o| !same(o, outer)).unwrap_or(outer);
            let next = (1..n).map(|ahead| stations[(k + ahead) % n].0).find(|&o| !same(o, outer)).unwrap_or(outer);
            let (dx, dy) = (next.0 - prev.0, next.1 - prev.1);
            let run = dx.hypot(dy).max(f32::EPSILON);
            (dy / run, -dx / run)
        })
        .collect();
    // One side for the whole contour, the side its band's depth mostly points.
    let side: f32 = across.iter().zip(stations).map(|(e, (o, i))| e.0 * (o.0 - i.0) + e.1 * (o.1 - i.1)).sum();
    across.into_iter().map(|(x, y)| if side < 0.0 { (-x, -y) } else { (x, y) }).collect()
}

fn same(a: (f32, f32), b: (f32, f32)) -> bool {
    (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
}

/// The gaps of `pairs` from `offset` px in, along a closed contour `length` px round, abutting
/// gaps joined; the last runs past `length` when it joins the first across the start.
fn spans(length: f64, pairs: &[[f64; 2]], offset: f64) -> Vec<(f64, f64)> {
    const SAME: f64 = 1e-6;
    let period: f64 = pairs.iter().flatten().sum();
    let mut spans: Vec<(f64, f64)> = Vec::new();
    let mut base = -offset.rem_euclid(period);
    while base < length {
        let mut at = base;
        for [on, off] in pairs {
            let (from, to) = ((at + on).max(0.0), (at + on + off).min(length));
            match spans.last_mut() {
                Some(last) if from - last.1 < SAME && from < to => last.1 = to,
                _ if from < to => spans.push((from, to)),
                _ => {}
            }
            at += on + off;
        }
        base += period;
    }
    if spans.len() > 1 && spans[0].0 < SAME && length - spans[spans.len() - 1].1 < SAME {
        let first = spans.remove(0);
        let last = spans.len() - 1;
        spans[last].1 = length + first.1;
    }
    spans
}

/// Appends the band from `span.0` to `span.1` round the outer edge as convex pieces. Each reaches
/// out past the band by its depth and a pixel across the outer edge, where nothing else in the
/// pass paints, and in past its inner edge by a pixel, none at a corner's inner point and short of
/// another contour's band, as in a frame's. femtovg shrinks a convex fill by half a pixel each side
/// before its fringe, which turns a piece under a pixel wide inside out, so each piece runs as far
/// as it stays convex and one left under a pixel reaches a pixel along the gap.
/// ponytail: the inward pixel crosses a band with no hole, as `radius=10,border_width=24` on a
/// 48 px box (4 pixels), and the outward reach a scoop's end beside an unpainted edge (2); where a
/// dash ends on a taper or a pointed corner, femtovg's antialiasing misses single pixels by up to
/// 183 of 255, and bands about a pixel apart keep a quarter of a pixel on their inner edges. Clip
/// pieces to the band, or cover them exactly in a shader, if any matters. Also unhandled: borders
/// wider than half the box, dashes under 0.0001 px, and frame bands that overlap.
fn slice(
    out: &mut Vec<Path>,
    (stations, others): (&[Station], &[&[Station]]),
    along: &[f64],
    normals: &[Point],
    (from, to): (f64, f64),
) {
    let n = stations.len();
    let length = along[n];
    let station = |k: usize| stations[k % n];
    let at = |k: usize| along[k % n] + (k / n) as f64 * length;
    // The band's outer and inner points `s` px round, moved past the band, and as they lie.
    let point = |s: f64| {
        // A gap's end a rounding error short of a station is at it, so the slices either side of the
        // station move its points alike.
        let k = (0..2 * n).find(|&k| at(k + 1) > s + 1e-4).unwrap_or(0);
        let ((o0, i0), (o1, i1)) = (station(k), station(k + 1));
        let t = ((s - at(k)) / (at(k + 1) - at(k))).max(0.0) as f32;
        let mix = |a: (f32, f32), b: (f32, f32)| (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
        let (outer, inner) = (mix(o0, o1), mix(i0, i1));
        // Out by the band's depth from the inner point, which reaches the square a corner a radius
        // under the band's width leaves, then a pixel across the outer edge, which also covers where
        // that line runs along the edge, as where a lone edge's colour tapers round a corner.
        let (dx_depth, dy_depth) = (outer.0 - inner.0, outer.1 - inner.1);
        let normal = mix(normals[k % n], normals[(k + 1) % n]);
        // In a pixel, short of another contour's band by the half pixel its antialiasing spreads.
        let reach = (others.iter().map(|other| room(inner, other)).fold(1.5, f32::min) - 0.5).max(0.0);
        // Across the inner edge, away from the outer one; none from a corner's inner point, which its
        // fan's stations share, so the slices either side meet there and don't cross past it.
        let chord = (i1.0 - i0.0, i1.1 - i0.1);
        let run = chord.0.hypot(chord.1);
        let into = match () {
            () if run < 1e-3 || same(inner, i0) && same(i0, station(k + n - 1).1) => (0.0, 0.0),
            () => {
                let into = (chord.1 / run * reach, -chord.0 / run * reach);
                if into.0 * dx_depth + into.1 * dy_depth > 0.0 { (-into.0, -into.1) } else { into }
            }
        };
        let moved =
            ((outer.0 + dx_depth + normal.0, outer.1 + dy_depth + normal.1), (inner.0 + into.0, inner.1 + into.1));
        (moved, (outer, inner))
    };
    let mut along_gap = vec![from];
    along_gap.extend((0..2 * n).map(at).filter(|&s| s > from && s < to));
    along_gap.push(to);
    along_gap.dedup_by(|s, kept| *s - *kept < 1e-4);
    let points: Vec<_> = along_gap.iter().map(|&s| point(s)).collect();
    let last = points.len() - 1;
    let mut a = 0;
    while a < last {
        let mut b = a + 1;
        while b < last && convex(&ring(&points[a..=b + 1], Reach::Moved)) {
            b += 1;
        }
        let (s_a, s_b) = (along_gap[a], along_gap[b]);
        // Under a pixel, a piece reaches a pixel along the gap, forward but at the gap's end; the
        // overlap adds to no more than the gap.
        let run = if s_b - s_a >= 1.0 {
            points[a..=b].to_vec()
        } else {
            let end = (s_a + 1.0).min(to);
            vec![point((end - 1.0).max(from)), point(end)]
        };
        // On an inner curve under 1 px round the moved inner points pass each other: keep its edge.
        let corners =
            Some(ring(&run, Reach::Moved)).filter(|ring| convex(ring)).unwrap_or_else(|| ring(&run, Reach::Outward));
        // A piece holding none of the band, along an edge with no depth, would only antialias into
        // its neighbour.
        if corners.len() > 2 && area(&ring(&run, Reach::Band)) > 0.01 {
            let mut piece = Path::new();
            piece.move_to(corners[0].0, corners[0].1);
            for &(x, y) in &corners[1..] {
                piece.line_to(x, y);
            }
            piece.close();
            piece.solidity(Solidity::Solid);
            out.push(piece);
        }
        a = b;
    }
}

type Point = (f32, f32);

/// How [`ring`] places a run's points.
#[derive(Clone, Copy, PartialEq)]
enum Reach {
    /// Both moved.
    Moved,
    /// The outer moved, the inner where the band ends.
    Outward,
    /// The band itself.
    Band,
}

/// A run of points as one loop, out along the outer points and back along the inner ones, placed
/// by `reach`, repeats dropped.
fn ring(run: &[(Station, Station)], reach: Reach) -> Vec<Point> {
    let outer = |p: &(Station, Station)| if reach == Reach::Band { p.1.0 } else { p.0.0 };
    let inner = |p: &(Station, Station)| if reach == Reach::Moved { p.0.1 } else { p.1.1 };
    let mut ring: Vec<Point> = run.iter().map(outer).chain(run.iter().rev().map(inner)).collect();
    ring.dedup_by(|p, kept| same(*p, *kept));
    while ring.len() > 1 && same(ring[0], ring[ring.len() - 1]) {
        ring.pop();
    }
    ring
}

/// `ring`'s area.
fn area(ring: &[Point]) -> f32 {
    // From the first point, so the sum keeps its precision far from the origin.
    let (m, o) = (ring.len(), ring[0]);
    let at = |i: usize| (ring[i % m].0 - o.0, ring[i % m].1 - o.1);
    (1..m).map(|i| at(i).0 * at(i + 1).1 - at(i + 1).0 * at(i).1).sum::<f32>().abs() / 2.0
}

/// Whether `ring` turns one way all round, as femtovg needs to antialias a fill exactly.
fn convex(ring: &[Point]) -> bool {
    let m = ring.len();
    let mut turn = 0.0;
    for i in 0..m {
        let (a, b, c) = (ring[i], ring[(i + 1) % m], ring[(i + 2) % m]);
        let cross = (b.0 - a.0) * (c.1 - b.1) - (b.1 - a.1) * (c.0 - b.0);
        if cross.abs() < 1e-4 {
            continue;
        }
        if turn == 0.0 {
            turn = cross.signum();
        } else if cross.signum() != turn {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_s_area_is_the_same_anywhere_on_screen() {
        let at = |x: f32, y: f32| area(&[(x, y), (x + 0.5, y), (x + 0.5, y + 1.0), (x, y + 1.0)]);
        assert_eq!(at(0.75, 0.0), 0.5);
        assert_eq!(at(3800.75, 2100.0), 0.5);
    }

    #[test]
    fn gaps_follow_every_pair_and_join_across_a_zero_dash_and_the_start() {
        assert_eq!(spans(20.0, &[[4.0, 2.0], [6.0, 4.0]], 0.0), [(4.0, 6.0), (12.0, 16.0)]);
        assert_eq!(spans(20.0, &[[4.0, 4.0], [0.0, 4.0]], 0.0), [(4.0, 12.0), (16.0, 20.0)]);
        // From 6 px in, the gap at 18-20 runs on into the first 4 px.
        assert_eq!(spans(20.0, &[[4.0, 6.0]], 6.0), [(8.0, 14.0), (18.0, 24.0)]);
        // One gap longer than the contour covers it once.
        assert_eq!(spans(10.0, &[[1.0, 100.0]], 1.0), [(0.0, 10.0)]);
    }
}
