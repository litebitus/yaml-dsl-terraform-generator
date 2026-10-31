//! Fills a template's refs, parses the result as HCL, and turns it into the
//! crate's output tree.

use std::sync::Arc;

use yaml_dsl_compiler::output::{BodyItem, Expression};
use yaml_dsl_compiler::resolved::Value;
use yaml_dsl_compiler::{Diagnostic, Position};

use crate::config_files::TemplateText;
use crate::hcl_conversion::{Sentinels, body_items, expression};
use crate::ref_paths::{RefSpan, find_refs};
use crate::ref_scope::RefFrame;
use crate::value_writing::ValueWriter;
use crate::yaml_values::scalar_of;

const EXPRESSION_ATTRIBUTE: &str = "__tfgen_expression__";

/// Where a ref stands in a template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefPlace {
    QuotedText,
    HeredocText,
    JoinedCode,
    WholeValue,
    AloneOnLine,
}

pub fn fill_body(
    template: &TemplateText,
    frame: &RefFrame<'_>,
    writer: &ValueWriter<'_>,
) -> Result<Vec<BodyItem>, Diagnostic> {
    let mut sentinels = Sentinels::default();
    let filled = fill_text(&template.text, template, frame, writer, &mut sentinels)?;
    let body =
        hcl_edit::parser::parse_body(&filled).map_err(|error| parse_error(template, &error))?;
    body_items(&body, &sentinels).map_err(|message| Diagnostic::at(&template.start, message))
}

pub fn fill_expression(
    template: &TemplateText,
    frame: &RefFrame<'_>,
    writer: &ValueWriter<'_>,
) -> Result<Expression, Diagnostic> {
    let mut sentinels = Sentinels::default();
    let filled = fill_text(&template.text, template, frame, writer, &mut sentinels)?;
    let wrapped = format!("{EXPRESSION_ATTRIBUTE} = {filled}\n");
    let body = hcl_edit::parser::parse_body(&wrapped).map_err(|error| {
        Diagnostic::at(
            &template.start,
            format!("`{filled}` is not an HCL expression: {}", error.message()),
        )
    })?;
    let attribute = body.get_attribute(EXPRESSION_ATTRIBUTE).ok_or_else(|| {
        Diagnostic::at(&template.start, format!("`{filled}` is not one expression"))
    })?;
    expression(&attribute.value, &sentinels)
        .map_err(|message| Diagnostic::at(&template.start, message))
}

fn fill_text(
    text: &str,
    template: &TemplateText,
    frame: &RefFrame<'_>,
    writer: &ValueWriter<'_>,
    sentinels: &mut Sentinels,
) -> Result<String, Diagnostic> {
    let spans = find_refs(text).map_err(|message| Diagnostic::at(&template.start, message))?;
    let places = ref_places(text, &spans);
    let mut filled = String::with_capacity(text.len());
    let mut cursor = 0;
    for (span, place) in spans.iter().zip(places) {
        filled.push_str(&text[cursor..span.start]);
        cursor = span.end;
        let position = offset_position(template, text, span.start);
        match place {
            RefPlace::QuotedText => {
                filled.push_str(&escape_quoted(&scalar_of(frame, &span.path, &position)?));
            }
            RefPlace::HeredocText => {
                filled.push_str(&escape_template(&scalar_of(frame, &span.path, &position)?));
            }
            RefPlace::JoinedCode => filled.push_str(&scalar_of(frame, &span.path, &position)?),
            RefPlace::WholeValue => {
                let value = frame.read(&span.path, &position)?;
                let written = match value {
                    Some(value) => writer.expression(&value)?,
                    None => Expression::Null,
                };
                filled.push_str(&sentinels.value(written));
            }
            RefPlace::AloneOnLine => {
                let Some(value) = frame.read(&span.path, &position)? else {
                    continue;
                };
                filled.push_str(&sentinels.entries(map_entries(
                    &value,
                    writer,
                    &span.path.to_string(),
                    &position,
                )?));
            }
        }
    }
    filled.push_str(&text[cursor..]);
    Ok(filled)
}

fn map_entries(
    value: &Value,
    writer: &ValueWriter<'_>,
    path: &str,
    position: &Position,
) -> Result<Vec<(String, Expression)>, Diagnostic> {
    let entries = value.local_value().entries().ok_or_else(|| {
        Diagnostic::at(
            position,
            format!("`{path}` stands alone in a body, and holds a value that is not a map"),
        )
    })?;
    entries
        .iter()
        .map(|entry| Ok((entry.key.clone(), writer.expression(&entry.value)?)))
        .collect()
}

#[derive(Debug)]
enum LexFrame {
    Code {
        closes_interpolation: bool,
        depth: usize,
    },
    Quoted,
    Heredoc {
        delimiter: String,
    },
}

