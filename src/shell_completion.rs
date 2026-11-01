//! Shell completion: each shell's script, and the candidates for the last word
//! of a command line, which a script asks the binary for.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::command_line::{COMPLETION_COMMAND, FLAGS, FlagKind, flag_named};
use crate::config_files::LoadedConfigs;
use crate::disk_source::DiskSource;
use crate::stack_descriptor::{TFGEN_GLOBAL, read_globals};
use crate::stack_discovery::{repository_root, repository_stacks};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

const SHELLS: [Shell; 3] = [Shell::Bash, Shell::Zsh, Shell::Fish];
/// The one hidden folder a schema path offers before a `.` is typed: where
/// `terraform init` keeps the modules, and the schemas they carry.
const TERRAFORM_FOLDER: &str = ".terraform";

const BASH_SCRIPT: &str = r#"_tfgen() {
    local line="${COMP_LINE:0:COMP_POINT}"
    local -a words
    read -ra words <<< "$line"
    [[ $line == *[[:space:]] ]] && words+=("")
    local current="${words[${#words[@]}-1]}"
    local candidate
    COMPREPLY=()
    while IFS= read -r candidate; do
        candidate="${candidate%%$'\t'*}"
        [[ $candidate == *"$current"* ]] || continue
        [[ $current == *=* && $COMP_WORDBREAKS == *=* ]] && candidate="${candidate#*=}"
        COMPREPLY+=("$candidate")
    done < <("${words[0]}" __complete "${words[@]:1}" 2>/dev/null)
    for candidate in "${COMPREPLY[@]}"; do
        [[ $candidate == *[=/] ]] && compopt -o nospace
    done
}

complete -F _tfgen tfgen
"#;

const ZSH_SCRIPT: &str = r#"#compdef tfgen
compdef _tfgen tfgen
zstyle ':completion:*:*:*tfgen:*' menu select

