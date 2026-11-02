//! `tfgen`: generates Terraform from YAML DSLs defined by configuration.

mod command_line;
mod config_files;
mod disk_source;
mod error_report;
mod format_function;
mod generated_files;
mod hcl_conversion;
mod line_wrapping;
mod ref_paths;
mod ref_scope;
mod schema_credentials;
mod schema_lookup;
mod schema_overrides;
mod schema_url_copies;
mod shell_completion;
mod stack_descriptor;
mod stack_discovery;
mod stack_generation;
mod template_filling;
mod value_writing;
mod yaml_values;

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use rayon::prelude::*;
use terminal_size::{Height, Width, terminal_size_of};
use yaml_dsl_spec::Diagnostic;

use crate::command_line::{Request, USAGE, command_line};
use crate::config_files::{
    DEFAULT_SCHEMA_CACHE_CAPACITY, DSL_CONFIG_FILE, LoadedConfig, LoadedConfigs,
};
use crate::disk_source::DiskSource;
use crate::error_report::{ReportStyle, Severity, SourceFiles, report};
use crate::generated_files::{FileChange, write_stack_files};
use crate::schema_overrides::{SCHEMA_VARIABLE, SchemaOverrides, schema_overrides, variable_pairs};
use crate::schema_url_copies::{SchemaUrlCopies, schema_url_copies_directory};
use crate::shell_completion::candidates;
use crate::stack_descriptor::{TFGEN_GLOBAL, read_globals};
use crate::stack_discovery::{named_stacks, repository_root, repository_stacks};
use crate::stack_generation::{StackJob, generate_stack};

/// The release a build is of, which GoReleaser passes at build time; `dev` for
/// any other build.
const VERSION: &str = match option_env!("TFGEN_VERSION") {
    Some(version) => version,
    None => "dev",
};
const NO_COLOR_VARIABLE: &str = "NO_COLOR";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = match command_line(&arguments) {
        Ok(Request::Run(command)) => command,
        Ok(Request::Usage) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Ok(Request::Version) => {
            println!("tfgen {VERSION}");
            return ExitCode::SUCCESS;
        }
        Ok(Request::CompletionScript(shell)) => {
            print!("{}", shell.script());
            return ExitCode::SUCCESS;
        }
        Ok(Request::CompletionCandidates(words)) => {
            if let Ok(working_directory) = std::env::current_dir() {
                for candidate in candidates(&words, &working_directory) {
                    println!("{candidate}");
                }
            }
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("tfgen: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let colored = |terminal: bool| {
        command
            .color
            .unwrap_or(terminal && std::env::var_os(NO_COLOR_VARIABLE).is_none())
    };
    let terminal_width = |size: Option<(Width, Height)>| size.map(|(Width(width), _)| width.into());
    let styles = OutputStyles {
        output: ReportStyle {
            color: colored(std::io::stdout().is_terminal()),
            width: terminal_width(terminal_size_of(std::io::stdout())),
        },
        errors: ReportStyle {
            color: colored(std::io::stderr().is_terminal()),
            width: terminal_width(terminal_size_of(std::io::stderr())),
        },
    };
    let variable = match std::env::var(SCHEMA_VARIABLE)
        .ok()
        .as_deref()
        .map(variable_pairs)
    {
        Some(Ok(pairs)) => pairs,
        Some(Err(message)) => {
            eprintln!("tfgen: {message}");
            return ExitCode::from(2);
        }
        None => Vec::new(),
    };
    let working_directory = match std::env::current_dir() {
        Ok(directory) => directory,
        Err(error) => {
            eprintln!("tfgen: the working directory: {error}");
            return ExitCode::FAILURE;
        }
    };
    let overrides = schema_overrides(&variable, &command.schemas, &working_directory);
    match run(&command.stacks, &overrides, styles) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("{}", styles.errors.plain_error(&message));
            ExitCode::FAILURE
        }
    }
}

/// How standard output and standard error are painted.
#[derive(Clone, Copy)]
struct OutputStyles {
    output: ReportStyle,
    errors: ReportStyle,
}

/// Prints errors as reports, reading each source file once.
struct ErrorPrinter<'r> {
    root: &'r Path,
    style: ReportStyle,
    sources: SourceFiles,
    warned: BTreeSet<String>,
}

impl ErrorPrinter<'_> {
    fn diagnostic(&mut self, diagnostic: &Diagnostic) {
        let shown = report(
            diagnostic,
            Severity::Error,
            self.root,
            self.style,
            &mut self.sources,
        );
        eprintln!("{shown}\n");
    }

    /// Each warning once, however many stacks read what it is about.
    fn warning(&mut self, warning: &Diagnostic) {
        if self.warned.insert(warning.message.clone()) {
            let shown = report(
                warning,
                Severity::Warning,
                self.root,
                self.style,
                &mut self.sources,
            );
            eprintln!("{shown}\n");
        }
    }

    fn message(&self, message: &str) {
        eprintln!("{}", self.style.plain_error(message));
    }
}

