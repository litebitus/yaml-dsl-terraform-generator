//! `tfgen`: generates Terraform from YAML DSLs defined by configuration.

mod command_line;
mod config_files;
mod disk_source;
mod error_report;
mod format_function;
mod generated_files;
mod hcl_conversion;
mod ref_paths;
mod ref_scope;
mod schema_cache;
mod schema_credentials;
mod schema_lookup;
mod schema_overrides;
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
use yaml_dsl_spec_diagnostic_renderer::RenderStyle;

use crate::command_line::{Request, USAGE, command_line};
use crate::config_files::{
    DEFAULT_SCHEMA_CACHE_CAPACITY, DSL_CONFIG_FILE, LoadedConfig, LoadedConfigs,
};
use crate::disk_source::DiskSource;
use crate::error_report::{SourceFiles, plain_error, report, stack_changes, stack_summary};
use crate::generated_files::{FileChange, write_stack_files};
use crate::schema_cache::{SchemaCache, schema_cache_directory};
use crate::schema_overrides::{SCHEMA_VARIABLE, SchemaOverrides, schema_overrides, variable_pairs};
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
const CLICOLOR_FORCE_VARIABLE: &str = "CLICOLOR_FORCE";
const COLUMNS_VARIABLE: &str = "COLUMNS";

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
        output_colored(
            command.color,
            terminal,
            std::env::var_os(NO_COLOR_VARIABLE).is_some(),
            std::env::var_os(CLICOLOR_FORCE_VARIABLE)
                .is_some_and(|forced| !forced.is_empty() && forced != "0"),
        )
    };
    let columns = match columns_variable() {
        Ok(columns) => columns,
        Err(message) => {
            eprintln!("tfgen: {message}");
            return ExitCode::from(2);
        }
    };
    let terminal_width =
        |size: Option<(Width, Height)>| size.map(|(Width(width), _)| width.into()).or(columns);
    let styles = OutputStyles {
        output: RenderStyle {
            color: colored(std::io::stdout().is_terminal()),
            width: terminal_width(terminal_size_of(std::io::stdout())),
        },
        errors: RenderStyle {
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
            eprintln!("{}", plain_error(styles.errors, &message));
            ExitCode::FAILURE
        }
    }
}

/// How standard output and standard error are painted.
#[derive(Clone, Copy)]
struct OutputStyles {
    output: RenderStyle,
    errors: RenderStyle,
}

/// Prints errors as reports, reading each source file once.
struct ErrorPrinter<'r> {
    root: &'r Path,
    style: RenderStyle,
    sources: SourceFiles,
    warned: BTreeSet<String>,
}

impl ErrorPrinter<'_> {
    fn diagnostic(&mut self, diagnostic: &Diagnostic) {
        let shown = report(diagnostic, self.root, self.style, &mut self.sources);
        eprintln!("{shown}\n");
    }

    /// Each warning once, however many stacks read what it is about.
    fn warning(&mut self, warning: &Diagnostic) {
        let shown = report(warning, self.root, self.style, &mut self.sources);
        if self.warned.insert(shown.clone()) {
            eprintln!("{shown}\n");
        }
    }

    fn message(&self, message: &str) {
        eprintln!("{}", plain_error(self.style, message));
    }
}

/// The width `COLUMNS` states for an output that is not a terminal; none where
/// it is unset or empty.
fn columns_variable() -> Result<Option<usize>, String> {
    match std::env::var(COLUMNS_VARIABLE) {
        Ok(text) if text.is_empty() => Ok(None),
        Ok(text) => match text.parse::<usize>() {
            Ok(columns) if columns > 0 => Ok(Some(columns)),
            _ => Err(format!(
                "COLUMNS is a whole number of columns, from 1: `{text}`"
            )),
        },
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err("COLUMNS is a whole number of columns, from 1".to_owned())
        }
    }
}

/// Whether an output is drawn in color, from the flags and the color variables.
fn output_colored(flag: Option<bool>, terminal: bool, no_color: bool, forced: bool) -> bool {
    flag.unwrap_or(!no_color && (forced || terminal))
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
    let schema_cache = SchemaCache::new(schema_cache_directory(), schema_cache_capacity);
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
            schema_cache: &schema_cache,
        });
    }
    let results: Vec<_> = jobs.par_iter().map(generate_stack).collect();
    schema_cache.drop_least_recently_used();
    let mut changed_stacks: Vec<(String, Vec<(String, FileChange)>)> = Vec::new();
    let mut compared = 0;
    for (job, result) in jobs.iter().zip(results) {
        match result {
            Ok(stack) => {
                for rejection in &stack.rejections {
                    errors.diagnostic(rejection);
                }
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
                    stack_summary(
                        styles.errors,
                        &shown(&job.directory, &root),
                        diagnostics.len()
                    )
                );
                succeeded = false;
            }
        }
    }
    changed_stacks.sort_by(|first, second| first.0.cmp(&second.0));
    for (stack, files) in &changed_stacks {
        println!("{}", stack_changes(styles.output, stack, files));
    }
    println!(
        "{}",
        error_report::changed_stacks(changed_stacks.len(), compared)
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
