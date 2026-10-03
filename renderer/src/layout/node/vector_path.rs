//! Bounded path commands. Parsed once per apply, compared by value in display lists.
use super::prop::{Keyword, Prop, keywords};
use super::{LayoutError, Property, input, invalid, only_keys};
use crate::lua::luacats::{lua_shape, spelled};
use std::rc::Rc;

use mlua::{Table, Value};

keywords! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum PathOp { M, L, Q, C, A, Z }
}

lua_shape! {
    /// Node-local logical pixels. Q has one control point, C has two, then the endpoint. A is
    /// centre x, centre y, radius, start and sweep in degrees clockwise from the +x axis. `hole`
    /// on a subpath's first command cuts it out of the fill, whatever its winding.
    #[alias = "PathCommand"]
    #[derive(Debug, Clone, PartialEq)]
    pub struct PathCommand {
        pub op: PathOp,
        pub points: Vec<f32>,
        pub hole: Option<bool>,
    }
}

/// One command's op and flags; its numbers live in [`PathData::points`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub op: PathOp,
    pub hole: bool,
    /// Opens a subpath: an `M`, or an `A` first or after a `Z`.
    pub begins: bool,
}

impl PathOp {
    /// How many numbers the op takes, and how many of those lead as pixels: an arc's start and
    /// sweep are angles.
    fn arity(self) -> (usize, usize) {
        match self {
            Self::M | Self::L => (2, 2),
            Self::Q => (4, 4),
            Self::A => (5, 3),
            Self::C => (6, 6),
            Self::Z => (0, 0),
        }
    }
}

/// Parsed `commands`. The segments are shared by every frame of a tween between two layouts, so
/// a frame allocates only its numbers.
#[derive(Debug, Clone, PartialEq)]
pub struct PathData {
    pub segments: Rc<[Segment]>,
    pub points: Vec<f32>,
}

/// Every number is finite: the parser rejects the rest and a tween clamps. `Eq` lets `Rc<PathData>`
/// compare by pointer first, so an unchanged path's display list entry skips its numbers.
impl Eq for PathData {}

/// Each segment, the range of its numbers, and how many of those lead as pixels.
fn spans(segments: &[Segment]) -> impl Iterator<Item = (Segment, std::ops::Range<usize>, usize)> {
    let mut at = 0;
    segments.iter().map(move |segment| {
        let (count, pixels) = segment.op.arity();
        at += count;
        (*segment, at - count..at, pixels)
    })
}

impl PathData {
    /// Each segment with its numbers.
    pub fn iter(&self) -> impl Iterator<Item = (Segment, &[f32])> {
        spans(&self.segments).map(|(segment, range, _)| (segment, &self.points[range]))
    }

    /// Every pixel value, which display scaling multiplies and a tween clamps.
    pub fn for_each_pixel(&mut self, mut f: impl FnMut(&mut f32)) {
        for (_, range, pixels) in spans(&self.segments) {
            self.points[range.start..range.start + pixels].iter_mut().for_each(&mut f);
        }
    }

    /// `self` moved toward `to` by `t`, or `None` when their segments differ and the tween snaps.
    pub fn lerp(&self, to: &Self, t: f32) -> Option<Self> {
        if !Rc::ptr_eq(&self.segments, &to.segments) && self.segments != to.segments {
            return None;
        }
        // In f64: arc angles may be any finite f32, and their difference can overflow one.
        let t = f64::from(t);
        let mut points: Vec<f32> = self
            .points
            .iter()
            .zip(&to.points)
            .map(|(&p, &q)| {
                let (p, q) = (f64::from(p), f64::from(q));
                (p + (q - p) * t).clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
            })
            .collect();
        for (segment, range, pixels) in spans(&self.segments) {
            points[range.start..range.start + pixels].iter_mut().for_each(|p| *p = p.clamp(-COORD_MAX, COORD_MAX));
            if segment.op == PathOp::A {
                // An overshooting easing must not turn a radius negative.
                points[range.start + 2] = points[range.start + 2].max(0.0);
            }
        }
        Some(Self { segments: self.segments.clone(), points })
    }
}

/// A tween's frame in the property map: the parsed path itself, so the parser takes it back
/// without building and walking a Lua table per command.
struct Tweened(Rc<PathData>);

pub(crate) fn tweened(lua: &mlua::Lua, path: &Rc<PathData>) -> mlua::Result<Value> {
    lua.create_any_userdata(Tweened(path.clone())).map(Value::UserData)
}

/// The bound on every pixel value, in node-local logical pixels.
pub const COORD_MAX: f32 = 8192.0;

/// A `path` node's paint, shared by its [`PaintStyle`](super::PaintStyle) and its display-list draw.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorPath {
    pub commands: Rc<PathData>,
    pub fill: Option<super::Fill>,
    pub stroke: Option<super::Fill>,
    pub stroke_width: f32,
}

pub(crate) struct PathCommands;
spelled!(PathCommands => "PathCommand[]");

// ponytail: 4096 commands and six coordinates each; larger drawings need a measured tessellation budget.
const MAX_COMMANDS: usize = 4096;

