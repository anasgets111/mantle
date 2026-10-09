//! The GLSL contract a config shader is compiled against (ADR-0184) and the assembly of its source.

use femtovg::ImageId;

/// Prepended to every config shader, and the whole of the contract a shader writes against
/// (ADR-0184). Kept here rather than asked of the config so that a shader is a mask and not a
/// pile of boilerplate, and so the sampling convention cannot drift between two of them.
pub(super) const PRELUDE: &str = r#"#version 300 es
precision highp float;

in vec2 v_uv;
out vec4 fragColor;

uniform float u_progress;
uniform vec2 u_size;
uniform float mantle_opacity;
uniform vec4 mantle_radii;
uniform vec4 mantle_reach;
uniform vec4 mantle_power;
uniform vec4 mantle_round;
// An `outline`'s contour as a polygon, two points to each element; `layout::node::outline::distance` mirrors it.
uniform vec4 mantle_outline[128];
uniform int mantle_outline_len;

vec2 mantle_outline_point(int i) {
    vec4 pair = mantle_outline[i / 2];
    return i % 2 == 0 ? pair.xy : pair.zw;
}

// Signed distance in logical px to the outline, negative inside; smoothed corners are superellipses (`mantle_power`).
float mantle_sdf(vec2 p) {
    if (mantle_outline_len > 0) {
        float nearest = 1e20;
        bool inside = false;
        vec2 a = mantle_outline_point(mantle_outline_len - 1);
        for (int i = 0; i < mantle_outline_len; i++) {
            vec2 b = mantle_outline_point(i);
            vec2 e = b - a;
            vec2 w = p - a;
            float t = dot(e, e) > 0.0 ? clamp(dot(w, e) / dot(e, e), 0.0, 1.0) : 0.0;
            nearest = min(nearest, dot(w - e * t, w - e * t));
            if ((a.y <= p.y) != (b.y <= p.y) && w.x < e.x * w.y / e.y) {
                inside = !inside;
            }
            a = b;
        }
        return inside ? -sqrt(nearest) : sqrt(nearest);
    }
    vec2 half_size = mantle_round.zw * 0.5;
    p -= mantle_round.xy + half_size;
    float e = p.x < 0.0 ? (p.y < 0.0 ? mantle_reach.x : mantle_reach.w) : (p.y < 0.0 ? mantle_reach.y : mantle_reach.z);
    float n = p.x < 0.0 ? (p.y < 0.0 ? mantle_power.x : mantle_power.w) : (p.y < 0.0 ? mantle_power.y : mantle_power.z);
    vec2 q = abs(p) - half_size + e;
    vec2 c = max(q, 0.0);
    float d = length(c) + min(max(q.x, q.y), 0.0) - e;
    if (n != 2.0 && max(q.x, q.y) > 0.0) {
        float l = pow(pow(c.x, n) + pow(c.y, n), 1.0 / n);
        d = (l - e) * pow(max(l, 1e-4), n - 1.0) / length(pow(max(c, 1e-4), vec2(n - 1.0)));
    }
    return d;
}
"#;

/// The transition's half of the contract, between [`PRELUDE`] and [`RENAME`].
///
/// `mantle_from`/`mantle_to` return the endpoint's colour at a node-space coordinate, or
/// `u_fill` outside the picture -- the engine has already applied each endpoint's `fit`, so a
/// shader never repeats that arithmetic and never disagrees with how the same image draws
/// ordinarily.
pub(super) const SAMPLERS: &str = r#"
precision highp sampler2D;
uniform sampler2D u_from;
uniform sampler2D u_to;
uniform vec4 u_from_rect;
uniform vec4 u_to_rect;
uniform vec4 u_fill;

vec4 mantle_sample(sampler2D tex, vec4 rect, vec2 uv) {
    vec2 local = (uv - rect.xy) / rect.zw;
    if (local.x < 0.0 || local.x > 1.0 || local.y < 0.0 || local.y > 1.0) {
        return u_fill;
    }
    return texture(tex, local);
}

vec4 mantle_from(vec2 uv) { return mantle_sample(u_from, u_from_rect, uv); }
vec4 mantle_to(vec2 uv) { return mantle_sample(u_to, u_to_rect, uv); }
"#;

