//! The progress curves an eased tween runs along: QML's named ones, CSS `cubic-bezier` and `steps`
//! (ADR-0145, ADR-0151).

/// QML's `Easing.Type` names, spelled the same so a
/// `Behavior on width { NumberAnimation { easing.type: Easing.OutCubic } }` ports by dropping the
/// prefix, plus CSS's two curves QML has no name for: an arbitrary cubic Bezier and a step
/// function (ADR-0151).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Easing {
    Linear,
    InQuad,
    OutQuad,
    /// The default: the one the reference config wrote most often, and the one that reads as
    /// motion rather than a snap when nobody chose.
    #[default]
    InOutQuad,
    InCubic,
    OutCubic,
    InOutCubic,
    InQuart,
    OutQuart,
    InOutQuart,
    InQuint,
    OutQuint,
    InOutQuint,
    InSine,
    OutSine,
    InOutSine,
    InExpo,
    OutExpo,
    InOutExpo,
    InCirc,
    OutCirc,
    InOutCirc,
    /// The three that overshoot past the target and settle. The tween clamps the result to the
    /// property's legal range, so a `width` easing to `0` never goes negative into the parser.
    InBack,
    OutBack,
    InOutBack,
    InElastic,
    OutElastic,
    InOutElastic,
    InBounce,
    OutBounce,
    InOutBounce,
    /// CSS `cubic-bezier(x1, y1, x2, y2)`: the two control points of a curve from `(0, 0)` to
    /// `(1, 1)`. Written as `easing = { x1, y1, x2, y2 }`.
    Bezier {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
    },
    /// CSS `steps(n)`: `n` equal jumps, the last landing on the target. Written as
    /// `easing = { steps = n }`.
    Steps(u32),
}

impl Easing {
    pub(super) const NAMES: &[(&str, Easing)] = &[
        ("Linear", Easing::Linear),
        ("InQuad", Easing::InQuad),
        ("OutQuad", Easing::OutQuad),
        ("InOutQuad", Easing::InOutQuad),
        ("InCubic", Easing::InCubic),
        ("OutCubic", Easing::OutCubic),
        ("InOutCubic", Easing::InOutCubic),
        ("InQuart", Easing::InQuart),
        ("OutQuart", Easing::OutQuart),
        ("InOutQuart", Easing::InOutQuart),
        ("InQuint", Easing::InQuint),
        ("OutQuint", Easing::OutQuint),
        ("InOutQuint", Easing::InOutQuint),
        ("InSine", Easing::InSine),
        ("OutSine", Easing::OutSine),
        ("InOutSine", Easing::InOutSine),
        ("InExpo", Easing::InExpo),
        ("OutExpo", Easing::OutExpo),
        ("InOutExpo", Easing::InOutExpo),
        ("InCirc", Easing::InCirc),
        ("OutCirc", Easing::OutCirc),
        ("InOutCirc", Easing::InOutCirc),
        ("InBack", Easing::InBack),
        ("OutBack", Easing::OutBack),
        ("InOutBack", Easing::InOutBack),
        ("InElastic", Easing::InElastic),
        ("OutElastic", Easing::OutElastic),
        ("InOutElastic", Easing::InOutElastic),
        ("InBounce", Easing::InBounce),
        ("OutBounce", Easing::OutBounce),
        ("InOutBounce", Easing::InOutBounce),
    ];

    /// Back's overshoot constant and Elastic's period, Penner's originals, the numbers QML and
    /// every CSS easing cheat sheet use. Named so the arms below read as the shape and not the
    /// arithmetic. Each family's `InOut` uses a wider constant than its `In` and `Out` do, which
    /// is why those two arms are written out rather than reflected.
    const BACK: f32 = 1.70158;
    const BACK_IN_OUT: f32 = Self::BACK * 1.525;
    const ELASTIC: f32 = 2.0 * std::f32::consts::PI / 3.0;
    const ELASTIC_IN_OUT: f32 = 2.0 * std::f32::consts::PI / 4.5;

    pub(super) fn parse(name: &str) -> Option<Self> {
        Self::NAMES.iter().find(|(spelling, _)| *spelling == name).map(|(_, easing)| *easing)
    }

    /// One curve's `Out` from its `In`, and its `InOut` from both: reflection through the centre,
    /// which is how Penner defined the families and why only the `In` arm below is written out.
    fn out_of(inward: impl Fn(f32) -> f32, t: f32) -> f32 {
        1.0 - inward(1.0 - t)
    }

    fn in_out_of(inward: impl Fn(f32) -> f32 + Copy, t: f32) -> f32 {
        if t < 0.5 { 0.5 * inward(2.0 * t) } else { 0.5 + 0.5 * Self::out_of(inward, 2.0 * t - 1.0) }
    }

