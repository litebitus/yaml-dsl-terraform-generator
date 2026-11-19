//! A resolved value as HCL: scalars, lists and maps as themselves, a reference by
//! the `references` template that matches it, a call as a Terraform call, and a
//! loop as a `for` expression.

use std::collections::BTreeSet;

use yaml_dsl_compiler::output::{
    Expression, FunctionCall, Interpolation, LoopSymbols, ObjectKey, TemplatePart, loop_expression,
};
use yaml_dsl_spec::Diagnostic;
use yaml_dsl_spec::resolved::{self, Loop, LoopBody, Reference, Target, Value, ValueKind};

use crate::config_files::ReferenceRule;
use crate::hcl_conversion::Sentinels;
use crate::ref_scope::{FixedRoot, RefFrame};
use crate::template_filling::{fill_expression, parse_expression};

/// The symbol a loop's variables are written as while its body is read for the
/// variables it reads, which no reference template writes.
const UNCHOSEN_LOOP_SYMBOL: &str = "__tfgen_loop_variable__";

pub struct ValueWriter<'w> {
    references: &'w [ReferenceRule],
    frame: RefFrame<'w>,
    /// Each loop variable around the value, by its name, with the `for` symbol it
    /// is written as; the innermost last.
    loop_symbols: Vec<(String, String)>,
}

impl<'w> ValueWriter<'w> {
    pub fn new(references: &'w [ReferenceRule], frame: RefFrame<'w>) -> Self {
        Self {
            references,
            frame,
            loop_symbols: Vec::new(),
        }
    }

    fn with_loop_symbols(&self, symbols: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut loop_symbols = self.loop_symbols.clone();
        loop_symbols.extend(symbols);
        Self {
            references: self.references,
            frame: self.frame,
            loop_symbols,
        }
    }

    /// A loop as a `for` expression, each loop variable written as a symbol that
    /// hides nothing the body reads.
    fn loop_expression(&self, left: &Loop, value: &Value) -> Result<Expression, Diagnostic> {
        let collection = self.expression(&left.collection)?;
        let unchosen = self.with_loop_symbols(
            left.variables
                .iter()
                .map(|variable| (variable.clone(), UNCHOSEN_LOOP_SYMBOL.to_owned())),
        );
        let mut read_by_body = BTreeSet::new();
        for body_value in body_values(&left.body) {
            read_by_body.extend(unchosen.expression(body_value)?.variables_read());
        }
        let mut symbols: Vec<String> = Vec::new();
        for variable in &left.variables {
            let mut symbol = variable.clone();
            while read_by_body.contains(&symbol) || symbols.contains(&symbol) {
                symbol.push('_');
            }
            symbols.push(symbol);
        }
        let loop_symbols = match symbols.as_slice() {
            [item] => LoopSymbols {
                key: None,
                item: item.clone(),
            },
            [key, item] => LoopSymbols {
                key: Some(key.clone()),
                item: item.clone(),
            },
            _ => {
                return Err(Diagnostic::at(
                    &value.position,
                    "a loop takes one or two loop variables",
                ));
            }
        };
        let chosen = self.with_loop_symbols(left.variables.iter().cloned().zip(symbols));
        loop_expression(collection, &left.body, &loop_symbols, |body_value| {
            chosen.expression(body_value)
        })
    }

    /// A loop variable as its `for` symbol, with the reference's path after it.
    fn loop_variable(
        &self,
        reference: &Reference,
        value: &Value,
    ) -> Result<Expression, Diagnostic> {
        let symbol = self
            .loop_symbols
            .iter()
            .rev()
            .find(|(name, _)| *name == reference.name)
            .map(|(_, symbol)| symbol)
            .ok_or_else(|| {
                Diagnostic::at(
                    &value.position,
                    format!("`{}` reads a variable of no loop around it", reference.text),
                )
            })?;
        parse_expression(
            &format!("{symbol}{}", reference.path),
            &Sentinels::default(),
        )
        .map_err(|message| Diagnostic::at(&value.position, message))
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
            ValueKind::Loop(left) => self.loop_expression(left, value)?,
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
        if reference.target == Target::LoopVariable {
            return self.loop_variable(reference, value);
        }
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
        let scope_key = match &reference.scope_key {
            Some(scope_key) => text(scope_key),
            None => Value::new(ValueKind::Null, position.clone()),
        };
        let reference_value = FixedRoot::Reference.value(
            position,
            vec![scope_key, text(&reference.name), text(&reference.path)],
        );
        let frame = self.frame.with_reference(&reference_value);
        fill_expression(&rule.template, &frame, self)
    }
}

/// The values one pass of a loop yields: a list body's item, or each key and value
/// of a map body.
fn body_values(body: &LoopBody) -> Vec<&Value> {
    match body {
        LoopBody::Item(item) => vec![item],
        LoopBody::Entries(entries) => entries
            .iter()
            .flat_map(|entry| [&entry.key, &entry.value])
            .collect(),
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
