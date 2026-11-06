//! Values `tfgen.yml` computes: globals, builtin values and a generator's values,
//! written in YAML with refs and `fn.` calls.

use std::sync::OnceLock;

use yaml_dsl_spec::definition::MarkerFunction;
use yaml_dsl_spec::resolved::{MapEntry, Value, ValueKind};
use yaml_dsl_spec::{CallKey, Diagnostic, Node, NodeKind, Position, parse_call_key};

use crate::config_files::ValueRule;
use crate::format_function::format_layout;
use crate::ref_paths::{find_refs, whole_ref};
use crate::ref_scope::RefFrame;

const FORMAT_FUNCTION: &str = "format";

fn call_syntax() -> &'static MarkerFunction {
    static SYNTAX: OnceLock<MarkerFunction> = OnceLock::new();
    SYNTAX.get_or_init(|| MarkerFunction {
        call_marker: "fn.".to_owned(),
        splat_operator: Some("*".to_owned()),
        calls_without_name_allowed: true,
    })
}

/// The call a key such as `name fn.format*` makes.
pub fn call_key(key: &str) -> Option<CallKey> {
    parse_call_key(key, call_syntax())
}

/// The value `node` computes, or `None` where it is a ref to an absent key.
pub fn evaluate_node(node: &Node, frame: &RefFrame<'_>) -> Result<Option<Value>, Diagnostic> {
    let position = &node.position;
    let kind = match &node.kind {
        NodeKind::Null => ValueKind::Null,
        NodeKind::Bool(flag) => ValueKind::Bool(*flag),
        NodeKind::Number(text) => ValueKind::Number(text.clone()),
        NodeKind::String(text) => return evaluate_text(text, position, frame),
        NodeKind::List(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.extend(evaluate_node(item, frame)?);
            }
            ValueKind::List(values)
        }
        NodeKind::Map(entries) => {
            let mut values = Vec::with_capacity(entries.len());
            for entry in entries {
                let (key, value) = match call_key(&entry.key) {
                    Some(call) => {
                        let result = call_value(&call, &entry.value, frame)?;
                        match call.name {
                            Some(name) => (name, result),
                            None if entries.len() == 1 => return Ok(Some(result)),
                            None => {
                                return Err(Diagnostic::at(
                                    &entry.key_position,
                                    "a call with no name is the whole map it stands in",
                                ));
                            }
                        }
                    }
                    None => match evaluate_node(&entry.value, frame)? {
                        Some(value) => (entry.key.clone(), value),
                        None => continue,
                    },
                };
                values.push(MapEntry {
                    key,
                    key_position: entry.key_position.clone(),
                    value,
                });
            }
            ValueKind::Map(values)
        }
    };
    Ok(Some(Value::new(kind, position.clone())))
}

/// Text with refs: a lone ref is the value it names, and refs inside longer text
/// are written as their scalars' text.
pub fn evaluate_text(
    text: &str,
    position: &Position,
    frame: &RefFrame<'_>,
) -> Result<Option<Value>, Diagnostic> {
    let at = |message: String| Diagnostic::at(position, message);
    if let Some(path) = whole_ref(text).map_err(at)? {
        return frame.read(&path, position);
    }
    let spans = find_refs(text).map_err(at)?;
    let mut written = String::new();
    let mut cursor = 0;
    for span in spans {
        written.push_str(&text[cursor..span.start]);
        written.push_str(&scalar_of(frame, &span.path, position)?);
        cursor = span.end;
    }
    written.push_str(&text[cursor..]);
    Ok(Some(Value::string(written, position.clone())))
}

/// The text of the scalar a ref names, for writing inside longer text.
pub fn scalar_of(
    frame: &RefFrame<'_>,
    path: &crate::ref_paths::RefPath,
    position: &Position,
) -> Result<String, Diagnostic> {
    let value = frame
        .read(path, position)?
        .ok_or_else(|| Diagnostic::at(position, format!("`{path}` is absent")))?;
    value.scalar_text().ok_or_else(|| {
        Diagnostic::at(
            position,
            format!("`{path}` stands inside text, and holds a value that is not a scalar"),
        )
    })
}

