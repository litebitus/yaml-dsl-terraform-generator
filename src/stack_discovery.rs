//! Which files and stacks a run reads, from git, without walking the repo.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::config_files::{DSL_CONFIG_FILE, LoadedConfigs, load_config};
use crate::disk_source::DiskSource;

/// The top of the git working tree holding `directory`.
pub fn repository_root(directory: &Path) -> Result<PathBuf, String> {
    let output = git(directory, &["rev-parse", "--show-toplevel"])?;
    let root = output.trim_end_matches(['\n', '\0']);
    Ok(PathBuf::from(root))
}

/// Files of the working tree under `directory`, tracked or not, ignored ones left
/// out; with `pattern`, those its glob names, relative to `directory`.
pub fn git_files(directory: &Path, pattern: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let mut arguments = vec![
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    ];
    let pathspec;
    if let Some(pattern) = pattern {
        pathspec = format!(":(glob){pattern}");
        arguments.extend(["--", pathspec.as_str()]);
    }
    let output = git(directory, &arguments)?;
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();
    for listed in output.split('\0').filter(|listed| !listed.is_empty()) {
        let path = normalized(&directory.join(listed));
        if path.is_file() {
            files.insert(path);
        }
    }
    Ok(files.into_iter().collect())
}

fn git(directory: &Path, arguments: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .map_err(|error| format!("running git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} in {}: {}",
            arguments.join(" "),
            directory.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| format!("git's output: {error}"))
}

/// `path` with its `.` and `..` steps folded, without reading the disk.
pub fn normalized(path: &Path) -> PathBuf {
    let mut parts: Vec<std::path::Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
                if matches!(parts.last(), Some(std::path::Component::Normal(_))) =>
            {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.iter().collect()
}

/// The config folder a path belongs to: the deepest folder at or above it, inside
/// the repo, that holds a `yaml-dsl.yml`.
pub fn config_directory_of(
    path: &Path,
    repository_root: &Path,
    known: Option<&BTreeSet<PathBuf>>,
) -> Option<PathBuf> {
    let mut directory = if path.is_dir() { path } else { path.parent()? };
    loop {
        let holds_config = match known {
            Some(known) => known.contains(directory),
            None => directory.join(DSL_CONFIG_FILE).is_file(),
        };
        if holds_config {
            return Some(directory.to_path_buf());
        }
        if directory == repository_root {
            return None;
        }
        directory = directory.parent()?;
    }
}

/// A stack a run may generate: its directory, its descriptor and its config.
pub struct StackCandidate {
    pub directory: PathBuf,
    pub descriptor: PathBuf,
    pub config_directory: PathBuf,
    /// Whether `--stack` named it.
    pub named: bool,
}

/// Every stack descriptor of the repo, by the descriptor names of the config it
/// belongs to; each config of the repo is loaded into `configs`.
pub fn repository_stacks(
    root: &Path,
    configs: &mut LoadedConfigs,
    source: &Arc<DiskSource>,
) -> Result<Vec<StackCandidate>, String> {
    let files = git_files(root, None)?;
    let config_directories: BTreeSet<PathBuf> = files
        .iter()
        .filter(|file| file.file_name().is_some_and(|name| name == DSL_CONFIG_FILE))
        .filter_map(|file| file.parent().map(Path::to_path_buf))
        .collect();
    for directory in &config_directories {
        load_config(configs, directory, source);
    }
    let mut candidates = Vec::new();
    for file in &files {
        let Some(config_directory) = config_directory_of(file, root, Some(&config_directories))
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
                directory: file.parent().unwrap_or(root).to_path_buf(),
                descriptor: file.clone(),
                config_directory,
                named: false,
            });
        }
    }
    Ok(candidates)
}

/// The stack descriptors of the directories `--stack` names, relative to the
/// working directory; each one's config is loaded into `configs`.
pub fn named_stacks(
    stack_arguments: &[String],
    working_directory: &Path,
    root: &Path,
    configs: &mut LoadedConfigs,
    source: &Arc<DiskSource>,
) -> Result<Vec<StackCandidate>, String> {
    let mut candidates = Vec::new();
    for argument in stack_arguments {
        let directory = normalized(&working_directory.join(argument));
        if !directory.is_dir() {
            return Err(format!("{argument} is not a directory"));
        }
        let config_directory = config_directory_of(&directory, root, None)
            .ok_or_else(|| format!("{argument}: no {DSL_CONFIG_FILE} holds this stack"))?;
        load_config(configs, &config_directory, source);
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
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_folds_its_dot_steps() {
        assert_eq!(
            normalized(Path::new("/repo/a/b/../../c/./d.yml")),
            PathBuf::from("/repo/c/d.yml")
        );
    }

    #[test]
    fn the_deepest_known_config_folder_owns_a_path() {
        let known: BTreeSet<PathBuf> = [PathBuf::from("/repo"), PathBuf::from("/repo/mock")]
            .into_iter()
            .collect();
        let owner = config_directory_of(
            Path::new("/repo/mock/stack/stack.tg.hcl"),
            Path::new("/repo"),
            Some(&known),
        );
        assert_eq!(owner, Some(PathBuf::from("/repo/mock")));
    }
}
