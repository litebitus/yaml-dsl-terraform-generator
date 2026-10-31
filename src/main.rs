//! `tfgen`: generates Terraform from YAML DSLs defined by configuration.

mod config_files;
mod disk_source;
mod format_function;
mod generated_files;
mod hcl_conversion;
mod ref_paths;
mod ref_scope;
mod stack_descriptor;
mod stack_discovery;
mod stack_generation;
mod template_filling;
mod value_writing;
mod yaml_values;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use rayon::prelude::*;
use yaml_dsl_compiler::Diagnostic;

use crate::config_files::{DSL_CONFIG_FILE, LoadedConfig};
use crate::disk_source::DiskSource;
use crate::generated_files::write_stack_files;
use crate::stack_descriptor::read_globals;
use crate::stack_discovery::{config_directory_of, git_files, normalized, repository_root};
use crate::stack_generation::{StackJob, generate_stack};

const USAGE: &str = "usage: tfgen [--stack <directory>]...

Generates every stack of the repo holding the working directory, or each stack
--stack names.";

const STACK_FLAG: &str = "--stack";
const HELP_FLAG: &str = "--help";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let stacks = match stack_arguments(&arguments) {
        Ok(Some(stacks)) => stacks,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("tfgen: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&stacks) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("tfgen: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The stack directories the arguments name, or `None` for a request for usage.
fn stack_arguments(arguments: &[String]) -> Result<Option<Vec<String>>, String> {
    let mut stacks = Vec::new();
    let mut remaining = arguments.iter();
    while let Some(argument) = remaining.next() {
        if argument == HELP_FLAG {
            return Ok(None);
        }
        if argument == STACK_FLAG {
            let directory = remaining
                .next()
                .ok_or_else(|| format!("{STACK_FLAG} takes a directory"))?;
            stacks.push(directory.clone());
        } else {
            return Err(format!("`{argument}` is not an argument tfgen takes"));
        }
    }
    Ok(Some(stacks))
}

/// A stack a run may generate: its directory, its descriptor and its config.
struct StackCandidate {
    directory: PathBuf,
    descriptor: PathBuf,
    config_directory: PathBuf,
    named: bool,
}

/// Generates the stacks; `false` where any stack has an error.
fn run(stack_arguments: &[String]) -> Result<bool, String> {
    let working_directory =
        std::env::current_dir().map_err(|error| format!("the working directory: {error}"))?;
    let root = repository_root(&working_directory)?;
    let source = Arc::new(DiskSource);
    let mut configs = LoadedConfigs::new();
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
            eprintln!("{}: {diagnostic}", directory.display());
            succeeded = false;
        }
    }
    let mut jobs = Vec::new();
    let mut seen: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for candidate in &candidates {
        if let Some(first) = seen.insert(candidate.directory.clone(), candidate.descriptor.clone())
        {
            eprintln!(
                "{}: two stack descriptors, {} and {}",
                candidate.directory.display(),
                first.display(),
                candidate.descriptor.display()
            );
            succeeded = false;
            continue;
        }
        let Some(Ok(config)) = configs.get(&candidate.config_directory) else {
            continue;
        };
        let globals = match read_globals(&candidate.descriptor) {
            Ok(globals) => globals,
            Err(diagnostic) => {
                eprintln!("{diagnostic}");
                succeeded = false;
                continue;
            }
        };
        if globals.get("tfgen").is_none() {
            if candidate.named {
                eprintln!(
                    "{}: the stack's globals hold no `tfgen`, so it is not tfgen's",
                    candidate.descriptor.display()
                );
                succeeded = false;
            }
            continue;
        }
        jobs.push(StackJob {
            directory: candidate.directory.clone(),
            globals,
            config,
            repository_root: &root,
        });
    }
    let results: Vec<_> = jobs.par_iter().map(generate_stack).collect();
    for result in results {
        match result {
            Ok(stack) => match write_stack_files(&stack.directory, &stack.files) {
                Ok(changes) => {
                    for path in &changes.written {
                        println!("wrote {}", shown(path, &root));
                    }
                    for path in &changes.deleted {
                        println!("deleted {}", shown(path, &root));
                    }
                }
                Err(message) => {
                    eprintln!("{message}");
                    succeeded = false;
                }
            },
            Err(diagnostics) => {
                for diagnostic in diagnostics {
                    eprintln!("{diagnostic}");
                }
                succeeded = false;
            }
        }
    }
    Ok(succeeded)
}

type LoadedConfigs = BTreeMap<PathBuf, Result<LoadedConfig, Diagnostic>>;

fn load_config(configs: &mut LoadedConfigs, directory: &Path, source: &Arc<DiskSource>) {
    configs
        .entry(directory.to_path_buf())
        .or_insert_with(|| LoadedConfig::load(directory, source));
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
            stack_arguments(&arguments(&["--stack", "a", "--stack", "b"])).unwrap(),
            Some(vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(stack_arguments(&arguments(&["--help"])).unwrap(), None);
        assert!(stack_arguments(&arguments(&["--stack"])).is_err());
        assert!(stack_arguments(&arguments(&["-s", "a"])).is_err());
    }
}