/// Where each ref of `spans` stands, by a lexer that tells HCL code from the text
/// of strings and heredocs.
fn ref_places(text: &str, spans: &[RefSpan]) -> Vec<RefPlace> {
    let bytes = text.as_bytes();
    let mut places = Vec::with_capacity(spans.len());
    let mut frames = vec![LexFrame::Code {
        closes_interpolation: false,
        depth: 0,
    }];
    let mut spans = spans.iter().peekable();
    let mut index = 0;
    let mut line_start = true;
    while index < bytes.len() {
        if let Some(span) = spans.peek()
            && span.start == index
        {
            places.push(match frames.last() {
                Some(LexFrame::Quoted) => RefPlace::QuotedText,
                Some(LexFrame::Heredoc { .. }) => RefPlace::HeredocText,
                _ => code_place(text, span),
            });
            index = span.end;
            spans.next();
            line_start = false;
            continue;
        }
        let rest = &text[index..];
        let at_line_start = line_start;
        line_start = bytes[index] == b'\n';
        match frames.last_mut() {
            Some(LexFrame::Heredoc { delimiter }) => {
                if at_line_start
                    && rest
                        .lines()
                        .next()
                        .is_some_and(|line| line.trim() == delimiter)
                {
                    frames.pop();
                    index += rest.find('\n').unwrap_or(rest.len());
                    continue;
                }
                if rest.starts_with("$${") || rest.starts_with("%%{") {
                    index += 3;
                } else if rest.starts_with("${") || rest.starts_with("%{") {
                    frames.push(LexFrame::Code {
                        closes_interpolation: true,
                        depth: 0,
                    });
                    index += 2;
                } else {
                    index += 1;
                }
            }
            Some(LexFrame::Quoted) => {
                if rest.starts_with('\\') {
                    index += 2;
                } else if rest.starts_with("$${") || rest.starts_with("%%{") {
                    index += 3;
                } else if rest.starts_with("${") || rest.starts_with("%{") {
                    frames.push(LexFrame::Code {
                        closes_interpolation: true,
                        depth: 0,
                    });
                    index += 2;
                } else {
                    if rest.starts_with('"') {
                        frames.pop();
                    }
                    index += 1;
                }
            }
            Some(LexFrame::Code {
                closes_interpolation,
                depth,
            }) => {
                if rest.starts_with('"') {
                    frames.push(LexFrame::Quoted);
                    index += 1;
                } else if let Some(delimiter) = heredoc_delimiter(rest) {
                    index += rest.find('\n').map_or(rest.len(), |end| end + 1);
                    line_start = true;
                    frames.push(LexFrame::Heredoc { delimiter });
                } else if rest.starts_with('#') || rest.starts_with("//") {
                    index += rest.find('\n').unwrap_or(rest.len());
                } else if rest.starts_with("/*") {
                    index += rest.find("*/").map_or(rest.len(), |end| end + 2);
                } else if rest.starts_with('{') {
                    *depth += 1;
                    index += 1;
                } else if rest.starts_with('}') {
                    if *depth == 0 && *closes_interpolation {
                        frames.pop();
                    } else {
                        *depth = depth.saturating_sub(1);
                    }
                    index += 1;
                } else {
                    index += rest.chars().next().map_or(1, char::len_utf8);
                }
            }
            None => index += 1,
        }
    }
    places
}

/// The delimiter of a heredoc that opens at the start of `rest`.
fn heredoc_delimiter(rest: &str) -> Option<String> {
    let opener = rest.strip_prefix("<<")?;
    let opener = opener.strip_prefix('-').unwrap_or(opener);
    let line = opener.lines().next()?.trim_end();
    let valid = !line.is_empty()
        && line
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_');
    valid.then(|| line.to_owned())
}

fn code_place(text: &str, span: &RefSpan) -> RefPlace {
    let joins = |character: Option<char>| {
        character.is_some_and(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '_' | '.' | '[' | ']' | ')' | '-')
        })
    };
    let before = text[..span.start].chars().next_back();
    let after = text[span.end..].chars().next();
    let beside_ref = text[..span.start].ends_with("}}") || text[span.end..].starts_with("{{");
    if beside_ref || joins(before) || joins(after) {
        return RefPlace::JoinedCode;
    }
    let line_before = text[..span.start].rsplit('\n').next().unwrap_or("");
    let line_after = text[span.end..].split('\n').next().unwrap_or("");
    if line_before.trim().is_empty() && line_after.trim().is_empty() {
        RefPlace::AloneOnLine
    } else {
        RefPlace::WholeValue
    }
}

fn offset_position(template: &TemplateText, text: &str, offset: usize) -> Position {
    let before = &text[..offset];
    let line_offset = before.matches('\n').count();
    let column = before.rsplit('\n').next().map_or(0, str::len) + 1;
    let start = &template.start;
    if line_offset == 0 {
        Position::new(&start.file, start.line, start.column + column - 1)
    } else {
        Position::new(&start.file, start.line + line_offset, column)
    }
}

fn parse_error(template: &TemplateText, error: &hcl_edit::parser::Error) -> Diagnostic {
    let location = error.location();
    let file: Arc<str> = Arc::clone(&template.start.file);
    Diagnostic::at(
        &Position::new(
            &file,
            template.start.line + location.line() - 1,
            location.column(),
        ),
        format!("the filled template is not HCL: {}", error.message()),
    )
}

fn escape_template(text: &str) -> String {
    text.replace("${", "$${").replace("%{", "%%{")
}

fn escape_quoted(text: &str) -> String {
    escape_template(text)
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places(text: &str) -> Vec<RefPlace> {
        ref_places(text, &find_refs(text).unwrap())
    }

    #[test]
    fn a_ref_takes_its_place_from_the_code_around_it() {
        let template = concat!(
            "module \"{{each.scope_key}}_{{each.name}}\" {\n",
            "  config = merge({{each.value}}, { a = 1 })\n",
            "  name   = \"${replace(\"{{stack.owner}}\", \"-\", \"_\")}\"\n",
            "  value  = module.{{each.name}}.o\n",
            "  script = <<-EOT\n    echo {{stack._env}}\n  EOT\n",
            "  {{fold[\"locals\"]}}\n",
            "}\n",
        );
        assert_eq!(
            places(template),
            [
                RefPlace::QuotedText,
                RefPlace::QuotedText,
                RefPlace::WholeValue,
                RefPlace::QuotedText,
                RefPlace::JoinedCode,
                RefPlace::HeredocText,
                RefPlace::AloneOnLine,
            ]
        );
    }

    #[test]
    fn text_in_a_string_is_escaped_for_hcl() {
        assert_eq!(escape_quoted("a\"b ${x}"), "a\\\"b $${x}");
    }
}