    /// Progress `t` in `[0, 1]` to the eased fraction of the distance covered. May leave `[0, 1]`
    /// for the overshooting families; [`Animatable::lerp`](super::Animatable::lerp) clamps to the property's own range.
    pub fn apply(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        let power = |p: i32| move |t: f32| t.powi(p);
        let sine = |t: f32| 1.0 - (t * std::f32::consts::FRAC_PI_2).cos();
        let expo = |t: f32| if t <= 0.0 { 0.0 } else { (10.0 * (t - 1.0)).exp2() };
        let circ = |t: f32| 1.0 - (1.0 - t * t).max(0.0).sqrt();
        let back = |t: f32| (Self::BACK + 1.0) * t.powi(3) - Self::BACK * t.powi(2);
        let elastic = |t: f32| {
            if t <= 0.0 || t >= 1.0 {
                return t;
            }
            -(10.0 * (t - 1.0)).exp2() * ((t * 10.0 - 10.75) * Self::ELASTIC).sin()
        };
        match self {
            Easing::Linear => t,
            Easing::InQuad => power(2)(t),
            Easing::OutQuad => Self::out_of(power(2), t),
            Easing::InOutQuad => Self::in_out_of(power(2), t),
            Easing::InCubic => power(3)(t),
            Easing::OutCubic => Self::out_of(power(3), t),
            Easing::InOutCubic => Self::in_out_of(power(3), t),
            Easing::InQuart => power(4)(t),
            Easing::OutQuart => Self::out_of(power(4), t),
            Easing::InOutQuart => Self::in_out_of(power(4), t),
            Easing::InQuint => power(5)(t),
            Easing::OutQuint => Self::out_of(power(5), t),
            Easing::InOutQuint => Self::in_out_of(power(5), t),
            Easing::InSine => sine(t),
            Easing::OutSine => Self::out_of(sine, t),
            Easing::InOutSine => Self::in_out_of(sine, t),
            Easing::InExpo => expo(t),
            Easing::OutExpo => Self::out_of(expo, t),
            Easing::InOutExpo => Self::in_out_of(expo, t),
            Easing::InCirc => circ(t),
            Easing::OutCirc => Self::out_of(circ, t),
            Easing::InOutCirc => Self::in_out_of(circ, t),
            Easing::InBack => back(t),
            Easing::OutBack => Self::out_of(back, t),
            Easing::InOutBack => {
                let c = Self::BACK_IN_OUT;
                if t < 0.5 {
                    (2.0 * t).powi(2) * ((c + 1.0) * 2.0 * t - c) / 2.0
                } else {
                    ((2.0 * t - 2.0).powi(2) * ((c + 1.0) * (2.0 * t - 2.0) + c) + 2.0) / 2.0
                }
            }
            Easing::InElastic => elastic(t),
            Easing::OutElastic => Self::out_of(elastic, t),
            Easing::InOutElastic => {
                if t <= 0.0 || t >= 1.0 {
                    return t;
                }
                let swing = ((t * 20.0 - 11.125) * Self::ELASTIC_IN_OUT).sin();
                if t < 0.5 {
                    -((20.0 * t - 10.0).exp2() * swing) / 2.0
                } else {
                    (-20.0f32).mul_add(t, 10.0).exp2() * swing / 2.0 + 1.0
                }
            }
            Easing::InBounce => bounce_in(t),
            Easing::OutBounce => Self::out_of(bounce_in, t),
            Easing::InOutBounce => Self::in_out_of(bounce_in, t),
            Easing::Bezier { x1, y1, x2, y2 } => bezier_at(x1, y1, x2, y2, t),
            // CSS `steps(n, jump-end)`: the value holds through each step and the last lands on
            // the target, so `t == 1` is the only progress that reaches it.
            Easing::Steps(n) => (t * n as f32).floor() / n as f32,
        }
    }
}

/// Penner's bounce, written `In` so the reflections above build the other two. Four parabolas of
/// shrinking height, the constants his originals use.
fn bounce_in(t: f32) -> f32 {
    const N: f32 = 7.5625;
    const D: f32 = 2.75;
    let t = 1.0 - t;
    let out = if t < 1.0 / D {
        N * t * t
    } else if t < 2.0 / D {
        let t = t - 1.5 / D;
        N * t * t + 0.75
    } else if t < 2.5 / D {
        let t = t - 2.25 / D;
        N * t * t + 0.9375
    } else {
        let t = t - 2.625 / D;
        N * t * t + 0.984375
    };
    1.0 - out
}

