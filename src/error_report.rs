//! Errors as a compiler reports them: a headline, the place, the source around it
//! with the wrong text underlined, and the rule it breaks, each line divided to
//! the terminal's width.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use yaml_dsl_spec::{Diagnostic, DiagnosticNotes, Position};

use crate::generated_files::FileChange;
use crate::line_wrapping::WrappedLine;

const CONTEXT_LINES: usize = 2;
/// The columns a source line's gutter takes beside its number: ` | `.
const GUTTER_COLUMNS: usize = 3;
const RESET: &str = "\x1b[0m";
/// The roles of the devops CLI's color scheme a report paints with.
const SUBJECT: &str = "\x1b[1m";
const LABEL: &str = "\x1b[36m";
const CONVERGED: &str = "\x1b[32m";
const ATTENTION: &str = "\x1b[33m";
const FAILURE: &str = "\x1b[91m";
const DIM: &str = "\x1b[90m";

/// Whether a report stops its stack or only warns.
#[derive(Clone, Copy)]
pub enum Severity {
    Error,
    Warning,
}

/// Whether a report is painted for a terminal, and the terminal's width its lines
/// divide to; `None` where the output is not a terminal.
#[derive(Clone, Copy)]
pub struct ReportStyle {
    pub color: bool,
    pub width: Option<usize>,
}

impl ReportStyle {
    fn paint(self, code: &str, text: &str) -> String {
        if self.color {
            format!("{code}{text}{RESET}")
        } else {
            text.to_owned()
        }
    }

    /// A stack's path in bold, then each file it changed under it, marked as
    /// `terramate generate` marks one: `[+]` created and `[~]` changed in green,
    /// `[-]` deleted in yellow.
    pub fn stack_changes(self, stack: &str, files: &[(String, FileChange)]) -> String {
        let mut text = self.paint(SUBJECT, stack);
        for (file, change) in files {
            let mark = match change {
                FileChange::Created => self.paint(CONVERGED, "[+]"),
                FileChange::Changed => self.paint(CONVERGED, "[~]"),
                FileChange::Deleted => self.paint(ATTENTION, "[-]"),
            };
            text.push_str(&format!("\n  {mark} {file}"));
        }
        text
    }

    /// The line closing a run: how many of the stacks it compared changed.
    pub fn changed_stacks(self, changed: usize, compared: usize) -> String {
        let noun = if compared == 1 { "stack" } else { "stacks" };
        format!("{changed} of {compared} {noun} changed")
    }

    /// A warning with no source to show.
    pub fn plain_warning(self, message: &str) -> String {
        self.headline("warning", ATTENTION, message)
    }

    /// An error with no source to show.
    pub fn plain_error(self, message: &str) -> String {
        self.headline("error", FAILURE, message)
    }

    /// `<word>: <message>`, the word in its color and the message as the subject.
    fn headline(self, word: &str, word_color: &str, message: &str) -> String {
        let line = WrappedLine::new(&format!("{word}: {message}"), self.width);
        let rows = line.row_texts();
        let mut shown = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                let words = row.trim_start();
                let indent = &row[..row.len() - words.len()];
                shown.push(format!("{indent}{}", self.paint(SUBJECT, words)));
                continue;
            }
            let after_word = row.strip_prefix(word).unwrap_or(row);
            let after_colon = after_word.strip_prefix(':').unwrap_or(after_word);
            let words = after_colon.trim_start();
            let mut first = format!("{}:", self.paint(word_color, word));
            if !words.is_empty() {
                first.push_str(&format!(" {}", self.paint(SUBJECT, words)));
            }
            shown.push(first);
        }
        shown.join("\n")
    }

    /// The line closing a stack's errors.
    pub fn stack_summary(self, stack: &str, errors: usize) -> String {
        let counted = if errors == 1 {
            "1 error".to_owned()
        } else {
            format!("{errors} errors")
        };
        self.plain_error(&format!("{stack} generated no files: {counted}"))
    }
}

/// The text of the files reports show, each read once.
#[derive(Default)]
pub struct SourceFiles {
    lines: BTreeMap<String, Option<Vec<String>>>,
}

impl SourceFiles {
    fn lines(&mut self, file: &str) -> Option<&[String]> {
        self.lines
            .entry(file.to_owned())
            .or_insert_with(|| {
                fs::read_to_string(file)
                    .ok()
                    .map(|text| text.lines().map(str::to_owned).collect())
            })
            .as_deref()
    }
}