/// The `effect.shader` half of the contract, in place of [`SAMPLERS`] (ADR-0336). `u_input` is the
/// node's painted subtree, or the backdrop under it, in an offscreen that may reach past the box, so
/// `mantle_input` takes box coordinates as `mantle_from` does, `u_input_rect` placing the texture
/// in box fractions. `u_input_blurred` is the same pixels through `effect.backdrop`'s filters, or
/// `u_input` again without them. An offscreen image keeps its top row last, which the sample undoes.
pub(super) const INPUT: &str = r#"
precision highp sampler2D;
uniform sampler2D u_input;
uniform sampler2D u_input_blurred;
uniform vec4 u_input_rect;

vec4 mantle_read(sampler2D tex, vec2 uv) {
    vec2 local = (uv - u_input_rect.xy) / u_input_rect.zw;
    if (local.x < 0.0 || local.x > 1.0 || local.y < 0.0 || local.y > 1.0) {
        return vec4(0.0);
    }
    return texture(tex, vec2(local.x, 1.0 - local.y));
}

vec4 mantle_input(vec2 uv) { return mantle_read(u_input, uv); }
vec4 mantle_input_blurred(vec2 uv) { return mantle_read(u_input_blurred, uv); }
"#;

pub(super) const RENAME: &str = r#"
#define main mantle_effect
#line 1
"#;

/// Appended after the config's source, and the reason a config writes `void main()` and still
/// cannot get the node's `opacity` wrong (ADR-0184). The `#define` above renamed its entry point,
/// so this is the real one: it runs the effect, then applies the opacity the node inherited to the
/// premultiplied result -- all four channels, once, where the engine can guarantee it.
///
/// Documenting that rule and leaving a config to obey it would be the promise-without-mechanism
/// this branch has already made twice.
pub(super) const EPILOGUE: &str = r#"
#undef main
void main() {
    mantle_effect();
    // Rounded corners: antialiased by the outline's own slope.
    if (mantle_radii != vec4(0.0) || mantle_outline_len > 0) {
        float d = mantle_sdf(v_uv * u_size);
        fragColor *= 1.0 - smoothstep(-0.5 * fwidth(d), 0.5 * fwidth(d), d);
    }
    fragColor *= mantle_opacity;
}
"#;

/// The engine's own effect: a straight cross-dissolve, and what a `transition` with no `shader`
/// runs (ADR-0186). Written exactly as a config would write it, against the same contract and
/// through the same [`assemble`], so there is one sampling convention and not two.
///
/// This replaced two source-over draws, which composed correctly only for opaque endpoints at full
/// opacity: `from` at `alpha` with `to` at `alpha * progress` over it leaves `alpha=0.5`,
/// `progress=0.5` showing 0.625 opacity where 0.5 is right, and the surface's ground through the
/// middle. Textures upload premultiplied (ADR-0184), so mixing them *is* the composite, and the
/// epilogue applies the node's opacity once afterwards.
pub(super) const FADE: &str = r#"
void main() {
    fragColor = mix(mantle_from(v_uv), mantle_to(v_uv), u_progress);
}
"#;

/// One quad covering the node's box, in clip space, with the node-space `v_uv` the prelude reads.
/// The engine owns the vertex stage so that a config shader is a fragment and nothing else.
pub(super) const VERTEX: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
out vec2 v_uv;
void main() {
    v_uv = a_uv;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

/// Which contract a program is assembled against; one file may serve more than one.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Variant {
    /// A `shader` node: only the `images` it names (ADR-0253).
    Plain,
    /// A transition: [`SAMPLERS`] (ADR-0184).
    Cross,
    /// An `effect.shader`: [`INPUT`] (ADR-0336).
    Input,
}

/// The whole source a config's file is compiled as: the contract, then the file at line 1, then the
/// engine's own `main`. An `effect.shader` has no `main` of the engine's: it replaces the content,
/// so there is no opacity to apply and no outline to cut. Split out so a test can read it without
/// a GL context.
pub(super) fn assemble(source: &str, variant: Variant, images: &[(&str, Option<ImageId>)]) -> String {
    match variant {
        Variant::Plain => format!("{PRELUDE}{}{RENAME}{source}{EPILOGUE}", sampler_declarations(images)),
        Variant::Cross => {
            format!("{PRELUDE}{SAMPLERS}{}{RENAME}{source}{EPILOGUE}", sampler_declarations(images))
        }
        Variant::Input => format!("{PRELUDE}{INPUT}{}\n#line 1\n{source}", sampler_declarations(images)),
    }
}