/// CSS `cubic-bezier`: the curve's `y` at the progress whose `x` is `t`. The `x` component is
/// strictly increasing over `[0, 1]` because both control `x` are held there, so a bisection
/// finds the parameter.
// ponytail: bisection, not Newton. Twenty halvings pin the parameter to under 1e-6 of `t`, which
// is finer than a frame of a 60 s animation, and it cannot diverge the way Newton does on a curve
// with a near-flat segment. Swap in Newton with a bisection fallback if a profile ever shows this.
fn bezier_at(x1: f32, y1: f32, x2: f32, y2: f32, t: f32) -> f32 {
    let curve = |a: f32, b: f32, p: f32| {
        let inv = 1.0 - p;
        3.0 * inv * inv * p * a + 3.0 * inv * p * p * b + p * p * p
    };
    // Both ends are on the curve exactly. Bisecting toward one lands a parameter about 1e-6 away
    // instead, which a control point far outside the unit square turns into a visible jump: the
    // `y` of a bezier is unbounded, so `{ 0, 1000000, 1, 1000000 }` starts a tween half again past
    // its target rather than on its source.
    if t <= 0.0 || t >= 1.0 {
        return t.clamp(0.0, 1.0);
    }
    let (mut low, mut high) = (0.0f32, 1.0f32);
    for _ in 0..20 {
        let mid = 0.5 * (low + high);
        if curve(x1, x2, mid) < t { low = mid } else { high = mid }
    }
    curve(y1, y2, 0.5 * (low + high))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_easing_starts_at_zero_and_ends_at_one() {
        for (name, easing) in Easing::NAMES {
            assert!((easing.apply(0.0)).abs() < 1e-6, "{name} at 0");
            assert!((easing.apply(1.0) - 1.0).abs() < 1e-5, "{name} at 1");
        }
    }

    #[test]
    fn in_out_quad_is_symmetric_about_the_midpoint() {
        assert!((Easing::InOutQuad.apply(0.5) - 0.5).abs() < 1e-6);
        assert!((Easing::InOutQuad.apply(0.25) + Easing::InOutQuad.apply(0.75) - 1.0).abs() < 1e-6);
    }
    /// Every named curve at a quarter, a midpoint and three quarters, against values computed from
    /// Qt's `QEasingCurve` -- which is what QML's `Easing.Type` actually runs -- and not against
    /// this module's own other arms. Checking a family's `Out` against a reflection of its own
    /// `In` is a tautology: it passed while `InOutBack` and `InOutElastic` were both wrong, because
    /// those two widen their constant for the `InOut` arm (`s * 1.525`, a period of `0.3 * 1.5`)
    /// and a plain reflection does not.
    #[test]
    fn every_named_curve_matches_qts_own_at_a_quarter_a_half_and_three_quarters() {
        #[rustfmt::skip]
        let reference: &[(Easing, [f32; 3])] = &[
            (Easing::Linear,       [0.250000, 0.500000, 0.750000]),
            (Easing::InQuad,       [0.062500, 0.250000, 0.562500]),
            (Easing::OutQuad,      [0.437500, 0.750000, 0.937500]),
            (Easing::InOutQuad,    [0.125000, 0.500000, 0.875000]),
            (Easing::InCubic,      [0.015625, 0.125000, 0.421875]),
            (Easing::OutCubic,     [0.578125, 0.875000, 0.984375]),
            (Easing::InOutCubic,   [0.062500, 0.500000, 0.937500]),
            (Easing::InQuart,      [0.003906, 0.062500, 0.316406]),
            (Easing::OutQuart,     [0.683594, 0.937500, 0.996094]),
            (Easing::InOutQuart,   [0.031250, 0.500000, 0.968750]),
            (Easing::InQuint,      [0.000977, 0.031250, 0.237305]),
            (Easing::OutQuint,     [0.762695, 0.968750, 0.999023]),
            (Easing::InOutQuint,   [0.015625, 0.500000, 0.984375]),
            (Easing::InSine,       [0.076120, 0.292893, 0.617317]),
            (Easing::OutSine,      [0.382683, std::f32::consts::FRAC_1_SQRT_2, 0.923880]),
            (Easing::InOutSine,    [0.146447, 0.500000, 0.853553]),
            (Easing::InExpo,       [0.005524, 0.031250, 0.176777]),
            (Easing::OutExpo,      [0.823223, 0.968750, 0.994476]),
            (Easing::InOutExpo,    [0.015625, 0.500000, 0.984375]),
            (Easing::InCirc,       [0.031754, 0.133975, 0.338562]),
            (Easing::OutCirc,      [0.661438, 0.866025, 0.968246]),
            (Easing::InOutCirc,    [0.066987, 0.500000, 0.933013]),
            (Easing::InBack,       [-0.064137, -0.087698, 0.182590]),
            (Easing::OutBack,      [0.817410, 1.087697, 1.064137]),
            (Easing::InOutBack,    [-0.099682, 0.500000, 1.099682]),
            (Easing::InElastic,    [-0.005524, -0.015625, 0.088388]),
            (Easing::OutElastic,   [0.911612, 1.015625, 1.005524]),
            (Easing::InOutElastic, [0.011969, 0.500000, 0.988031]),
            (Easing::InBounce,     [0.027344, 0.234375, 0.527344]),
            (Easing::OutBounce,    [0.472656, 0.765625, 0.972656]),
            (Easing::InOutBounce,  [0.117188, 0.500000, 0.882812]),
        ];
        assert_eq!(reference.len(), Easing::NAMES.len(), "every name has a row");
        for (easing, expected) in reference {
            for (t, want) in [0.25f32, 0.5, 0.75].into_iter().zip(expected) {
                let got = easing.apply(t);
                assert!((got - want).abs() < 1e-5, "{easing:?} at {t}: got {got}, Qt says {want}");
            }
        }
    }
}
