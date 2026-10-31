//! A resolved value as HCL: scalars, lists and maps as themselves, a reference by
//! the `references` template that matches it, and a call as a Terraform call.

use yaml_dsl_compiler::Diagnostic;
use yaml_dsl_compiler::output::{Expression, FunctionCall, Interpolation, ObjectKey, TemplatePart};
use yaml_dsl_compiler::resolved::{self, MapEntry, Reference, Target, Value, ValueKind};

use crate::config_files::ReferenceRule;
use crate::ref_scope::RefFrame;
use crate::template_filling::fill_expression;

pub struct ValueWriter<'w> {
    references: &'w [ReferenceRule],
    frame: RefFrame<'w>,
}

impl<'w> ValueWriter<'w> {
    pub fn new(references: &'w [ReferenceRule], frame: RefFrame<'w>) -> Self {
        Self { references, frame }
    }

    pub fn expression(&self, value: &Value) -> Result<Expression, Diagnostic> {
        Ok(match &value.kind {
            ValueKind::Null => Expression::Null,
            ValueKind::Bool(flag) => Expression::Bool(*flag),
            ValueKind::Number(text) => Expression::Number(text.clone()),
            ValueKind::String(text) => Expression::String(text.clone()),
            ValueKind::List(items) => Expression::List(
                items
                    .iter()
                    .map(|item| self.expression(item))
                    .collect::<Result<_, _>>()?,
            ),
            ValueKind::Map(entries) => Expression::Map(
                entries
                    .iter()
                    .map(|entry| {
                        Ok((
                            ObjectKey::Name(entry.key.clone()),
                            self.expression(&entry.value)?,
                        ))
                    })
                    .collect::<Result<_, Diagnostic>>()?,
            ),
            ValueKind::Reference(reference) => self.reference(reference, value)?,
            ValueKind::Call(call) => Expression::Call(FunctionCall {
                function: call.function.clone(),
                arguments: call
                    .arguments
                    .iter()
                    .map(|argument| self.expression(argument))
                    .collect::<Result<_, _>>()?,
                expand_final_argument: call.expand_final_argument,
            }),
            ValueKind::Template(parts) => Expression::Template(
                parts
                    .iter()
                    .map(|part| {
                        Ok(match part {
                            resolved::TemplatePart::Text(text) => TemplatePart::Text(text.clone()),
                            resolved::TemplatePart::Reference(reference) => {
                                TemplatePart::Interpolation(Interpolation::new(
                                    self.reference(reference, value)?,
                                ))
                            }
                        })
                    })
                    .collect::<Result<_, Diagnostic>>()?,
            ),
        })
    }

    /// A reference, written by the first `references` template that matches it.
    fn reference(&self, reference: &Reference, value: &Value) -> Result<Expression, Diagnostic> {
        if reference.target == Target::Builtin {
            return Err(Diagnostic::at(
                &value.position,
                format!(
                    "`{}` reads the builtin `{}`, which builtin_values gives no value",
                    reference.text, reference.name
                ),
            ));
        }
        let rule = self
            .references
            .iter()
            .find(|rule| matches(rule, reference))
            .ok_or_else(|| {
                Diagnostic::at(
                    &value.position,
                    format!(
                        "no `references` template writes `{}`, a reference of scope `{}`",
                        reference.text, reference.scope
                    ),
                )
            })?;
        let position = &value.position;
        let text = |text: &str| Value::string(text, position.clone());
        let entry = |key: &str, value: Value| MapEntry {
            key: key.to_owned(),
            key_position: position.clone(),
            value,
        };
        let scope_key = match &reference.scope_key {
            Some(scope_key) => text(scope_key),
            None => Value::new(ValueKind::Null, position.clone()),
        };
        let reference_value = Value::new(
            ValueKind::Map(vec![
                entry("scope_key", scope_key),
                entry("name", text(&reference.name)),
                entry("path", text(&reference.path)),
            ]),
            position.clone(),
        );
        let frame = self.frame.with_reference(&reference_value);
        fill_expression(&rule.template, &frame, self)
    }
}

fn matches(rule: &ReferenceRule, reference: &Reference) -> bool {
    rule.scope == reference.scope
        && rule.scope_keys.as_ref().is_none_or(|keys| {
            reference
                .scope_key
                .as_ref()
                .is_some_and(|scope_key| keys.contains(scope_key))
        })
        && rule
            .path_matches
            .as_ref()
            .is_none_or(|pattern| pattern.is_match(&reference.path).unwrap_or(false))
}