/// A sampler and its pixel size per `images` entry.
fn sampler_declarations(images: &[(&str, Option<ImageId>)]) -> String {
    let declared: String =
        images.iter().map(|(name, _)| format!("uniform sampler2D {name};\nuniform vec2 {name}_size;\n")).collect();
    if declared.is_empty() { declared } else { format!("\nprecision highp sampler2D;\n{declared}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-0184. The source a config writes is wrapped, not trusted: its `main` is renamed so the
    /// engine's own can apply the node's opacity after it, and `#line 1` puts the config's first
    /// line at line 1 so a compiler error names a line the config can find.
    #[test]
    fn a_config_shader_is_wrapped_so_the_engine_owns_the_last_operation() {
        let assembled = assemble("void main() { fragColor = mantle_to(v_uv); }\n", Variant::Cross, &[]);

        let effect = assembled.find("#define main mantle_effect").expect("the rename");
        let user = assembled.find("void main() { fragColor").expect("the config's own source");
        let undef = assembled.find("#undef main").expect("the rename ends");
        let engine = assembled.rfind("fragColor *= mantle_opacity;").expect("the engine's last word");
        assert!(effect < user, "the rename has to reach the config's `main`");
        assert!(user < undef, "and has to stop before the engine writes its own");
        assert!(undef < engine);

        // The line directive is the last thing before the config's source, so its line numbers are
        // its own however long the prelude grows.
        let prelude = &assembled[..user];
        assert!(prelude.trim_end().ends_with("#line 1"), "got: {:?}", prelude.trim_end().rsplit('\n').next());
    }

    /// ADR-0253. A `shader` node binds no textures, so its prelude declares none: an unbound
    /// `sampler2D` would read unit 0, whatever femtovg left there.
    #[test]
    fn a_shader_node_is_assembled_without_samplers() {
        let assembled = assemble("void main() { fragColor = vec4(u_progress); }\n", Variant::Plain, &[]);
        let user = assembled.find("void main() { fragColor").expect("the config's own source");
        let prelude = &assembled[..user];
        for absent in ["sampler2D", "mantle_from", "mantle_to", "u_fill"] {
            assert!(!prelude.contains(absent), "`{absent}` in {prelude}");
        }
        for present in ["uniform float u_progress;", "uniform vec2 u_size;", "#define main mantle_effect"] {
            assert!(prelude.contains(present), "`{present}` missing from {prelude}");
        }
        assert!(prelude.trim_end().ends_with("#line 1"));
        assert!(assembled.ends_with(EPILOGUE));
    }

    /// A `shader` node's `images` become a sampler and a `_size` each, declared before the config's
    /// first line so its numbering holds.
    #[test]
    fn a_shader_nodes_images_are_declared_in_its_prelude() {
        let assembled = assemble("void main() {}\n", Variant::Plain, &[("lut", None), ("noise", None)]);
        let user = assembled.find("void main() {}").expect("the config's own source");
        let prelude = &assembled[..user];
        for present in
            ["uniform sampler2D lut;", "uniform vec2 lut_size;", "uniform sampler2D noise;", "uniform vec2 noise_size;"]
        {
            assert!(prelude.contains(present), "`{present}` missing from {prelude}");
        }
        assert!(prelude.trim_end().ends_with("#line 1"));
    }

    /// ADR-0336. An `effect.shader` has the input and the outline, no rename and no epilogue: it
    /// replaces the content, so there is no opacity to apply and no corner to cut. Every variant
    /// can measure the outline.
    #[test]
    fn an_effect_shader_is_assembled_with_the_input_and_no_epilogue() {
        let source = "void main() { fragColor = mantle_input(v_uv) * step(mantle_sdf(v_uv * u_size), 0.0); }\n";
        let assembled = assemble(source, Variant::Input, &[]);
        let user = assembled.find("void main() { fragColor").expect("the config's own source");
        let prelude = &assembled[..user];
        for present in ["uniform sampler2D u_input;", "uniform vec4 u_input_rect;", "vec4 mantle_input(vec2 uv)"] {
            assert!(prelude.contains(present), "`{present}` missing from {prelude}");
        }
        assert!(prelude.contains("float mantle_sdf(vec2 p)") && prelude.trim_end().ends_with("#line 1"));
        assert!(assembled.ends_with(source), "nothing of the engine's follows the config's source");
        for absent in ["mantle_effect", "mantle_from", "u_from"] {
            assert!(!assembled.contains(absent), "`{absent}` in an effect shader");
        }
        for variant in [Variant::Plain, Variant::Cross] {
            assert!(assemble(source, variant, &[]).contains("float mantle_sdf(vec2 p)"));
        }
    }
}
