use super::{within, xy};
use crate::layout::node::{LayoutError, PropMap, invalid, preview_for_error, value_as_f32};
use crate::text::snap::LogicalRect;

/// `scale`, `rotate`, `translate` and `origin` (ADR-0149): a paint-only affine on the
/// node and its subtree, applied after layout about `origin` (fractions of the node's own box).
/// CSS's `transform` rather than separate properties: one matrix,
/// one origin, nothing for the solver to see. Every field tweens (numbers and `{ x, y }` tables).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub scale: (f32, f32),
    /// Degrees, clockwise on screen.
    pub rotate: f32,
    /// Logical pixels.
    pub translate: (f32, f32),
    /// Fractions of the node's box, `(0.5, 0.5)` its centre.
    pub origin: (f32, f32),
}

impl Default for Transform {
    fn default() -> Self {
        Self { scale: (1.0, 1.0), rotate: 0.0, translate: (0.0, 0.0), origin: (0.5, 0.5) }
    }
}

/// An affine `[a, b, c, d, e, f]` in femtovg's layout: `x' = a x + c y + e`, `y' = b x + d y + f`.
pub type Affine = [f32; 6];

impl Transform {
    pub fn is_identity(&self) -> bool {
        *self == Self { origin: self.origin, ..Self::default() }
    }

    /// The matrix mapping the node's untransformed absolute coordinates to painted ones:
    /// translate to the origin, scale, rotate, translate back, then `translate`.
    pub fn matrix(&self, rect: LogicalRect) -> Affine {
        let (cx, cy) = (rect.x + rect.width * self.origin.0, rect.y + rect.height * self.origin.1);
        let (sin, cos) = self.rotate.to_radians().sin_cos();
        let (a, b, c, d) = (self.scale.0 * cos, self.scale.0 * sin, -self.scale.1 * sin, self.scale.1 * cos);
        let e = cx + self.translate.0 - (a * cx + c * cy);
        let f = cy + self.translate.1 - (b * cx + d * cy);
        [a, b, c, d, e, f]
    }
}

pub fn apply_affine([a, b, c, d, e, f]: Affine, x: f32, y: f32) -> (f32, f32) {
    (a * x + c * y + e, b * x + d * y + f)
}

/// `None` for a degenerate matrix (a zero scale), which maps everything to a line and nothing back.
pub fn invert_affine([a, b, c, d, e, f]: Affine) -> Option<Affine> {
    let det = a * d - b * c;
    if det.abs() < 1e-9 {
        return None;
    }
    let (ia, ib, ic, id) = (d / det, -b / det, -c / det, a / det);
    Some([ia, ib, ic, id, -(ia * e + ic * f), -(ib * e + id * f)])
}

pub fn parse_transform(properties: &PropMap) -> Result<Transform, LayoutError> {
    let mut transform = Transform::default();
    if let Some(value) = properties.get("scale") {
        transform.scale = match value_as_f32("scale", value)? {
            Some(n) => (within("scale", n)?, n),
            None => xy("scale", value)?,
        };
    }
    if let Some(value) = properties.get("rotate") {
        let n = value_as_f32("rotate", value)?
            .ok_or_else(|| invalid("rotate", format!("expected degrees, got {}", preview_for_error(value))))?;
        transform.rotate = within("rotate", n)?;
    }
    if let Some(value) = properties.get("translate") {
        transform.translate = xy("translate", value)?;
    }
    if let Some(value) = properties.get("origin") {
        transform.origin = xy("origin", value)?;
    }
    Ok(transform)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::rect_props;
    use mlua::Lua;

    #[test]
    fn a_transform_parses_its_four_properties_and_maps_a_corner_about_its_origin() {
        let lua = Lua::new();
        let props =
            rect_props(&lua, r#"return { scale = 2, rotate = 90, translate = { x = 10 }, origin = { x = 0, y = 0 } }"#);
        let t = parse_transform(&props).unwrap();
        assert_eq!((t.scale, t.rotate, t.translate, t.origin), ((2.0, 2.0), 90.0, (10.0, 0.0), (0.0, 0.0)));
        // About the top-left corner: (x + 4, y) scales to (x + 8, y), rotates a quarter turn
        // clockwise to (x, y + 8), then shifts right by 10.
        let m = t.matrix(LogicalRect { x: 100.0, y: 50.0, width: 20.0, height: 20.0 });
        let (px, py) = apply_affine(m, 104.0, 50.0);
        assert!((px - 110.0).abs() < 1e-3 && (py - 58.0).abs() < 1e-3, "got ({px}, {py})");
        let (bx, by) = apply_affine(invert_affine(m).unwrap(), px, py);
        assert!((bx - 104.0).abs() < 1e-3 && (by - 50.0).abs() < 1e-3);
        assert!(Transform::default().is_identity());
        assert!(!t.is_identity());
    }

    #[test]
    fn a_transform_refuses_a_negative_scale_and_an_origin_outside_the_box() {
        let lua = Lua::new();
        let parse = |src: &str| parse_transform(&rect_props(&lua, src));
        assert!(parse("return { scale = -1 }").unwrap_err().to_string().contains("[0, 64]"));
        assert!(parse("return { origin = { x = 2 } }").unwrap_err().to_string().contains("[0, 1]"));
        assert!(parse("return { scale = { y = 3 } }").unwrap().scale == (1.0, 3.0), "an absent axis keeps 1");
        assert!(
            invert_affine(Transform { scale: (0.0, 1.0), ..Transform::default() }.matrix(LogicalRect {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0
            }))
            .is_none()
        );
    }
}
