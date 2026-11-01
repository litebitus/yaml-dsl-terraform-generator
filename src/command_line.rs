//! What tfgen's arguments ask for. The flag table is the one source the parser
//! and completion read.

use crate::schema_overrides::schema_pair;
use crate::shell_completion::Shell;

pub const USAGE: &str = "usage: tfgen [--stack <directory>]... [--schema <dsl id>=<path or URL>]...
             [--color | --no-color]
       tfgen completion bash | zsh | fish
       tfgen --version | -v

Generates every stack of the repo holding the working directory, or each stack
--stack names. --schema, and TFGEN_SCHEMA as pairs separated by commas, name the
schema a DSL's documents are read against for this run, ahead of their modelines.
Color is on where the output is a terminal and NO_COLOR is unset. `completion`
prints the shell's completion script.";

pub const COMPLETION_COMMAND: &str = "completion";
/// The hidden command a completion script calls for a command line's candidates.
pub const CANDIDATES_COMMAND: &str = "__complete";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagKind {
    Stack,
    Schema,
    Color,
    NoColor,
    Version,
    Help,
}

pub struct Flag {
    pub kind: FlagKind,
    pub names: &'static [&'static str],
    pub description: &'static str,
}

pub const FLAGS: [Flag; 6] = [
    Flag {
        kind: FlagKind::Stack,
        names: &["--stack"],
        description: "a stack to generate, repeatable",
    },
    Flag {
        kind: FlagKind::Schema,
        names: &["--schema"],
        description: "a DSL's schema for this run: <dsl id>=<path or URL>, repeatable",
    },
    Flag {
        kind: FlagKind::Color,
        names: &["--color"],
        description: "color the output even where it is not a terminal",
    },
    Flag {
        kind: FlagKind::NoColor,
        names: &["--no-color"],
        description: "no color, even on a terminal",
    },
    Flag {
        kind: FlagKind::Version,
        names: &["--version", "-v"],
        description: "print the release",
    },
    Flag {
        kind: FlagKind::Help,
        names: &["--help"],
        description: "print the usage",
    },
];

pub fn flag_named(argument: &str) -> Option<&'static Flag> {
    FLAGS.iter().find(|flag| flag.names.contains(&argument))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Request {
    Run(CommandLine),
    Usage,
    Version,
    CompletionScript(Shell),
    CompletionCandidates(Vec<String>),
}

/// What a run is asked for: the stacks, the schemas named for the run, and
/// color when a flag forces it.
#[derive(Debug, PartialEq, Eq)]
pub struct CommandLine {
    pub stacks: Vec<String>,
    pub schemas: Vec<(String, String)>,
    pub color: Option<bool>,
}

pub fn command_line(arguments: &[String]) -> Result<Request, String> {
    match arguments.first().map(String::as_str) {
        Some(COMPLETION_COMMAND) => {
            let [_, shell] = arguments else {
                return Err(format!(
                    "`{COMPLETION_COMMAND}` takes one shell: {}",
                    Shell::names()
                ));
            };
            return Ok(Request::CompletionScript(Shell::named(shell)?));
        }
        Some(CANDIDATES_COMMAND) => {
            return Ok(Request::CompletionCandidates(arguments[1..].to_vec()));
        }
        _ => {}
    }
    let mut command = CommandLine {
        stacks: Vec::new(),
        schemas: Vec::new(),
        color: None,
    };
    let mut remaining = arguments.iter();
    while let Some(argument) = remaining.next() {
        let flag = flag_named(argument)
            .ok_or_else(|| format!("`{argument}` is not an argument tfgen takes"))?;
        match flag.kind {
            FlagKind::Help => return Ok(Request::Usage),
            FlagKind::Version => return Ok(Request::Version),
            FlagKind::Color => command.color = Some(true),
            FlagKind::NoColor => command.color = Some(false),
            FlagKind::Stack => {
                let directory = remaining
                    .next()
                    .ok_or_else(|| format!("{argument} takes a directory"))?;
                command.stacks.push(directory.clone());
            }
            FlagKind::Schema => {
                let pair = remaining
                    .next()
                    .ok_or_else(|| format!("{argument} takes `<dsl id>=<path or URL>`"))?;
                command.schemas.push(schema_pair(pair)?);
            }
        }
    }
    Ok(Request::Run(command))
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
            Request::Run(CommandLine {
                stacks: vec!["a".to_owned(), "b".to_owned()],
                schemas: vec![("mock-dsl".to_owned(), "mock.json".to_owned())],
                color: Some(false),
            })
        );
        assert_eq!(
            command_line(&arguments(&["--help"])).unwrap(),
            Request::Usage
        );
        assert_eq!(
            command_line(&arguments(&["--stack", "a", "-v"])).unwrap(),
            Request::Version
        );
        assert_eq!(
            command_line(&arguments(&["--version"])).unwrap(),
            Request::Version
        );
        assert!(command_line(&arguments(&["--stack"])).is_err());
        assert!(command_line(&arguments(&["-s", "a"])).is_err());
    }

    #[test]
    fn completion_takes_one_shell_and_candidates_take_the_words() {
        assert_eq!(
            command_line(&arguments(&["completion", "zsh"])).unwrap(),
            Request::CompletionScript(Shell::Zsh)
        );
        assert!(command_line(&arguments(&["completion"])).is_err());
        assert!(command_line(&arguments(&["completion", "mock-shell"])).is_err());
        assert_eq!(
            command_line(&arguments(&["__complete", "--stack", ""])).unwrap(),
            Request::CompletionCandidates(arguments(&["--stack", ""]))
        );
    }
}
