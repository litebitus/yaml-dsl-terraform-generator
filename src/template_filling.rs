//! Fills a template's refs, parses the result as HCL, and turns it into the
//! crate's output tree.

use yaml_dsl_compiler::output::{BodyItem, Expression};
use yaml_dsl_compiler::resolved::Value;
use yaml_dsl_compiler::{Diagnostic, Position};

use crate::config_files::TemplateText;
use crate::hcl_conversion::{Sentinels, body_items, expression};
use crate::ref_paths::{RefPath, RefSpan, RefStep, find_refs};
use crate::ref_scope::{FixedRoot, RefFrame, absent_key, unread_root};
use crate::value_writing::ValueWriter;
use crate::yaml_values::scalar_of;

const EXPRESSION_ATTRIBUTE: &str = "__tfgen_expression__";

/// A filled template's text, and where each stretch of it came from in the
/// template, so an error in the filled text is reported at the template's text.
struct FilledText {
    text: String,
    /// Each stretch's start in the filled text, its start in the template, and
    /// whether the template's text was copied there, where a ref was filled in
    /// otherwise.
    stretches: Vec<(usize, usize, bool)>,
}

impl FilledText {
    /// The template offset of a filled offset: the same character where the
    /// template's text was copied, the ref's start where a ref was filled in.
    fn template_offset(&self, filled_offset: usize) -> usize {
        let stretch = self
            .stretches
            .iter()
            .rev()
            .find(|(filled_start, _, _)| *filled_start <= filled_offset);
        match stretch {
            Some((filled_start, template_start, true)) => {
                template_start + filled_offset - filled_start
            }
            Some((_, template_start, false)) => *template_start,
            None => 0,
        }
    }
}

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
    let body = hcl_edit::parser::parse_body(&filled.text)
        .map_err(|error| parse_error(template, &filled, &error))?;
    body_items(&body, &sentinels).map_err(|message| Diagnostic::at(&template.start, message))
}

