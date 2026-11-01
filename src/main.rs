//! `tfgen`: generates Terraform from YAML DSLs defined by configuration.

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
use yaml_dsl_compiler::Diagnostic;

use crate::config_files::{DSL_CONFIG_FILE, LoadedConfig};
use crate::disk_source::DiskSource;
use crate::error_report::{ReportStyle, Severity, SourceFiles, report};
use crate::generated_files::{FileChange, write_stack_files};
use crate::schema_overrides::{
    SCHEMA_VARIABLE, SchemaOverrides, schema_overrides, schema_pair, variable_pairs,
};
use crate::schema_url_copies::{SchemaUrlCopies, schema_url_copies_directory};
use crate::stack_descriptor::read_globals;
use crate::stack_discovery::{config_directory_of, git_files, normalized, repository_root};
use crate::stack_generation::{StackJob, generate_stack};

const USAGE: &str = "usage: tfgen [--stack <directory>]... [--schema <dsl id>=<path or URL>]...
             [--color | --no-color]

Generates every stack of the repo holding the working directory, or each stack
--stack names. --schema, and TFGEN_SCHEMA as pairs separated by commas, name the
schema a DSL's documents are read against for this run, ahead of their modelines.
Color is on where the output is a terminal and NO_COLOR is unset.";

const STACK_FLAG: &str = "--stack";
const HELP_FLAG: &str = "--help";
const SCHEMA_FLAG: &str = "--schema";
const COLOR_FLAG: &str = "--color";
const NO_COLOR_FLAG: &str = "--no-color";
const NO_COLOR_VARIABLE: &str = "NO_COLOR";

/// What the arguments ask for: the stacks, the schemas named for the run, and
/// color when a flag forces it.
#[derive(Debug, PartialEq, Eq)]
struct CommandLine {
    stacks: Vec<String>,
    schemas: Vec<(String, String)>,
    color: Option<bool>,
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command = match command_line(&arguments) {
        Ok(Some(command)) => command,
        Ok(None) => {
            println!("{USAGE}");
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

/// The arguments' request, or `None` for a request for usage.
fn command_line(arguments: &[String]) -> Result<Option<CommandLine>, String> {
    let mut command = CommandLine {
        stacks: Vec::new(),
        schemas: Vec::new(),
        color: None,
    };
    let mut remaining = arguments.iter();
    while let Some(argument) = remaining.next() {
        match argument.as_str() {
            HELP_FLAG => return Ok(None),
            STACK_FLAG => {
                let directory = remaining
                    .next()
                    .ok_or_else(|| format!("{STACK_FLAG} takes a directory"))?;
                command.stacks.push(directory.clone());
            }
            SCHEMA_FLAG => {
                let pair = remaining
                    .next()
                    .ok_or_else(|| format!("{SCHEMA_FLAG} takes `<dsl id>=<path or URL>`"))?;
                command.schemas.push(schema_pair(pair)?);
            }
            COLOR_FLAG => command.color = Some(true),
            NO_COLOR_FLAG => command.color = Some(false),
            _ => return Err(format!("`{argument}` is not an argument tfgen takes")),
        }
    }
    Ok(Some(command))
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

/// A stack a run may generate: its directory, its descriptor and its config.
struct StackCandidate {
    directory: PathBuf,
    descriptor: PathBuf,
    config_directory: PathBuf,
    named: bool,
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
    let schema_urls = SchemaUrlCopies::new(schema_url_copies_directory());
    let mut candidates = Vec::new();
    if stack_arguments.is_empty() {
        let files = git_files(&root, None)?;
        let config_directories: BTreeSet<PathBuf> = files
            .iter()
            .filter(|file| file.file_name().is_some_and(|name| name == DSL_CONFIG_FILE))
            .filter_map(|file| file.parent().map(Path::to_path_buf))
            .collect();
        for directory in &config_directories {
            load_config(&mut configs, directory, &source);
        }
        for file in &files {
            let Some(config_directory) =
                config_directory_of(file, &root, Some(&config_directories))
            else {
                continue;
            };
            let Some(Ok(config)) = configs.get(&config_directory) else {
                continue;
            };
            let named_descriptor = file.file_name().is_some_and(|name| {
                config
                    .descriptor_names()
                    .iter()
                    .any(|descriptor| name == descriptor.as_str())
            });
            if named_descriptor {
                candidates.push(StackCandidate {
                    directory: file.parent().unwrap_or(&root).to_path_buf(),
                    descriptor: file.clone(),
                    config_directory,
                    named: false,
                });
            }
        }
    } else {
        for argument in stack_arguments {
            let directory = normalized(&working_directory.join(argument));
            if !directory.is_dir() {
                return Err(format!("{argument} is not a directory"));
            }
            let config_directory = config_directory_of(&directory, &root, None)
                .ok_or_else(|| format!("{argument}: no {DSL_CONFIG_FILE} holds this stack"))?;
            load_config(&mut configs, &config_directory, &source);
            let descriptor_names = match configs.get(&config_directory) {
                Some(Ok(config)) => config.descriptor_names(),
                _ => continue,
            };
            let found = candidates.len();
            for name in descriptor_names {
                let descriptor = directory.join(&name);
                if descriptor.is_file() {
                    candidates.push(StackCandidate {
                        directory: directory.clone(),
                        descriptor,
                        config_directory: config_directory.clone(),
                        named: true,
                    });
                }
            }
            if candidates.len() == found {
                return Err(format!("{argument} holds no stack descriptor"));
            }
        }
    }
    let mut succeeded = true;
    for (directory, result) in &configs {
        if let Err(diagnostic) = result {
            errors.diagnostic(diagnostic);
            let file = match &diagnostic.position {
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
        if globals.get("tfgen").is_none() {
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

type LoadedConfigs = BTreeMap<PathBuf, Result<LoadedConfig, Diagnostic>>;

fn load_config(configs: &mut LoadedConfigs, directory: &Path, source: &Arc<DiskSource>) {
    configs
        .entry(directory.to_path_buf())
        .or_insert_with(|| LoadedConfig::load(directory, source));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    #[test]
    fn the_stack_flag_repeats_and_nothing_else_is_taken() {
        assert_eq!(
            command_line(&arguments(&[
                "--stack",
                "a",
                "--stack",
                "b",
                "--schema",
                "mock-dsl=mock.json",
                "--no-color"
            ]))
            .unwrap(),
            Some(CommandLine {
                stacks: vec!["a".to_owned(), "b".to_owned()],
                schemas: vec![("mock-dsl".to_owned(), "mock.json".to_owned())],
                color: Some(false),
            })
        );
        assert_eq!(command_line(&arguments(&["--help"])).unwrap(), None);
        assert!(command_line(&arguments(&["--stack"])).is_err());
        assert!(command_line(&arguments(&["-s", "a"])).is_err());
    }
}
