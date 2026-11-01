//! A stack's descriptor: the literal globals its `globals` blocks hold.

use std::path::Path;
use std::sync::Arc;

use hcl_edit::Span;
use hcl_edit::expr::{Expression, ObjectKey};
use hcl_edit::structure::{BlockLabel, Structure};
use hcl_edit::template::Element;
use yaml_dsl_spec::resolved::{MapEntry, Value, ValueKind};
use yaml_dsl_spec::{Diagnostic, Position};

const GLOBALS_BLOCK: &str = "globals";

/// The globals of the descriptor at `file`, as one map; a labeled `globals` block
/// sets its attributes under the map its labels name.
pub fn read_globals(file: &Path) -> Result<Value, Diagnostic> {
    let text = std::fs::read_to_string(file)
        .map_err(|error| Diagnostic::unplaced(format!("reading {}: {error}", file.display())))?;
    let lines = LineIndex::new(file, &text);
    let body = hcl_edit::parser::parse_body(&text).map_err(|error| {
        let location = error.location();
        Diagnostic::at(
            &Position::new(&lines.file, location.line(), location.column()),
            error.message().to_owned(),
        )
    })?;
    let mut globals = Value::new(ValueKind::Map(Vec::new()), lines.position(0));
    for structure in body.iter() {
        let Structure::Block(block) = structure else {
            continue;
        };
        if block.ident.as_str() != GLOBALS_BLOCK {
            continue;
        }
        let labels: Vec<String> = block
            .labels
            .iter()
            .map(|label| match label {
                BlockLabel::Ident(ident) => ident.as_str().to_owned(),
                BlockLabel::String(text) => text.value().clone(),
            })
            .collect();
        for attribute_structure in block.body.iter() {
            let Structure::Attribute(attribute) = attribute_structure else {
                continue;
            };
            let value = literal(&attribute.value, &lines)?;
            let key_position = lines.position(attribute.key.span().map_or(0, |span| span.start));
            set_global(
                &mut globals,
                &labels,
                attribute.key.as_str(),
                key_position,
                value,
            );
        }
    }
    Ok(globals)
}

fn set_global(
    globals: &mut Value,
    labels: &[String],
    key: &str,
    key_position: Position,
    value: Value,
) {
    let mut holder = globals;
    for label in labels {
        let ValueKind::Map(entries) = &mut holder.kind else {
            return;
        };
        let index = match entries.iter().position(|entry| entry.key == *label) {
            Some(index) => index,
            None => {
                entries.push(MapEntry {
                    key: label.clone(),
                    key_position: key_position.clone(),
                    value: Value::new(ValueKind::Map(Vec::new()), key_position.clone()),
                });
                entries.len() - 1
            }
        };
        holder = &mut entries[index].value;
    }
    if let ValueKind::Map(entries) = &mut holder.kind {
        entries.retain(|entry| entry.key != key);
        entries.push(MapEntry {
            key: key.to_owned(),
            key_position,
            value,
        });
    }
}

fn literal(expression: &Expression, lines: &LineIndex) -> Result<Value, Diagnostic> {
    let position = lines.position(expression.span().map_or(0, |span| span.start));
    let kind = match expression {
        Expression::Null(_) => ValueKind::Null,
        Expression::Bool(flag) => ValueKind::Bool(*flag.value()),
        Expression::Number(number) => ValueKind::Number(number.value().to_string()),
        Expression::String(text) => ValueKind::String(text.value().clone()),
        Expression::Array(array) => ValueKind::List(
            array
                .iter()
                .map(|item| literal(item, lines))
                .collect::<Result<_, _>>()?,
        ),
        Expression::Object(object) => {
            let mut entries = Vec::new();
            for (key, value) in object.iter() {
                let key_text = match key {
                    ObjectKey::Ident(ident) => ident.as_str().to_owned(),
                    ObjectKey::Expression(Expression::String(text)) => text.value().clone(),
                    ObjectKey::Expression(_) => {
                        return Err(Diagnostic::at(
                            &position,
                            "a descriptor global's key is literal",
                        ));
                    }
                };
                entries.push(MapEntry {
                    key: key_text,
                    key_position: lines.position(key.span().map_or(0, |span| span.start)),
                    value: literal(value.expr(), lines)?,
                });
            }
            ValueKind::Map(entries)
        }
        Expression::StringTemplate(template) => {
            ValueKind::String(literal_text(template.iter(), &position)?)
        }
        Expression::HeredocTemplate(heredoc) => {
            ValueKind::String(literal_text(heredoc.template.iter(), &position)?)
        }
        _ => {
            return Err(Diagnostic::at(
                &position,
                "a descriptor global is a literal: tfgen reads no expressions",
            ));
        }
    };
    Ok(Value::new(kind, position))
}

fn literal_text<'e>(
    elements: impl Iterator<Item = &'e Element>,
    position: &Position,
) -> Result<String, Diagnostic> {
    let mut text = String::new();
    for element in elements {
        match element {
            Element::Literal(literal) => text.push_str(literal.value()),
            _ => {
                return Err(Diagnostic::at(
                    position,
                    "a descriptor global is a literal: tfgen reads no interpolation",
                ));
            }
        }
    }
    Ok(text)
}

/// Byte offsets of a file's line starts, for a position from an offset.
struct LineIndex {
    file: Arc<str>,
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(file: &Path, text: &str) -> Self {
        let starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(index, _)| index + 1))
            .collect();
        Self {
            file: Arc::from(file.to_string_lossy().as_ref()),
            starts,
        }
    }

    fn position(&self, offset: usize) -> Position {
        let line = self.starts.partition_point(|&start| start <= offset);
        let column = offset - self.starts[line - 1] + 1;
        Position::new(&self.file, line, column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globals_blocks_merge_into_one_map_of_literals() {
        let directory =
            std::env::temp_dir().join(format!("tfgen-descriptor-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("stack.tg.hcl");
        std::fs::write(
            &file,
            concat!(
                "stack {\n  name = \"mock\"\n}\n",
                "globals {\n  env = \"dev\"\n  tfgen = { templates = { aws = {} } }\n}\n",
                "globals \"mock\" {\n  count = 2\n}\n",
            ),
        )
        .unwrap();
        let globals = read_globals(&file);
        std::fs::remove_dir_all(&directory).unwrap();
        let globals = globals.unwrap();
        assert_eq!(globals.get("env").and_then(Value::as_str), Some("dev"));
        assert!(
            globals
                .get("tfgen")
                .and_then(|tfgen| tfgen.get("templates"))
                .is_some()
        );
        let count = globals
            .get("mock")
            .and_then(|mock| mock.get("count"))
            .unwrap();
        assert_eq!(count.kind, ValueKind::Number("2".into()));
        assert_eq!((count.position.line, count.position.column), (9, 11));
    }
}