_tfgen() {
    local out line value description
    local -a candidates flag_names flag_descriptions spaced unspaced options
    options=(-Q -M 'l:|=* l:[:/]|=* r:|=*')
    out=$(${~${(Q)words[1]}} __complete "${(@Q)words[2,CURRENT]}" 2>/dev/null) || return 1
    for line in "${(@f)out}"; do
        [[ -n $line ]] && candidates+=("$line")
    done
    (( ${#candidates} )) || return 1
    comppostfuncs+=(__tfgen_type_no_shared_run)
    (( ${#candidates} == 1 )) && [[ ${candidates[1]} == *$'\t'?* ]] &&
        comppostfuncs+=(__tfgen_list_a_lone_description)
    for line in "${candidates[@]}"; do
        value=${line%%$'\t'*}
        description=""
        [[ $line == *$'\t'* ]] && description=${line#*$'\t'}
        if [[ $value == -* ]]; then
            flag_names+=("$value")
            flag_descriptions+=("$description")
        elif [[ $value == *[=/] ]]; then
            unspaced+=("$value")
        else
            spaced+=("$value")
        fi
    done
    (( ${#flag_names} )) && __tfgen_add_flags
    (( ${#spaced} )) && compadd "${options[@]}" -a spaced
    (( ${#unspaced} )) && compadd "${options[@]}" -S '' -a unspaced
    return 0
}

__tfgen_add_flags() {
    local -a shown lefts shown_descriptions displays hidden group=(-V options)
    local -A paired
    local left said
    local -i long short width
    for (( long = 1; long <= ${#flag_names}; long++ )); do
        [[ ${flag_names[long]} == --* ]] || continue
        left=${flag_names[long]}
        said=${flag_descriptions[long]}
        for (( short = 1; short <= ${#flag_names}; short++ )); do
            [[ ${flag_names[short]} == -[^-] && -z ${paired[$short]} ]] || continue
            [[ -n $said && ${flag_descriptions[short]} == "$said" ]] || continue
            left+=" | ${flag_names[short]}"
            paired[$short]=1
            hidden+=("${flag_names[short]}")
            break
        done
        shown+=("${flag_names[long]}")
        lefts+=("$left")
        shown_descriptions+=("${flag_descriptions[long]}")
    done
    for (( short = 1; short <= ${#flag_names}; short++ )); do
        [[ ${flag_names[short]} == --* || -n ${paired[$short]} ]] && continue
        shown+=("${flag_names[short]}")
        lefts+=("${flag_names[short]}")
        shown_descriptions+=("${flag_descriptions[short]}")
    done
    for left in "${lefts[@]}"; do
        (( ${#left} > width )) && width=${#left}
    done
    for (( long = 1; long <= ${#shown}; long++ )); do
        displays+=("${(r:width:)lefts[long]}${shown_descriptions[long]:+  -- ${shown_descriptions[long]}}")
    done
    compadd "${group[@]}" -l -d displays "${options[@]}" -a shown || return
    (( ${#hidden} )) && compadd "${group[@]}" -n "${options[@]}" -a hidden
    return 0
}

__tfgen_type_no_shared_run() {
    [[ ${compstate[insert]} == *unambiguous* ]] && (( compstate[nmatches] > 1 )) || return
    __tfgen_list_then_menu
}

__tfgen_list_a_lone_description() {
    (( compstate[nmatches] == 1 )) || return
    compstate[list]='list force'
}

__tfgen_list_then_menu() {
    if [[ ${compstate[insert]} == automenu* && -n ${compstate[old_list]} ]]; then
        compstate[insert]=menu
    else
        compstate[insert]=''
    fi
}

if [ "$funcstack[1]" = "_tfgen" ]; then
    _tfgen
fi
"#;

const FISH_SCRIPT: &str = r#"function __tfgen_candidates
    set -l words (commandline -opc)
    set -l command $words[1]
    set -e words[1]
    $command __complete $words (commandline -ct | string collect -a) 2>/dev/null
end

complete -c tfgen -f -a '(__tfgen_candidates)'
"#;

impl Shell {
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
        }
    }

    pub fn names() -> String {
        SHELLS.map(Self::name).join(", ")
    }

    pub fn named(name: &str) -> Result<Self, String> {
        SHELLS
            .into_iter()
            .find(|shell| shell.name() == name)
            .ok_or_else(|| format!("`{name}` is not a shell tfgen completes: {}", Self::names()))
    }

    pub fn script(self) -> &'static str {
        match self {
            Self::Bash => BASH_SCRIPT,
            Self::Zsh => ZSH_SCRIPT,
            Self::Fish => FISH_SCRIPT,
        }
    }
}

/// The candidates for the last of `words`, the arguments typed after `tfgen`.
/// A flag holding the word anywhere is offered, followed by a tab and its
/// description. The shell keeps the candidates holding the word anywhere; a
/// candidate ending in `=` or `/` takes no space after it.
pub fn candidates(words: &[String], working_directory: &Path) -> Vec<String> {
    let Some((current, before)) = words.split_last() else {
        return Vec::new();
    };
    match before {
        [] => described_flags(current),
        [command] if command == COMPLETION_COMMAND => {
            SHELLS.map(|shell| shell.name().to_owned()).to_vec()
        }
        [command, ..] if command == COMPLETION_COMMAND => Vec::new(),
        [.., previous] => match flag_named(previous).map(|flag| flag.kind) {
            Some(FlagKind::Stack) => stack_directories(working_directory),
            Some(FlagKind::Schema) => schema_pairs(current, working_directory),
            _ => described_flags(current),
        },
    }
}

fn described_flags(current: &str) -> Vec<String> {
    FLAGS
        .iter()
        .flat_map(|flag| {
            flag.names
                .iter()
                .filter(|name| name.contains(current))
                .map(|name| format!("{name}\t{}", flag.description))
        })
        .collect()
}

/// The repo's stacks, those whose globals hold `tfgen`, relative to the working
/// directory.
fn stack_directories(working_directory: &Path) -> Vec<String> {
    let mut configs = LoadedConfigs::new();
    let Some(stacks) = repository_root(working_directory)
        .and_then(|root| repository_stacks(&root, &mut configs, &Arc::new(DiskSource)))
        .ok()
    else {
        return Vec::new();
    };
    let directories: BTreeSet<String> = stacks
        .iter()
        .filter(|stack| {
            read_globals(&stack.descriptor).is_ok_and(|globals| globals.get(TFGEN_GLOBAL).is_some())
        })
        .map(|stack| relative_path(&stack.directory, working_directory))
        .collect();
    directories.into_iter().collect()
}

/// `<dsl id>=` for each DSL the repo generates; after the `=`, the files and
/// folders of the folder the path typed so far is in.
fn schema_pairs(current: &str, working_directory: &Path) -> Vec<String> {
    let Some((dsl_id, typed_path)) = current.split_once('=') else {
        return generated_dsl_ids(working_directory)
            .into_iter()
            .map(|dsl_id| format!("{dsl_id}="))
            .collect();
    };
    let typed_folder = &typed_path[..typed_path.rfind('/').map_or(0, |slash| slash + 1)];
    let typed_name = &typed_path[typed_folder.len()..];
    let Ok(entries) = fs::read_dir(working_directory.join(typed_folder)) else {
        return Vec::new();
    };
    let mut pairs: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let hidden = name.starts_with('.') && name != TERRAFORM_FOLDER;
            if hidden && !typed_name.starts_with('.') {
                return None;
            }
            let folder_mark = if entry.path().is_dir() { "/" } else { "" };
            Some(format!("{dsl_id}={typed_folder}{name}{folder_mark}"))
        })
        .collect();
    pairs.sort();
    pairs
}

fn generated_dsl_ids(working_directory: &Path) -> BTreeSet<String> {
    let mut configs = LoadedConfigs::new();
    let discovered = repository_root(working_directory)
        .and_then(|root| repository_stacks(&root, &mut configs, &Arc::new(DiskSource)));
    if discovered.is_err() {
        return BTreeSet::new();
    }
    configs
        .values()
        .flatten()
        .flat_map(|config| &config.dsls)
        .filter(|dsl| dsl.generated.is_some())
        .map(|dsl| dsl.id.clone())
        .collect()
}

/// `path` as typed from `base`, both absolute.
fn relative_path(path: &Path, base: &Path) -> String {
    let path_parts: Vec<Component<'_>> = path.components().collect();
    let base_parts: Vec<Component<'_>> = base.components().collect();
    let shared = path_parts
        .iter()
        .zip(&base_parts)
        .take_while(|(path_part, base_part)| path_part == base_part)
        .count();
    let mut relative = PathBuf::new();
    for _ in shared..base_parts.len() {
        relative.push("..");
    }
    relative.extend(&path_parts[shared..]);
    if relative.as_os_str().is_empty() {
        return ".".to_owned();
    }
    relative.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    #[test]
    fn the_first_word_completes_to_the_described_flags_and_no_hidden_command() {
        let first = candidates(&words(&[""]), Path::new("/mock-repo"));
        assert!(first.contains(&"--stack\ta stack to generate, repeatable".to_owned()));
        assert!(first.contains(&"--version\tprint the release".to_owned()));
        assert!(first.contains(&"-v\tprint the release".to_owned()));
        assert!(
            !first
                .iter()
                .any(|candidate| candidate.starts_with("completion"))
        );
        assert!(
            !first
                .iter()
                .any(|candidate| candidate.starts_with("__complete"))
        );
        assert_eq!(
            candidates(&words(&["completion", ""]), Path::new("/mock-repo")),
            words(&["bash", "zsh", "fish"])
        );
        assert!(candidates(&words(&["completion", "zsh", ""]), Path::new("/mock-repo")).is_empty());
    }

    #[test]
    fn a_word_after_a_flag_taking_no_value_completes_to_the_flags() {
        let after = candidates(&words(&["--no-color", "--"]), Path::new("/mock-repo"));
        assert!(
            after
                .iter()
                .any(|candidate| candidate.starts_with("--schema\t"))
        );
    }

    #[test]
    fn a_flag_is_offered_where_it_holds_the_word_anywhere() {
        assert_eq!(
            candidates(&words(&["olor"]), Path::new("/mock-repo")),
            words(&[
                "--color\tcolor the output even where it is not a terminal",
                "--no-color\tno color, even on a terminal"
            ])
        );
        assert_eq!(
            candidates(&words(&["--sta"]), Path::new("/mock-repo")),
            words(&["--stack\ta stack to generate, repeatable"])
        );
    }

    #[test]
    fn a_schema_path_completes_to_the_entries_of_its_folder() {
        let folder =
            std::env::temp_dir().join(format!("tfgen-schema-pairs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(folder.join("schemas/nested")).unwrap();
        fs::create_dir_all(folder.join("schemas/.hidden")).unwrap();
        fs::create_dir_all(folder.join("schemas/.terraform")).unwrap();
        fs::write(folder.join("schemas/mock.schema.json"), "{}").unwrap();
        assert_eq!(
            candidates(&words(&["--schema", "mock-dsl=schemas/m"]), &folder),
            words(&[
                "mock-dsl=schemas/.terraform/",
                "mock-dsl=schemas/mock.schema.json",
                "mock-dsl=schemas/nested/"
            ])
        );
        assert_eq!(
            candidates(&words(&["--schema", "mock-dsl=schemas/."]), &folder),
            words(&[
                "mock-dsl=schemas/.hidden/",
                "mock-dsl=schemas/.terraform/",
                "mock-dsl=schemas/mock.schema.json",
                "mock-dsl=schemas/nested/"
            ])
        );
        fs::remove_dir_all(&folder).unwrap();
    }

    #[test]
    fn a_path_is_shown_relative_to_the_working_directory() {
        let base = Path::new("/mock-repo/stacks");
        assert_eq!(
            relative_path(Path::new("/mock-repo/stacks/dev"), base),
            "dev"
        );
        assert_eq!(
            relative_path(Path::new("/mock-repo/other/dev"), base),
            "../other/dev"
        );
        assert_eq!(relative_path(base, base), ".");
    }

    #[test]
    fn each_script_asks_the_binary_for_candidates() {
        for shell in SHELLS {
            assert!(shell.script().contains("__complete"), "{}", shell.name());
        }
        assert!(Shell::Zsh.script().starts_with("#compdef tfgen\n"));
        assert!(Shell::named("mock-shell").is_err());
    }
}
