//! Errors drawn by the YAML DSL standard's diagnostic renderer, each source file
//! read once, and the lines a run closes with.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use yaml_dsl_spec::Diagnostic;
use yaml_dsl_spec_diagnostic_renderer::{
    DiagnosticSource, RenderStyle, Severity, render_diagnostic,
};

use crate::generated_files::FileChange;

const RESET: &str = "\x1b[0m";
/// The roles of the devops CLI's color scheme the stack changes paint with.
const SUBJECT: &str = "\x1b[1m";
const CONVERGED: &str = "\x1b[32m";
const ATTENTION: &str = "\x1b[33m";
const UNPLACED_SOURCE: DiagnosticSource<'static> = DiagnosticSource { name: "", text: "" };

fn paint(style: RenderStyle, code: &str, text: &str) -> String {
    if style.color {
        format!("{code}{text}{RESET}")
    } else {
        text.to_owned()
    }
}

/// A stack's path in bold, then each file it changed under it, marked as
/// `terramate generate` marks one: `[+]` created and `[~]` changed in green,
/// `[-]` deleted in yellow.
pub fn stack_changes(style: RenderStyle, stack: &str, files: &[(String, FileChange)]) -> String {
    let mut text = paint(style, SUBJECT, stack);
    for (file, change) in files {
        let mark = match change {
            FileChange::Created => paint(style, CONVERGED, "[+]"),
            FileChange::Changed => paint(style, CONVERGED, "[~]"),
            FileChange::Deleted => paint(style, ATTENTION, "[-]"),
        };
        text.push_str(&format!("\n  {mark} {file}"));
    }
    text
}

/// The line closing a run: how many of the stacks it compared changed.
pub fn changed_stacks(changed: usize, compared: usize) -> String {
    let noun = if compared == 1 { "stack" } else { "stacks" };
    format!("{changed} of {compared} {noun} changed")
}

/// An error with no source to show.
pub fn plain_error(style: RenderStyle, message: &str) -> String {
    render_diagnostic(
        &Diagnostic::unplaced(message),
        Severity::Error,
        UNPLACED_SOURCE,
        style,
    )
}

/// The line closing a stack's errors.
pub fn stack_summary(style: RenderStyle, stack: &str, errors: usize) -> String {
    let counted = if errors == 1 {
        "1 error".to_owned()
    } else {
        format!("{errors} errors")
    };
    plain_error(style, &format!("{stack} generated no files: {counted}"))
}

/// The files reports show, each read once, empty where it cannot be read.
#[derive(Default)]
pub struct SourceFiles {
    shown_files: BTreeMap<String, (String, String)>,
}

impl SourceFiles {
    fn source(&mut self, file: &str, root: &Path) -> DiagnosticSource<'_> {
        let (name, text) = self.shown_files.entry(file.to_owned()).or_insert_with(|| {
            let name = Path::new(file)
                .strip_prefix(root)
                .map_or_else(|_| file.to_owned(), |path| path.display().to_string());
            (name, fs::read_to_string(file).unwrap_or_default())
        });
        DiagnosticSource { name, text }
    }
}

/// A diagnostic as the renderer draws it, its source the file it is placed in.
pub fn report(
    diagnostic: &Diagnostic,
    severity: Severity,
    root: &Path,
    style: RenderStyle,
    sources: &mut SourceFiles,
) -> String {
    let source = match &diagnostic.position {
        Some(position) => sources.source(&position.file, root),
        None => UNPLACED_SOURCE,
    };
    render_diagnostic(diagnostic, severity, source, style)
}

/// The help naming the candidate spelt closest to a name, where one is close
/// enough to be a likely fix.
pub fn did_you_mean(name: &str, candidates: &[String]) -> Option<String> {
    yaml_dsl_spec::closest_name(name, candidates)
        .map(|candidate| format!("did you mean `{candidate}`?"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use yaml_dsl_spec::Position;

    use super::*;

    const PLAIN: RenderStyle = RenderStyle {
        color: false,
        width: None,
    };

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
    fn a_report_names_its_file_from_the_root_and_shows_the_file_s_text() {
        let scratch = ScratchFile::new("mock-dsl.yml", "queue:\n  brnch: main\n");
        let file: Arc<str> = Arc::from(scratch.0.to_string_lossy().as_ref());
        let diagnostic = Diagnostic::at(&Position::new(&file, 2, 3), "`brnch` is not a field")
            .with_help(did_you_mean("brnch", &["branch".to_owned()]));
        let root = scratch.0.parent().unwrap().to_path_buf();
        let text = report(
            &diagnostic,
            Severity::Error,
            &root,
            PLAIN,
            &mut SourceFiles::default(),
        );
        assert_eq!(
            text,
            concat!(
                "error: `brnch` is not a field\n",
                " --> mock-dsl.yml:2:3\n",
                "  |\n",
                "1 | queue:\n",
                "2 |   brnch: main\n",
                "  |   ^^^^^\n",
                "  |\n",
                "  = help: did you mean `branch`?",
            )
        );
    }

    #[test]
    fn an_unreadable_file_shows_its_place_and_no_source_lines() {
        let file: Arc<str> = Arc::from("/mock-root/missing/mock-dsl.yml");
        let diagnostic = Diagnostic::at(&Position::new(&file, 4, 1), "mock problem");
        let text = report(
            &diagnostic,
            Severity::Warning,
            Path::new("/mock-root"),
            PLAIN,
            &mut SourceFiles::default(),
        );
        assert_eq!(text, "warning: mock problem\n --> missing/mock-dsl.yml:4:1");
    }

    #[test]
    fn a_stack_summary_counts_its_errors() {
        assert_eq!(
            stack_summary(PLAIN, "services/mock-api", 1),
            "error: services/mock-api generated no files: 1 error"
        );
        assert_eq!(
            stack_summary(PLAIN, "services/mock-api", 3),
            "error: services/mock-api generated no files: 3 errors"
        );
    }

    #[test]
    fn stack_changes_mark_each_file_and_the_run_counts_the_stacks_changed() {
        let files = [
            ("_main.tf".to_owned(), FileChange::Created),
            ("_outputs.tf".to_owned(), FileChange::Changed),
            ("_locals.tf".to_owned(), FileChange::Deleted),
        ];
        assert_eq!(
            stack_changes(PLAIN, "services/mock-api", &files),
            "services/mock-api\n  [+] _main.tf\n  [~] _outputs.tf\n  [-] _locals.tf"
        );
        assert_eq!(changed_stacks(1, 1), "1 of 1 stack changed");
        assert_eq!(changed_stacks(0, 141), "0 of 141 stacks changed");
    }

    #[test]
    fn a_name_spelt_far_from_every_candidate_has_no_help() {
        assert_eq!(did_you_mean("zzzzzz", &["branch".to_owned()]), None);
    }
}
