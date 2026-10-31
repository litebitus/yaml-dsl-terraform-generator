//! A filled template, parsed as HCL, turned into the crate's output tree, with the
//! values that stood in for refs put back in their places.

use hcl_edit::expr::{
    BinaryOperator as HclBinaryOperator, Expression as HclExpression, ObjectKey as HclObjectKey,
    TraversalOperator, UnaryOperator as HclUnaryOperator,
};
use hcl_edit::structure::{BlockLabel, Body as HclBody, Structure};
use hcl_edit::template::{Directive, Element, Strip as HclStrip, Template};
use yaml_dsl_compiler::output::{
    Attribute, BinaryOperation, BinaryOperator, Block, BodyItem, Conditional, Expression,
    ForExpression, FunctionCall, Interpolation, ObjectKey, Strip, TemplateFor, TemplateIf,
    TemplatePart, Traversal, TraversalRoot, TraversalStep, UnaryOperator,
};

const VALUE_SENTINEL_PREFIX: &str = "__tfgen_value_";
const ENTRIES_SENTINEL_PREFIX: &str = "__tfgen_entries_";
const SENTINEL_SUFFIX: &str = "__";

/// The values standing in a template where its refs were: a whole value, or the
/// entries of a map written as attributes or as a map's entries.
#[derive(Default)]
pub struct Sentinels {
    values: Vec<Expression>,
    entries: Vec<Vec<(String, Expression)>>,
}

impl Sentinels {
    /// The name standing in the template for `value`.
    pub fn value(&mut self, value: Expression) -> String {
        self.values.push(value);
        format!(
            "{VALUE_SENTINEL_PREFIX}{}{SENTINEL_SUFFIX}",
            self.values.len() - 1
        )
    }

    /// The attribute standing in the template for `entries`.
    pub fn entries(&mut self, entries: Vec<(String, Expression)>) -> String {
        self.entries.push(entries);
        format!(
            "{ENTRIES_SENTINEL_PREFIX}{}{SENTINEL_SUFFIX} = null",
            self.entries.len() - 1
        )
    }

    fn value_of(&self, name: &str) -> Option<&Expression> {
        self.values
            .get(sentinel_index(name, VALUE_SENTINEL_PREFIX)?)
    }

    fn entries_of(&self, name: &str) -> Option<&[(String, Expression)]> {
        self.entries
            .get(sentinel_index(name, ENTRIES_SENTINEL_PREFIX)?)
            .map(Vec::as_slice)
    }
}

fn sentinel_index(name: &str, prefix: &str) -> Option<usize> {
    name.strip_prefix(prefix)?
        .strip_suffix(SENTINEL_SUFFIX)?
        .parse()
        .ok()
}

pub fn body_items(body: &HclBody, sentinels: &Sentinels) -> Result<Vec<BodyItem>, String> {
    let mut items = Vec::new();
    for structure in body.iter() {
        match structure {
            Structure::Attribute(attribute) => {
                if let Some(entries) = sentinels.entries_of(attribute.key.as_str()) {
                    items.extend(entries.iter().map(|(name, value)| {
                        BodyItem::Attribute(Attribute::new(name.clone(), value.clone()))
                    }));
                    continue;
                }
                items.push(BodyItem::Attribute(Attribute::new(
                    attribute.key.as_str(),
                    expression(&attribute.value, sentinels)?,
                )));
            }
            Structure::Block(block) => {
                let labels = block
                    .labels
                    .iter()
                    .map(|label| match label {
                        BlockLabel::Ident(ident) => ident.as_str().to_owned(),
                        BlockLabel::String(text) => text.value().clone(),
                    })
                    .collect();
                items.push(BodyItem::Block(Block::new(
                    block.ident.as_str(),
                    labels,
                    body_items(&block.body, sentinels)?,
                )));
            }
        }
    }
    Ok(items)
}