/// A diagnostic as a report: the headline, the place from `root`, the lines that
/// hold the wrong text and the parents above them, and the notes.
pub fn report(
    diagnostic: &Diagnostic,
    severity: Severity,
    root: &Path,
    style: ReportStyle,
    sources: &mut SourceFiles,
) -> String {
    let mut report = match severity {
        Severity::Error => style.plain_error(&diagnostic.message),
        Severity::Warning => style.plain_warning(&diagnostic.message),
    };
    let notes = diagnostic.notes.as_deref();
    let Some(position) = &diagnostic.position else {
        return report + &notes_text(notes, 1, style);
    };
    let shown_file = Path::new(position.file.as_ref())
        .strip_prefix(root)
        .map_or_else(
            |_| position.file.to_string(),
            |path| path.display().to_string(),
        );
    let lines = sources.lines(&position.file).unwrap_or_default();
    let shown = shown_lines(lines, position.line);
    let width = shown.last().map_or(1, |line| line.to_string().len());
    let gutter = |text: &str| style.paint(DIM, text);
    let blank = gutter(&format!("{} |", " ".repeat(width)));
    report.push_str(&format!(
        "\n{}{} {shown_file}:{}:{}",
        " ".repeat(width),
        gutter("-->"),
        position.line,
        position.column
    ));
    if shown.is_empty() {
        return report + &notes_text(notes, width, style);
    }
    report.push_str(&format!("\n{blank}"));
    let room = style
        .width
        .map(|columns| columns.saturating_sub(width + GUTTER_COLUMNS));
    let mut previous: Option<usize> = None;
    for &number in &shown {
        if previous.is_some_and(|previous| number > previous + 1) {
            report.push_str(&format!("\n{}", gutter("...")));
        }
        let line = WrappedLine::new(&lines[number - 1], room);
        let underlined = (number == position.line).then(|| underlined_span(&line, position));
        for (index, row) in line.rows.iter().enumerate() {
            let row_gutter = if index == 0 {
                gutter(&format!("{number:>width$} |"))
            } else {
                blank.clone()
            };
            report.push_str(&format!("\n{row_gutter} {}", line.row_text(row)));
            if let Some((row_index, start, end)) = underlined
                && row_index == index
            {
                for underline_row in underline(&line, row, start, end, notes, room, style) {
                    report.push_str(&format!("\n{blank} {underline_row}"));
                }
            }
        }
        previous = Some(number);
    }
    report.push_str(&format!("\n{blank}"));
    report + &notes_text(notes, width, style)
}

/// The numbers of the lines a report shows: a few on each side of the wrong one,
/// and above them each line the wrong one is nested under.
fn shown_lines(lines: &[String], line: usize) -> Vec<usize> {
    if line == 0 || line > lines.len() {
        return Vec::new();
    }
    let first = line.saturating_sub(CONTEXT_LINES).max(1);
    let last = (line + CONTEXT_LINES).min(lines.len());
    let mut shown: Vec<usize> = (first..=last).collect();
    let mut indent = indentation(&lines[line - 1]);
    for number in (1..line).rev() {
        let text = &lines[number - 1];
        if text.trim().is_empty() || text.trim_start().starts_with('#') {
            continue;
        }
        let nested_under = indentation(text);
        if nested_under < indent {
            if number < first {
                shown.push(number);
            }
            indent = nested_under;
            if indent == 0 {
                break;
            }
        }
    }
    shown.sort_unstable();
    shown
}

fn indentation(text: &str) -> usize {
    text.len() - text.trim_start().len()
}

/// The wrong text's characters in a line, and the row it is underlined under: the
/// row its last character is on.
fn underlined_span(line: &WrappedLine, position: &Position) -> (usize, usize, usize) {
    let start = position.column.saturating_sub(1).min(line.characters.len());
    let rest: String = line.characters[start..].iter().collect();
    let end = (start + token_length(&rest).max(1)).min(line.characters.len().max(start + 1));
    let row_index = line
        .rows
        .iter()
        .rposition(|row| row.start < end)
        .unwrap_or(0);
    (row_index, start, end)
}

