//! Bounded path commands. Parsed once per apply, compared by value in display lists.
use super::prop::{Keyword, Prop, keywords};
use super::{LayoutError, Property, input, invalid, only_keys};
use crate::lua::luacats::{lua_shape, spelled};
use mlua::{Table, Value};

keywords! {
    #[derive(Debug, Clone, Copy, PartialEq)]
    pub enum PathOp { M, L, Q, C, Z }
}

lua_shape! {
    /// Node-local logical pixels. Q has one control point, C has two, then the endpoint.
    #[alias = "PathCommand"]
    #[derive(Debug, Clone, PartialEq)]
    pub struct PathCommand {
        pub op: PathOp,
        pub points: Vec<f32>,
    }
}

pub(crate) struct PathCommands;
spelled!(PathCommands => "PathCommand[]");

// ponytail: 4096 commands and six coordinates each; larger drawings need a measured tessellation budget.
const MAX_COMMANDS: usize = 4096;

impl Prop for PathCommands {
    type Out = Vec<PathCommand>;
    fn read(row: &Property, value: Option<&Value>) -> Result<Self::Out, LayoutError> {
        let Some(value) = value else { return Ok(Vec::new()) };
        let Value::Table(table) = value else { return Err(invalid(row.name, "expected a command array")) };
        let len = array_len(row.name, table, MAX_COMMANDS)?;
        let mut commands = Vec::with_capacity(len);
        let mut open = false;
        for i in 1..=len {
            let name = format!("{}[{i}]", row.name);
            let input: Table = table.raw_get(i).map_err(|e| invalid(&name, e.to_string()))?;
            only_keys(&name, &input, PathCommand::KEYS)?;
            let op = input::field(&name, &input, "op")?;
            let points: Table = input.get("points").map_err(|e| invalid(&name, e.to_string()))?;
            array_len(&name, &points, 6)?;
            let command = PathCommand { op, points: input::read(&name, "points", Value::Table(points))? };
            let count = match command.op {
                PathOp::M | PathOp::L => 2,
                PathOp::Q => 4,
                PathOp::C => 6,
                PathOp::Z => 0,
            };
            if command.points.len() != count || command.points.iter().any(|n| !n.is_finite() || n.abs() > 8192.0) {
                return Err(invalid(
                    &name,
                    format!("{} needs {count} finite coordinates in [-8192, 8192]", command.op.name()),
                ));
            }
            if command.op == PathOp::M {
                open = true;
            } else if !open {
                return Err(invalid(&name, "begin each subpath with M"));
            }
            if command.op == PathOp::Z {
                open = false;
            }
            commands.push(command);
        }
        Ok(commands)
    }
}

fn array_len(name: &str, table: &Table, limit: usize) -> Result<usize, LayoutError> {
    let len = table.raw_len();
    if len > limit {
        return Err(invalid(name, format!("at most {limit} entries")));
    }
    let mut count = 0;
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair.map_err(|e| invalid(name, e.to_string()))?;
        if !matches!(key, Value::Integer(i) if i > 0 && i as usize <= len) {
            return Err(invalid(name, "expected a dense array with no named keys"));
        }
        count += 1;
        if count > limit {
            return Err(invalid(name, format!("at most {limit} entries")));
        }
    }
    if count != len {
        return Err(invalid(name, "expected a dense array"));
    }
    Ok(len)
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
        assert!(parse("{}").unwrap().is_empty());
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
        assert_eq!(command[0].points, [1.0, 2.0]);

        assert!(parse(r#"{{op='M',points={1,2}}, {op='Q',points={2,3,4,5}}, {op='C',points={1,2,3,4,5,6}}, {op='Z',points={}}}"#).is_ok());
        for bad in [
            "1",
            "{extra=1}",
            "{[2]={op='M',points={1,2}}}",
            "{{op='L',points={1,2}}}",
            "{{op='A',points={1,2}}}",
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