pub fn expression(source: &HclExpression, sentinels: &Sentinels) -> Result<Expression, String> {
    Ok(match source {
        HclExpression::Null(_) => Expression::Null,
        HclExpression::Bool(flag) => Expression::Bool(*flag.value()),
        HclExpression::Number(number) => Expression::Number(number.value().to_string()),
        HclExpression::String(text) => Expression::String(text.value().clone()),
        HclExpression::Array(array) => Expression::List(
            array
                .iter()
                .map(|item| expression(item, sentinels))
                .collect::<Result<_, _>>()?,
        ),
        HclExpression::Object(object) => {
            let mut entries = Vec::new();
            for (key, value) in object.iter() {
                if let HclObjectKey::Ident(ident) = key
                    && let Some(spliced) = sentinels.entries_of(ident.as_str())
                {
                    entries.extend(
                        spliced
                            .iter()
                            .map(|(name, value)| (ObjectKey::Name(name.clone()), value.clone())),
                    );
                    continue;
                }
                let key = match key {
                    HclObjectKey::Ident(ident) => ObjectKey::Name(ident.as_str().to_owned()),
                    HclObjectKey::Expression(HclExpression::String(text)) => {
                        ObjectKey::Name(text.value().clone())
                    }
                    HclObjectKey::Expression(key) => {
                        ObjectKey::Expression(expression(key, sentinels)?)
                    }
                };
                entries.push((key, expression(value.expr(), sentinels)?));
            }
            Expression::Map(entries)
        }
        HclExpression::StringTemplate(template) => template_expression(template.iter(), sentinels)?,
        HclExpression::HeredocTemplate(heredoc) => {
            template_expression(heredoc.template.iter(), sentinels)?
        }
        HclExpression::Parenthesis(inner) => {
            Expression::Parenthesis(Box::new(expression(inner.inner(), sentinels)?))
        }
        HclExpression::Variable(name) => match sentinels.value_of(name.as_str()) {
            Some(value) => value.clone(),
            None => Expression::Traversal(Traversal {
                root: TraversalRoot::Variable(name.as_str().to_owned()),
                steps: Vec::new(),
            }),
        },
        HclExpression::Conditional(conditional) => Expression::Conditional(Box::new(Conditional {
            condition: expression(&conditional.cond_expr, sentinels)?,
            if_true: expression(&conditional.true_expr, sentinels)?,
            if_false: expression(&conditional.false_expr, sentinels)?,
        })),
        HclExpression::FuncCall(call) => {
            let mut function = String::new();
            for namespace in &call.name.namespace {
                function.push_str(namespace.as_str());
                function.push_str("::");
            }
            function.push_str(call.name.name.as_str());
            Expression::Call(FunctionCall {
                function,
                arguments: call
                    .args
                    .iter()
                    .map(|argument| expression(argument, sentinels))
                    .collect::<Result<_, _>>()?,
                expand_final_argument: call.args.expand_final(),
            })
        }
        HclExpression::Traversal(traversal) => {
            let root = match expression(&traversal.expr, sentinels)? {
                Expression::Traversal(Traversal { root, steps }) if steps.is_empty() => root,
                other => TraversalRoot::Expression(Box::new(other)),
            };
            let steps = traversal
                .operators
                .iter()
                .map(|operator| {
                    Ok(match operator.value() {
                        TraversalOperator::AttrSplat(_) => TraversalStep::AttributeSplat,
                        TraversalOperator::FullSplat(_) => TraversalStep::FullSplat,
                        TraversalOperator::GetAttr(name) => {
                            TraversalStep::Attribute(name.as_str().to_owned())
                        }
                        TraversalOperator::Index(index) => {
                            TraversalStep::Index(expression(index, sentinels)?)
                        }
                        TraversalOperator::LegacyIndex(index) => {
                            TraversalStep::Index(Expression::Number(index.value().to_string()))
                        }
                    })
                })
                .collect::<Result<_, String>>()?;
            Expression::Traversal(Traversal { root, steps })
        }
        HclExpression::UnaryOp(operation) => {
            let operator = match operation.operator.value() {
                HclUnaryOperator::Neg => UnaryOperator::Negate,
                HclUnaryOperator::Not => UnaryOperator::Not,
            };
            Expression::Unary(operator, Box::new(expression(&operation.expr, sentinels)?))
        }
        HclExpression::BinaryOp(operation) => Expression::Binary(Box::new(BinaryOperation {
            left: expression(&operation.lhs_expr, sentinels)?,
            operator: binary_operator(*operation.operator.value()),
            right: expression(&operation.rhs_expr, sentinels)?,
        })),
        HclExpression::ForExpr(for_expression) => Expression::For(Box::new(ForExpression {
            key_variable: for_expression
                .intro
                .key_var
                .as_ref()
                .map(|key| key.as_str().to_owned()),
            value_variable: for_expression.intro.value_var.as_str().to_owned(),
            collection: expression(&for_expression.intro.collection_expr, sentinels)?,
            key: match &for_expression.key_expr {
                Some(key) => Some(expression(key, sentinels)?),
                None => None,
            },
            value: expression(&for_expression.value_expr, sentinels)?,
            grouping: for_expression.grouping,
            condition: match &for_expression.cond {
                Some(condition) => Some(expression(&condition.expr, sentinels)?),
                None => None,
            },
        })),
    })
}