/// Carets under the part of the wrong text on `row`, and the label beside them,
/// or on its own rows under them where it does not fit beside them.
fn underline(
    line: &WrappedLine,
    row: &crate::line_wrapping::WrappedRow,
    start: usize,
    end: usize,
    notes: Option<&DiagnosticNotes>,
    room: Option<usize>,
    style: ReportStyle,
) -> Vec<String> {
    let first = start.max(row.start);
    let last = end.min(row.end).max(first);
    let caret_column = row.indent + line.columns(row.start, first);
    let carets = "^".repeat(line.columns(first, last).max(1));
    let indent = " ".repeat(caret_column);
    let Some(label) = notes.and_then(|notes| notes.label.as_deref()) else {
        return vec![format!("{indent}{}", style.paint(FAILURE, &carets))];
    };
    let label_columns = WrappedLine::new(label, None).columns(0, label.chars().count());
    let fits_beside =
        room.is_none_or(|room| caret_column + carets.len() + 1 + label_columns <= room);
    if fits_beside {
        return vec![format!(
            "{indent}{}",
            style.paint(FAILURE, &format!("{carets} {label}"))
        )];
    }
    let label_room = room.map(|room| room.saturating_sub(caret_column));
    let mut rows = vec![format!("{indent}{}", style.paint(FAILURE, &carets))];
    for label_row in WrappedLine::new(label, label_room).row_texts() {
        rows.push(format!("{indent}{}", style.paint(FAILURE, &label_row)));
    }
    rows
}