impl Prop for PathCommands {
    type Out = Rc<PathData>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError> {
        let table = match value {
            None => return Ok(Rc::new(PathData { segments: Rc::from([]), points: Vec::new() })),
            Some(Value::Table(table)) => table,
            Some(Value::UserData(ud)) if let Ok(frame) = ud.borrow::<Tweened>() => return Ok(frame.0.clone()),
            Some(_) => return Err(invalid(row.name, "expected a command array")),
        };
        let len = input::array_len(row.name, table, MAX_COMMANDS)?;
        let mut segments = Vec::with_capacity(len);
        let mut points = Vec::with_capacity(len * 2);
        let mut open = false;
        for i in 1..=len {
            let name = format!("{}[{i}]", row.name);
            let input: Table = table.raw_get(i).map_err(|e| invalid(&name, e.to_string()))?;
            only_keys(&name, &input, PathCommand::KEYS)?;
            let numbers: Table = input.get("points").map_err(|e| invalid(&name, e.to_string()))?;
            input::array_len(&name, &numbers, 6)?;
            let command = PathCommand {
                op: input::field(&name, &input, "op")?,
                points: input::read(&name, "points", Value::Table(numbers))?,
                hole: input::field(&name, &input, "hole")?,
            };
            let (count, pixels) = command.op.arity();
            if command.points.len() != count
                || command.points.iter().any(|n| !n.is_finite())
                || command.points[..pixels].iter().any(|n| n.abs() > COORD_MAX)
            {
                return Err(invalid(
                    &name,
                    format!("{} needs {count} finite numbers, coordinates in [-8192, 8192]", command.op.name()),
                ));
            }
            if command.op == PathOp::A && command.points[2] < 0.0 {
                return Err(invalid(&name, "A needs a radius of at least 0"));
            }
            let begins = !open || command.op == PathOp::M;
            if command.hole.is_some() && !begins {
                return Err(invalid(&name, "hole belongs on the command that begins a subpath"));
            }
            if begins && !matches!(command.op, PathOp::M | PathOp::A) {
                return Err(invalid(&name, "begin each subpath with M or A"));
            }
            open = command.op != PathOp::Z;
            segments.push(Segment { op: command.op, hole: command.hole == Some(true), begins });
            points.extend(command.points);
        }
        Ok(Rc::new(PathData { segments: segments.into(), points }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::node::fields;
    #[test]
    fn path_commands_validate_limits_structure_and_subpaths() {
        let lua = mlua::Lua::new();
        let parse = |src: &str| {
            let value: Value = lua.load(src).eval().unwrap();
            PathCommands::read(&fields::path::commands.row, Some(&value))
        };
        assert!(parse("{}").unwrap().segments.is_empty());
        let command = parse(
            r#"(function()
            local reads = 0
            return {setmetatable({op = 'M'}, {__index = function(_, key)
                if key == 'points' then
                    reads = reads + 1
                    assert(reads == 1, 'points getter ran twice')
                    return {1, 2}
                end
            end})}
        end)()"#,
        )
        .unwrap();
        assert_eq!(command.points, [1.0, 2.0]);

        assert!(parse(r#"{{op='M',points={1,2}}, {op='Q',points={2,3,4,5}}, {op='C',points={1,2,3,4,5,6}}, {op='Z',points={}}}"#).is_ok());
        let ring = parse(
            "{{op='A',points={8,8,8,0,360}}, {op='Z',points={}}, {op='A',points={8,8,4,0,-360},hole=true}, \
             {op='M',points={0,0},hole=false}, {op='A',points={8,8,8,9000,-9000}}}",
        )
        .unwrap();
        let flags = |f: fn(&Segment) -> bool| ring.segments.iter().map(f).collect::<Vec<_>>();
        assert_eq!(flags(|s| s.hole), [false, false, true, false, false]);
        assert_eq!(flags(|s| s.begins), [true, false, true, true, false]);
        // A tween's frame comes back as the same parsed path, no table walked.
        let frame = tweened(&lua, &ring).unwrap();
        assert!(Rc::ptr_eq(&PathCommands::read(&fields::path::commands.row, Some(&frame)).unwrap(), &ring));
        for bad in [
            "1",
            "{extra=1}",
            "{[2]={op='M',points={1,2}}}",
            "{{op='L',points={1,2}}}",
            "{{op='B',points={1,2}}}",
            "{{op='A',points={1,2,3,4}}}",
            "{{op='A',points={1,2,-1,0,90}}}",
            "{{op='A',points={1,2,8193,0,90}}}",
            "{{op='A',points={1,2,3,0,math.huge}}}",
            "{{op='M',points={1,2}},{op='L',points={2,3},hole=true}}",
            "{{op='M',points={1,2}},{op='A',points={1,2,3,0,90},hole=true}}",
            "{{op='M',points={1,2},hole=1}}",
            "{{op='M',points={1}}}",
            "{{op='M',points={1,2,3}}}",
            "{{op='M',points={1,0/0}}}",
            "{{op='M',points={1,math.huge}}}",
            "{{op='M',points={1,8193}}}",
            "{{op='M',points={1,2},oops=true}}",
            "{{op='M',points={1,2,extra=3}}}",
            "{{op='M',points={1,2}},{op='Z',points={}},{op='L',points={2,3}}}",
            "(function() local a={} for i=1,4097 do a[i]={op='M',points={1,2}} end return a end)()",
        ] {
            assert!(parse(bad).is_err(), "accepted {bad}");
        }
        assert!(
            parse("(function() local a={} for i=1,4096 do a[i]={op='M',points={-8192,8192}} end return a end)()")
                .is_ok()
        );
    }
}