pub fn fill_expression(
    template: &TemplateText,
    frame: &RefFrame<'_>,
    writer: &ValueWriter<'_>,
) -> Result<Expression, Diagnostic> {
    let mut sentinels = Sentinels::default();
    let filled = fill_text(&template.text, template, frame, writer, &mut sentinels)?.text;
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

/// Checks a template's refs to the roots whose fields tfgen fixes, `each` and
/// `reference`: each is read only where the template is filled with it, and only
/// for its fields. `open_roots` are the other roots the template reads.
pub fn check_fixed_roots(
    template: &TemplateText,
    fixed_roots: &[FixedRoot],
    open_roots: &[&str],
) -> Result<(), Diagnostic> {
    let spans =
        find_refs(&template.text).map_err(|message| Diagnostic::at(&template.start, message))?;
    for span in spans {
        let position = offset_position(template, &template.text, span.start);
        check_path(&span.path, fixed_roots, open_roots, &position)?;
    }
    Ok(())
}

fn check_path(
    path: &RefPath,
    fixed_roots: &[FixedRoot],
    open_roots: &[&str],
    position: &Position,
) -> Result<(), Diagnostic> {
    for step in &path.steps {
        if let RefStep::IndexedBy(index_path) = step {
            check_path(index_path, fixed_roots, open_roots, position)?;
        }
    }
    let is_fixed = |root: &FixedRoot| root.root_name() == path.root;
    let fixed_names = [FixedRoot::DeclarationEach, FixedRoot::Reference].map(FixedRoot::root_name);
    if !fixed_names.contains(&path.root.as_str()) {
        return Ok(());
    }
    let Some(root) = fixed_roots.iter().copied().find(is_fixed) else {
        let roots: Vec<String> = open_roots
            .iter()
            .copied()
            .chain(fixed_roots.iter().map(|root| root.root_name()))
            .map(str::to_owned)
            .collect();
        return Err(unread_root(path, &roots, position));
    };
    let field = match path.steps.first() {
        Some(RefStep::RequiredKey(field) | RefStep::OptionalKey(field)) => field,
        _ => return Ok(()),
    };
    if root.fields().contains(&field.as_str()) {
        return Ok(());
    }
    let fields: Vec<String> = root
        .fields()
        .iter()
        .map(|field| (*field).to_owned())
        .collect();
    Err(absent_key(path, root.root_name(), field, &fields, position))
}

fn fill_text(
    text: &str,
    template: &TemplateText,
    frame: &RefFrame<'_>,
    writer: &ValueWriter<'_>,
    sentinels: &mut Sentinels,
) -> Result<FilledText, Diagnostic> {
    let spans = find_refs(text).map_err(|message| Diagnostic::at(&template.start, message))?;
    let places = ref_places(text, &spans);
    let mut filled = String::with_capacity(text.len());
    let mut stretches = vec![(0, 0, true)];
    let mut cursor = 0;
    for (span, place) in spans.iter().zip(places) {
        filled.push_str(&text[cursor..span.start]);
        stretches.push((filled.len(), span.start, false));
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
        stretches.push((filled.len(), span.end, true));
    }
    filled.push_str(&text[cursor..]);
    Ok(FilledText {
        text: filled,
        stretches,
    })
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

/// An HCL error in the filled text, reported at the template's text it came from.
fn parse_error(
    template: &TemplateText,
    filled: &FilledText,
    error: &hcl_edit::parser::Error,
) -> Diagnostic {
    let location = error.location();
    let filled_offset = filled
        .text
        .split_inclusive('\n')
        .take(location.line().saturating_sub(1))
        .map(str::len)
        .sum::<usize>()
        + filled
            .text
            .lines()
            .nth(location.line().saturating_sub(1))
            .unwrap_or("")
            .chars()
            .take(location.column().saturating_sub(1))
            .map(char::len_utf8)
            .sum::<usize>();
    let template_offset = filled
        .template_offset(filled_offset)
        .min(template.text.len());
    Diagnostic::at(
        &offset_position(template, &template.text, template_offset),
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
    fn a_filled_offset_maps_to_the_template_text_it_came_from() {
        let template = r#"version = "{{stack.version}}" "x""#;
        let span = find_refs(template).unwrap()[0].clone();
        let filled_text = r#"version = "1.2.3" "x""#;
        let filled = FilledText {
            text: filled_text.to_owned(),
            stretches: vec![(0, 0, true), (11, span.start, false), (16, span.end, true)],
        };
        let after = filled.template_offset(filled_text.find(" \"x").unwrap() + 1);
        assert_eq!(&template[after..], r#""x""#);
        assert_eq!(filled.template_offset(13), span.start);
    }

    fn mock_template(text: &str) -> TemplateText {
        TemplateText {
            text: text.to_owned(),
            start: Position::file_start(&std::sync::Arc::from("mock.hcl")),
        }
    }

    #[test]
    fn a_fixed_root_is_read_only_where_it_is_filled_in_and_only_for_its_fields() {
        let roots = &["stack", "tfgen"];
        let reads =
            |text: &str, fixed: &[FixedRoot]| check_fixed_roots(&mock_template(text), fixed, roots);
        let declaration = &[FixedRoot::DeclarationEach];
        assert!(reads("a = {{each.value}}\n", declaration).is_ok());
        assert!(
            reads(
                "a = {{tfgen.modules[each.scope_key].source}}\n",
                declaration
            )
            .is_ok()
        );
        let absent = reads("a = \"{{tfgen.modules[each.scope_kye]}}\"\n", declaration);
        let absent = absent.unwrap_err();
        assert_eq!(
            absent.message,
            "`each.scope_kye` reads a key that is not there"
        );
        let notes = absent.notes.unwrap();
        assert_eq!(notes.help.as_deref(), Some("did you mean `scope_key`?"));
        let unread = reads("a = {{reference.name}}\n", declaration).unwrap_err();
        let position = unread.position.unwrap();
        assert_eq!((position.line, position.column), (1, 5));
        assert!(reads("a = {{each.value}}\n", &[]).is_err());
    }

    #[test]
    fn text_in_a_string_is_escaped_for_hcl() {
        assert_eq!(escape_quoted("a\"b ${x}"), "a\\\"b $${x}");
    }
}