/// Generates the stacks; `false` where any stack has an error.
fn run(
    stack_arguments: &[String],
    schema_overrides: &SchemaOverrides,
    styles: OutputStyles,
) -> Result<bool, String> {
    let working_directory =
        std::env::current_dir().map_err(|error| format!("the working directory: {error}"))?;
    let root = repository_root(&working_directory)?;
    let mut errors = ErrorPrinter {
        root: &root,
        style: styles.errors,
        sources: SourceFiles::default(),
        warned: BTreeSet::new(),
    };
    let source = Arc::new(DiskSource);
    let mut configs = LoadedConfigs::new();
    let candidates = if stack_arguments.is_empty() {
        repository_stacks(&root, &mut configs, &source)?
    } else {
        named_stacks(
            stack_arguments,
            &working_directory,
            &root,
            &mut configs,
            &source,
        )?
    };
    let schema_cache_capacity = configs
        .values()
        .filter_map(|config| config.as_ref().ok())
        .map(LoadedConfig::schema_cache_capacity)
        .max()
        .unwrap_or(DEFAULT_SCHEMA_CACHE_CAPACITY);
    let schema_urls = SchemaUrlCopies::new(schema_url_copies_directory(), schema_cache_capacity);
    let mut succeeded = true;
    for (directory, result) in &configs {
        if let Err(diagnostics) = result {
            for diagnostic in diagnostics {
                errors.diagnostic(diagnostic);
            }
            let file = match diagnostics
                .first()
                .and_then(|first| first.position.as_ref())
            {
                Some(position) => shown(Path::new(position.file.as_ref()), &root),
                None => shown(&directory.join(DSL_CONFIG_FILE), &root),
            };
            errors.message(&format!(
                "{file} holds an error, so no stack it configures was generated"
            ));
            succeeded = false;
        }
    }
    let mut jobs = Vec::new();
    let mut seen: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for candidate in &candidates {
        if let Some(first) = seen.insert(candidate.directory.clone(), candidate.descriptor.clone())
        {
            errors.message(&format!(
                "{} holds two stack descriptors, {} and {}",
                shown(&candidate.directory, &root),
                shown(&first, &root),
                shown(&candidate.descriptor, &root)
            ));
            succeeded = false;
            continue;
        }
        let Some(Ok(config)) = configs.get(&candidate.config_directory) else {
            continue;
        };
        let globals = match read_globals(&candidate.descriptor) {
            Ok(globals) => globals,
            Err(diagnostic) => {
                errors.diagnostic(&diagnostic);
                succeeded = false;
                continue;
            }
        };
        if globals.get(TFGEN_GLOBAL).is_none() {
            if candidate.named {
                errors.message(&format!(
                    "{}: the stack's globals hold no `tfgen`, so it is not tfgen's",
                    shown(&candidate.descriptor, &root)
                ));
                succeeded = false;
            }
            continue;
        }
        jobs.push(StackJob {
            directory: candidate.directory.clone(),
            globals,
            config,
            repository_root: &root,
            schema_overrides,
            schema_urls: &schema_urls,
        });
    }
    let results: Vec<_> = jobs.par_iter().map(generate_stack).collect();
    schema_urls.drop_least_recently_used();
    let mut changed_stacks: Vec<(String, Vec<(String, FileChange)>)> = Vec::new();
    let mut compared = 0;
    for (job, result) in jobs.iter().zip(results) {
        match result {
            Ok(stack) => {
                for warning in &stack.warnings {
                    errors.warning(warning);
                }
                match write_stack_files(&stack.directory, &stack.files) {
                    Ok(changes) => {
                        compared += 1;
                        if !changes.files.is_empty() {
                            let files = changes
                                .files
                                .iter()
                                .map(|(path, change)| (shown(path, &stack.directory), *change))
                                .collect();
                            changed_stacks.push((shown(&job.directory, &root), files));
                        }
                    }
                    Err(message) => {
                        errors.message(&message);
                        succeeded = false;
                    }
                }
            }
            Err(mut diagnostics) => {
                diagnostics.sort_by(|first, second| place(first).cmp(&place(second)));
                for diagnostic in &diagnostics {
                    errors.diagnostic(diagnostic);
                }
                eprintln!(
                    "{}",
                    styles
                        .errors
                        .stack_summary(&shown(&job.directory, &root), diagnostics.len())
                );
                succeeded = false;
            }
        }
    }
    changed_stacks.sort_by(|first, second| first.0.cmp(&second.0));
    for (stack, files) in &changed_stacks {
        println!("{}", styles.output.stack_changes(stack, files));
    }
    println!(
        "{}",
        styles.output.changed_stacks(changed_stacks.len(), compared)
    );
    Ok(succeeded)
}

/// Where a diagnostic stands, for reporting a stack's errors in file order.
fn place(diagnostic: &Diagnostic) -> Option<(&str, usize, usize)> {
    let position = diagnostic.position.as_ref()?;
    Some((position.file.as_ref(), position.line, position.column))
}

fn shown(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}