/// The length of the token `rest` starts with: a quoted string, a key up to its
/// colon, or a value up to a comment or the line's end.
fn token_length(rest: &str) -> usize {
    if rest.starts_with("{{")
        && let Some(end) = rest.find("}}")
    {
        return rest[..end + 2].chars().count();
    }
    let mut characters = rest.char_indices();
    if let Some((_, quote @ ('"' | '\''))) = characters.next() {
        let mut escaped = false;
        for (index, character) in characters {
            if escaped {
                escaped = false;
            } else if character == '\\' && quote == '"' {
                escaped = true;
            } else if character == quote {
                return rest[..=index].chars().count();
            }
        }
        return rest.chars().count();
    }
    let end = [rest.find(": "), rest.find(" #")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    let token = rest[..end].trim_end();
    token.strip_suffix(':').unwrap_or(token).chars().count()
}

/// The help naming the candidate spelt closest to a name, where one is close
/// enough to be a likely fix.
pub fn did_you_mean(name: &str, candidates: &[String]) -> Option<String> {
    yaml_dsl_spec::closest_name(name, candidates)
        .map(|candidate| format!("did you mean `{candidate}`?"))
}

/// The `= rule:` and `= help:` lines, each divided to the terminal's width.
fn notes_text(notes: Option<&DiagnosticNotes>, width: usize, style: ReportStyle) -> String {
    let Some(notes) = notes else {
        return String::new();
    };
    let mut text = String::new();
    for (name, note) in [("rule", &notes.rule), ("help", &notes.help)] {
        let Some(note) = note else {
            continue;
        };
        let marker = format!("{} =", " ".repeat(width));
        let line = WrappedLine::new(&format!("{marker} {name}: {note}"), style.width);
        for (index, row) in line.row_texts().iter().enumerate() {
            if index > 0 {
                text.push_str(&format!("\n{row}"));
                continue;
            }
            let after_name = row
                .strip_prefix(&format!("{marker} {name}:"))
                .unwrap_or_default();
            text.push_str(&format!(
                "\n{} {}{after_name}",
                style.paint(DIM, &marker),
                style.paint(LABEL, &format!("{name}:"))
            ));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::*;

    struct ScratchFile(PathBuf);

    impl ScratchFile {
        fn new(name: &str, text: &str) -> Self {
            let directory =
                std::env::temp_dir().join(format!("tfgen-report-{}", std::process::id()));
            fs::create_dir_all(&directory).unwrap();
            let path = directory.join(name);
            fs::write(&path, text).unwrap();
            Self(path)
        }
    }

    impl Drop for ScratchFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn a_report_shows_the_parents_and_the_lines_around_the_wrong_one() {
        let scratch = ScratchFile::new(
            "mock-dsl.yml",
            concat!(
                "queue:\n",
                "  name mock-orders:\n",
                "    visibility: 30\n",
                "    retention: 7\n",
                "    tags: {}\n",
                "    brnch: main\n",
                "    tier: gold\n",
            ),
        );
        let file: Arc<str> = Arc::from(scratch.0.to_string_lossy().as_ref());
        let diagnostic =
            Diagnostic::at(&Position::new(&file, 6, 5), "field `brnch` is not allowed")
                .with_label("`name mock-orders` has no field `brnch`")
                .with_rule(
                    "unevaluatedProperties, at mock.schema.json#/$defs/queue/unevaluatedProperties",
                )
                .with_help(Some("did you mean `branch`?".to_owned()));
        let root = scratch.0.parent().unwrap().to_path_buf();
        let text = report(
            &diagnostic,
            Severity::Error,
            &root,
            ReportStyle {
                color: false,
                width: None,
            },
            &mut SourceFiles::default(),
        );
        assert_eq!(
            text,
            concat!(
                "error: field `brnch` is not allowed\n",
                " --> mock-dsl.yml:6:5\n",
                "  |\n",
                "1 | queue:\n",
                "2 |   name mock-orders:\n",
                "...\n",
                "4 |     retention: 7\n",
                "5 |     tags: {}\n",
                "6 |     brnch: main\n",
                "  |     ^^^^^ `name mock-orders` has no field `brnch`\n",
                "7 |     tier: gold\n",
                "  |\n",
                "  = rule: unevaluatedProperties, ",
                "at mock.schema.json#/$defs/queue/unevaluatedProperties\n",
                "  = help: did you mean `branch`?",
            )
        );
    }

    #[test]
    fn a_long_line_wraps_and_is_underlined_under_its_last_row() {
        let scratch = ScratchFile::new(
            "tfgen.yml",
            concat!(
                "generators:\n",
                "  resource:\n",
                "    schema:\n",
                "      - \"s3://mock-bucket/schemas/{{stack.mock_version}}/mock.schema.json\"\n",
            ),
        );
        let file: Arc<str> = Arc::from(scratch.0.to_string_lossy().as_ref());
        let diagnostic = Diagnostic::at(
            &Position::new(&file, 4, 9),
            "AWS refused to read s3://mock-bucket/schemas/1.2.3/mock.schema.json",
        )
        .with_label("AccessDenied for arn:aws:iam::123456789012:user/mock-user")
        .with_help(Some(
            "use a profile allowed `s3:GetObject` on it".to_owned(),
        ));
        let root = scratch.0.parent().unwrap().to_path_buf();
        let text = report(
            &diagnostic,
            Severity::Error,
            &root,
            ReportStyle {
                color: false,
                width: Some(60),
            },
            &mut SourceFiles::default(),
        );
        assert_eq!(
            text,
            concat!(
                "error: AWS refused to read s3://mock-bucket/schemas/1.2.3/\n",
                "  mock.schema.json\n",
                " --> tfgen.yml:4:9\n",
                "  |\n",
                "1 | generators:\n",
                "2 |   resource:\n",
                "3 |     schema:\n",
                "4 |       - \"s3://mock-bucket/schemas/{{stack.mock_version}}\n",
                "  |         /mock.schema.json\"\n",
                "  |         ^^^^^^^^^^^^^^^^^^\n",
                "  |         AccessDenied for arn:aws:iam::123456789012:user/\n",
                "  |           mock-user\n",
                "  |\n",
                "  = help: use a profile allowed `s3:GetObject` on it",
            )
        );
    }

    #[test]
    fn a_parent_is_found_from_the_wrong_line_up_through_the_lines_around_it() {
        let lines: Vec<String> = [
            "data_source:",
            "  cicd_bucket:",
            "    type: remote",
            "    outputs: [cicd]",
            "    path: mock",
            "  data_at_rest_key:",
            "    type: kms_keys",
        ]
        .iter()
        .map(|line| (*line).to_owned())
        .collect();
        assert_eq!(shown_lines(&lines, 7), [1, 5, 6, 7]);
    }

    #[test]
    fn a_token_ends_at_its_colon_its_quote_or_a_comment() {
        assert_eq!(token_length("brnch: main"), 5);
        assert_eq!(token_length("\"gld\" # tier"), 5);
        assert_eq!(token_length("kms_keys  # typo"), 8);
        assert_eq!(token_length("2"), 1);
    }
}