fn binary_operator(operator: HclBinaryOperator) -> BinaryOperator {
    match operator {
        HclBinaryOperator::Eq => BinaryOperator::Equal,
        HclBinaryOperator::NotEq => BinaryOperator::NotEqual,
        HclBinaryOperator::LessEq => BinaryOperator::LessOrEqual,
        HclBinaryOperator::GreaterEq => BinaryOperator::GreaterOrEqual,
        HclBinaryOperator::Less => BinaryOperator::Less,
        HclBinaryOperator::Greater => BinaryOperator::Greater,
        HclBinaryOperator::Plus => BinaryOperator::Add,
        HclBinaryOperator::Minus => BinaryOperator::Subtract,
        HclBinaryOperator::Mul => BinaryOperator::Multiply,
        HclBinaryOperator::Div => BinaryOperator::Divide,
        HclBinaryOperator::Mod => BinaryOperator::Modulo,
        HclBinaryOperator::And => BinaryOperator::And,
        HclBinaryOperator::Or => BinaryOperator::Or,
    }
}

/// A template of literal text alone is a string.
fn template_expression<'e>(
    elements: impl Iterator<Item = &'e Element>,
    sentinels: &Sentinels,
) -> Result<Expression, String> {
    let parts = template_parts(elements, sentinels)?;
    let literal: Option<String> = parts
        .iter()
        .map(|part| match part {
            TemplatePart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    Ok(match literal {
        Some(text) => Expression::String(text),
        None => Expression::Template(parts),
    })
}

fn template_parts<'e>(
    elements: impl Iterator<Item = &'e Element>,
    sentinels: &Sentinels,
) -> Result<Vec<TemplatePart>, String> {
    let mut parts = Vec::new();
    for element in elements {
        parts.push(match element {
            Element::Literal(text) => TemplatePart::Text(text.value().clone()),
            Element::Interpolation(interpolation) => TemplatePart::Interpolation(Interpolation {
                expression: expression(&interpolation.expr, sentinels)?,
                strip: strip(interpolation.strip),
            }),
            Element::Directive(directive) => match directive.as_ref() {
                Directive::If(directive) => TemplatePart::If(Box::new(TemplateIf {
                    condition: expression(&directive.if_expr.cond_expr, sentinels)?,
                    if_strip: strip(directive.if_expr.strip),
                    then_parts: nested_parts(&directive.if_expr.template, sentinels)?,
                    else_branch: match &directive.else_expr {
                        Some(otherwise) => Some((
                            strip(otherwise.strip),
                            nested_parts(&otherwise.template, sentinels)?,
                        )),
                        None => None,
                    },
                    endif_strip: strip(directive.endif_expr.strip),
                })),
                Directive::For(directive) => TemplatePart::For(Box::new(TemplateFor {
                    key_variable: directive
                        .for_expr
                        .key_var
                        .as_ref()
                        .map(|key| key.as_str().to_owned()),
                    value_variable: directive.for_expr.value_var.as_str().to_owned(),
                    collection: expression(&directive.for_expr.collection_expr, sentinels)?,
                    for_strip: strip(directive.for_expr.strip),
                    body: nested_parts(&directive.for_expr.template, sentinels)?,
                    endfor_strip: strip(directive.endfor_expr.strip),
                })),
            },
        });
    }
    Ok(parts)
}

fn nested_parts(template: &Template, sentinels: &Sentinels) -> Result<Vec<TemplatePart>, String> {
    template_parts(template.iter(), sentinels)
}

fn strip(strip: HclStrip) -> Strip {
    Strip {
        before: strip.strip_start(),
        after: strip.strip_end(),
    }
}