fn call_value(call: &CallKey, node: &Node, frame: &RefFrame<'_>) -> Result<Value, Diagnostic> {
    if call.function != FORMAT_FUNCTION {
        return Err(Diagnostic::at(
            &node.position,
            format!(
                "`fn.{}` is not a function: tfgen has `fn.format`",
                call.function
            ),
        ));
    }
    let arguments = evaluate_node(node, frame)?
        .ok_or_else(|| Diagnostic::at(&node.position, "fn.format's arguments are absent"))?;
    let operands: Vec<Value> = match (&arguments.kind, call.splat) {
        (ValueKind::List(items), true) => items.clone(),
        (_, true) => {
            return Err(Diagnostic::at(
                &node.position,
                "`fn.format*` takes a list: its layout and its operands",
            ));
        }
        (_, false) => vec![arguments],
    };
    let texts = operands
        .iter()
        .map(|operand| {
            operand
                .scalar_text()
                .ok_or_else(|| Diagnostic::at(&operand.position, "a fn.format operand is a scalar"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let (layout, rest) = texts
        .split_first()
        .ok_or_else(|| Diagnostic::at(&node.position, "fn.format takes a layout"))?;
    let formatted =
        format_layout(layout, rest).map_err(|message| Diagnostic::at(&node.position, message))?;
    Ok(Value::string(formatted, node.position.clone()))
}

/// A generator's value: a value or its default, a list built from the entries a
/// JSONPath selects in the folded document, or the value written for the first
/// declaration of a scope, else its default.
pub fn evaluate_value_rule(rule: &ValueRule, frame: &RefFrame<'_>) -> Result<Value, Diagnostic> {
    match rule {
        ValueRule::Single { value, default } => {
            if let Some(computed) = evaluate_node(value, frame)? {
                return Ok(computed);
            }
            let default = default.as_ref().ok_or_else(|| {
                Diagnostic::at(&value.position, "the value is absent and has no `default`")
            })?;
            evaluate_node(default, frame)?
                .ok_or_else(|| Diagnostic::at(&default.position, "the default is absent"))
        }
        ValueRule::Each {
            each,
            where_fields,
            defaults,
            value,
        } => {
            let fold = &frame
                .generator
                .ok_or_else(|| Diagnostic::at(&value.position, "`each` reads the folded document"))?
                .fold;
            let defaults = match defaults {
                Some(defaults) => evaluate_node(defaults, frame)?,
                None => None,
            };
            let mut computed = Vec::new();
            for (_, selected) in each.select(fold) {
                let selected = selected.local_value();
                if !fields_equal(selected, where_fields, frame)? {
                    continue;
                }
                let entry = with_defaults(selected, defaults.as_ref());
                let entry_frame = frame.with_each(&entry);
                computed.extend(evaluate_node(value, &entry_frame)?);
            }
            Ok(Value::new(
                ValueKind::List(computed),
                value.position.clone(),
            ))
        }
        ValueRule::First {
            scope,
            scope_keys,
            value,
            default,
        } => {
            let declarations = &frame
                .generator
                .ok_or_else(|| Diagnostic::at(&value.position, "`first` reads the declarations"))?
                .declarations;
            let first = declarations.iter().find(|declaration| {
                declaration.scope == *scope
                    && scope_keys.as_ref().is_none_or(|keys| {
                        declaration
                            .scope_key
                            .as_ref()
                            .is_some_and(|scope_key| keys.contains(scope_key))
                    })
            });
            if let Some(declaration) = first {
                return evaluate_node(value, &frame.with_each(&declaration.each))?.ok_or_else(
                    || {
                        Diagnostic::at(
                            &value.position,
                            "the value is absent for the first declaration",
                        )
                    },
                );
            }
            let default = default.as_ref().ok_or_else(|| {
                Diagnostic::at(
                    &value.position,
                    "`first` selects no declaration and has no `default`",
                )
            })?;
            evaluate_node(default, frame)?
                .ok_or_else(|| Diagnostic::at(&default.position, "the default is absent"))
        }
    }
}

fn fields_equal(
    selected: &Value,
    where_fields: &[(String, Node)],
    frame: &RefFrame<'_>,
) -> Result<bool, Diagnostic> {
    for (field, expected) in where_fields {
        let expected = evaluate_node(expected, frame)?.and_then(|value| value.scalar_text());
        let actual = selected
            .get(field)
            .and_then(|value| value.local_value().scalar_text());
        if expected.is_none() || expected != actual {
            return Ok(false);
        }
    }
    Ok(true)
}

/// `selected`, with each field of `defaults` it does not hold.
fn with_defaults(selected: &Value, defaults: Option<&Value>) -> Value {
    let (Some(entries), Some(default_entries)) =
        (selected.entries(), defaults.and_then(Value::entries))
    else {
        return selected.clone();
    };
    let mut merged = entries.to_vec();
    for default in default_entries {
        if !entries.iter().any(|entry| entry.key == default.key) {
            merged.push(default.clone());
        }
    }
    Value::new(ValueKind::Map(merged), selected.position.clone())
}
