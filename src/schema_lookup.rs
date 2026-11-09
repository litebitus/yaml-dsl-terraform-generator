//! A document's schema, found as the editor finds a file's: the schema named for
//! the run first, then its modeline, then the generator's `schema` list, else the
//! DSL's `schema_search_paths`; and, for a validation fallback, one remote schema
//! of the list.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use globset::Glob;
use yaml_dsl_compiler::{DocumentSchema, SchemaProvider, Unit};
use yaml_dsl_spec::{Diagnostic, Position};

use crate::schema_cache::{ReadFailure, SchemaCache, is_schema_url};

const MODELINE_MARKER: &str = "yaml-language-server: $schema=";
const CONFIG_ROOT: &str = "{config_root}";

/// How one stack's documents of one DSL find their schema.
pub struct StackSchemas<'s> {
    pub config_root: &'s Path,
    /// The schema named for the run, ahead of the modeline: an absolute path or a URL.
    pub run_schema: Option<&'s str>,
    /// The generator's `schema` list, its refs filled, else the DSL's search paths.
    pub search_paths: &'s [SchemaSource],
    pub schema_cache: &'s SchemaCache,
    /// The one remote schema of the list a validation fallback reads, after the
    /// schema the lookup found rejected the document.
    pub fallback_source: Option<&'s SchemaSource>,
}

/// A path, path pattern or URL a schema is looked for at, and where it was written,
/// so a failure to read it is reported there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaSource {
    pub location: String,
    pub written_at: Position,
}

/// A document's modeline: the place of the path or URL it names, that path or URL,
/// and the fragment after `#`.
struct Modeline {
    location_position: (usize, usize),
    location: String,
    fragment: Option<String>,
}

/// Where a document's schema is read from, the first present in the lookup's order.
/// `None` as the place a failure is reported means the run named it.
enum SchemaChoice {
    /// An S3 or HTTP schema, read from its source.
    Remote {
        location: String,
        written_at: Option<Position>,
        origin: SchemaOrigin,
    },
    /// A file on disk.
    Local {
        path: PathBuf,
        written_at: Option<Position>,
        origin: SchemaOrigin,
    },
    /// The file the run names, which is not on disk.
    RunFileMissing(String),
    /// Nothing the modeline or the list names is on disk.
    NotFound,
}

/// What named the schema a lookup found.
#[derive(Clone, Copy)]
enum SchemaOrigin {
    /// `--schema` or `TFGEN_SCHEMA`.
    Run,
    /// A validation fallback.
    Fallback,
    Modeline,
    /// The list's entry at this index.
    ListEntry(usize),
}

impl SchemaProvider for StackSchemas<'_> {
    fn schema_of(&self, unit: &Unit, document_text: &str) -> Result<DocumentSchema, Diagnostic> {
        let file: Arc<str> = Arc::from(unit.file.to_string_lossy().as_ref());
        let modeline = modeline_of(document_text);
        let fragment = modeline
            .as_ref()
            .and_then(|modeline| modeline.fragment.clone());
        let read = match self.choice(unit, &file, modeline.as_ref()) {
            SchemaChoice::Remote {
                location,
                written_at,
                ..
            } => self.read_url(&location, written_at.as_ref()),
            SchemaChoice::Local {
                path, written_at, ..
            } => read_file(&path, written_at.as_ref()),
            SchemaChoice::RunFileMissing(run_schema) => Err(Diagnostic::unplaced(format!(
                "{run_schema}, the schema `--schema` or `TFGEN_SCHEMA` names, is not on disk"
            ))),
            SchemaChoice::NotFound => Err(Diagnostic::at(
                &Position::new(&file, 1, 1),
                "no schema found: neither the modeline nor the search paths name a file on disk",
            )),
        };
        read.map(|(name, text, warnings)| DocumentSchema {
            name,
            text,
            fragment,
            warnings,
        })
    }
}

/// What a validation fallback reads when the schema the lookup found rejects the
/// document, and whether the document's modeline named that schema.
pub struct ValidationFallback<'s> {
    pub remote_sources: Vec<&'s SchemaSource>,
    pub modeline_named_found_schema: bool,
}

impl<'s> StackSchemas<'s> {
    /// The remote schemas (S3 and HTTP) a validation fallback reads, in the list's
    /// order, when the schema the lookup found rejects the document: every remote
    /// entry when the modeline names a file on disk, the remote entries after it
    /// when the list found it, and none when the run or a modeline's remote schema
    /// named it, which is the schema's own source.
    pub fn validation_fallback(&self, unit: &Unit) -> ValidationFallback<'s> {
        let found_choice = self.found_choice(unit);
        let modeline_named_found_schema = matches!(
            found_choice,
            Some(
                SchemaChoice::Local {
                    origin: SchemaOrigin::Modeline,
                    ..
                } | SchemaChoice::Remote {
                    origin: SchemaOrigin::Modeline,
                    ..
                }
            )
        );
        let after = match found_choice {
            Some(SchemaChoice::Local {
                origin: SchemaOrigin::Modeline,
                ..
            }) => Some(0),
            Some(
                SchemaChoice::Local {
                    origin: SchemaOrigin::ListEntry(index),
                    ..
                }
                | SchemaChoice::Remote {
                    origin: SchemaOrigin::ListEntry(index),
                    ..
                },
            ) => Some(index + 1),
            _ => None,
        };
        let remote_sources = match after {
            Some(after) => self
                .search_paths
                .iter()
                .skip(after)
                .filter(|source| is_schema_url(&source.location))
                .collect(),
            None => Vec::new(),
        };
        ValidationFallback {
            remote_sources,
            modeline_named_found_schema,
        }
    }

    /// The schema the lookup finds for the document on disk.
    fn found_choice(&self, unit: &Unit) -> Option<SchemaChoice> {
        let text = if unit.holds_no_documents {
            String::new()
        } else {
            fs::read_to_string(&unit.file).ok()?
        };
        let file: Arc<str> = Arc::from(unit.file.to_string_lossy().as_ref());
        Some(self.choice(unit, &file, modeline_of(&text).as_ref()))
    }

    /// The one place the schema is read from: a fallback's remote schema, else the
    /// schema the run names, else the modeline's, else the list's first present.
    fn choice(&self, unit: &Unit, file: &Arc<str>, modeline: Option<&Modeline>) -> SchemaChoice {
        if let Some(source) = self.fallback_source {
            return SchemaChoice::Remote {
                location: source.location.clone(),
                written_at: Some(source.written_at.clone()),
                origin: SchemaOrigin::Fallback,
            };
        }
        if let Some(run_schema) = self.run_schema {
            if is_schema_url(run_schema) {
                return SchemaChoice::Remote {
                    location: run_schema.to_owned(),
                    written_at: None,
                    origin: SchemaOrigin::Run,
                };
            }
            let path = Path::new(run_schema);
            return if path.is_file() {
                SchemaChoice::Local {
                    path: path.to_path_buf(),
                    written_at: None,
                    origin: SchemaOrigin::Run,
                }
            } else {
                SchemaChoice::RunFileMissing(run_schema.to_owned())
            };
        }
        if let Some(modeline) = modeline {
            let (line, column) = modeline.location_position;
            let written_at = Some(Position::new(file, line, column));
            if is_schema_url(&modeline.location) {
                return SchemaChoice::Remote {
                    location: modeline.location.clone(),
                    written_at,
                    origin: SchemaOrigin::Modeline,
                };
            }
            let path = unit.directory.join(&modeline.location);
            if path.is_file() {
                return SchemaChoice::Local {
                    path,
                    written_at,
                    origin: SchemaOrigin::Modeline,
                };
            }
        }
        for (index, source) in self.search_paths.iter().enumerate() {
            let entry = &source.location;
            let written_at = Some(source.written_at.clone());
            let origin = SchemaOrigin::ListEntry(index);
            if is_schema_url(entry) {
                return SchemaChoice::Remote {
                    location: entry.clone(),
                    written_at,
                    origin,
                };
            }
            let (base, pattern) = match entry.strip_prefix(CONFIG_ROOT) {
                Some(rest) => (self.config_root, rest.trim_start_matches('/')),
                None => (unit.directory.as_path(), entry.as_str()),
            };
            if let Some(path) = matching_files(base, pattern).into_iter().next() {
                return SchemaChoice::Local {
                    path,
                    written_at,
                    origin,
                };
            }
        }
        SchemaChoice::NotFound
    }

    /// A URL's schema, a failure reported where the URL was written, or as the
    /// run's when the run named it.
    fn read_url(
        &self,
        url: &str,
        written_at: Option<&Position>,
    ) -> Result<(String, String, Vec<String>), Diagnostic> {
        let read = self
            .schema_cache
            .read(url)
            .map_err(|failure| read_failure(failure, written_at))?;
        Ok((url.to_owned(), read.text, read.warnings))
    }
}

fn read_failure(failure: ReadFailure, written_at: Option<&Position>) -> Diagnostic {
    let ReadFailure {
        message,
        reason,
        help,
    } = failure;
    match (written_at, reason) {
        (Some(position), Some(reason)) => Diagnostic::at(position, message).with_label(reason),
        (Some(position), None) => Diagnostic::at(position, message),
        (None, Some(reason)) => Diagnostic::unplaced(format!(
            "{message}, the schema `--schema` or `TFGEN_SCHEMA` names: {reason}"
        )),
        (None, None) => Diagnostic::unplaced(format!(
            "{message}, the schema `--schema` or `TFGEN_SCHEMA` names"
        )),
    }
    .with_help(help)
}

fn read_file(
    path: &Path,
    written_at: Option<&Position>,
) -> Result<(String, String, Vec<String>), Diagnostic> {
    let text = fs::read_to_string(path).map_err(|error| {
        let message = format!("reading schema {} failed", path.display());
        match written_at {
            Some(position) => Diagnostic::at(position, message).with_label(error.to_string()),
            None => Diagnostic::unplaced(format!("{message}: {error}")),
        }
    })?;
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    Ok((name, text, Vec::new()))
}

fn modeline_of(text: &str) -> Option<Modeline> {
    text.lines().enumerate().find_map(|(index, line)| {
        let comment = line.trim_start().strip_prefix('#')?;
        let uri = comment.trim_start().strip_prefix(MODELINE_MARKER)?.trim();
        let column = line.find(MODELINE_MARKER)? + MODELINE_MARKER.len() + 1;
        let (location, fragment) = match uri.split_once('#') {
            Some((location, fragment)) => (location, Some(fragment.to_owned())),
            None => (uri, None),
        };
        let location = if is_schema_url(location) {
            location.to_owned()
        } else {
            location.split('?').next().unwrap_or(location).to_owned()
        };
        Some(Modeline {
            location_position: (index + 1, column),
            location,
            fragment,
        })
    })
}

/// The files on disk a search path names under `base`, in order: the one file a
/// plain path names, or every file a pattern's `*` steps match, each step read
/// from the folders the steps before it matched.
fn matching_files(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let is_pattern = |step: &str| step.contains('*');
    if !is_pattern(pattern) {
        let path = base.join(pattern);
        return if path.is_file() {
            vec![path]
        } else {
            Vec::new()
        };
    }
    let mut reached = vec![base.to_path_buf()];
    for step in pattern.split('/').filter(|step| !step.is_empty()) {
        if !is_pattern(step) {
            reached = reached.into_iter().map(|path| path.join(step)).collect();
            continue;
        }
        let Ok(glob) = Glob::new(step) else {
            return Vec::new();
        };
        let matcher = glob.compile_matcher();
        let mut matched: Vec<PathBuf> = reached
            .iter()
            .filter_map(|folder| fs::read_dir(folder).ok())
            .flatten()
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.file_name().is_some_and(|name| matcher.is_match(name)))
            .collect();
        matched.sort();
        reached = matched;
    }
    reached.retain(|path| path.is_file());
    reached
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScratchFolder(PathBuf);

    impl ScratchFolder {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("tfgen-lookup-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn write(&self, relative: &str, text: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
    }

    impl Drop for ScratchFolder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn unit_in(folder: &Path) -> Unit {
        Unit {
            id: "stack/mock-dsl.yml".into(),
            file: folder.join("stack/mock-dsl.yml"),
            directory: folder.join("stack"),
            overlay: None,
            common_layer: None,
            holds_no_documents: false,
        }
    }

    fn sources(locations: &[&str]) -> Vec<SchemaSource> {
        let file: Arc<str> = Arc::from("mock-tfgen.yml");
        locations
            .iter()
            .enumerate()
            .map(|(index, location)| SchemaSource {
                location: (*location).to_owned(),
                written_at: Position::new(&file, index + 1, 5),
            })
            .collect()
    }

    const MOCK_CAPACITY: usize = 8;

    fn schemas<'s>(
        root: &'s Path,
        run_schema: Option<&'s str>,
        search_paths: &'s [SchemaSource],
        schema_cache: &'s SchemaCache,
    ) -> StackSchemas<'s> {
        StackSchemas {
            config_root: root,
            run_schema,
            search_paths,
            schema_cache,
            fallback_source: None,
        }
    }

    #[test]
    fn a_search_path_pattern_takes_the_first_file_it_matches() {
        let scratch = ScratchFolder::new("pattern");
        scratch.write(
            "stack/.terraform/modules/queue_b/mock.schema.json",
            r#"{"title": "b"}"#,
        );
        scratch.write(
            "stack/.terraform/modules/key_a/mock.schema.json",
            r#"{"title": "a"}"#,
        );
        let copies = SchemaCache::new(scratch.0.join("copies"), MOCK_CAPACITY);
        let search_paths = sources(&[".terraform/modules/*/mock.schema.json"]);
        let found = schemas(&scratch.0, None, &search_paths, &copies)
            .schema_of(&unit_in(&scratch.0), "queue: {}\n")
            .unwrap();
        assert_eq!(found.text, r#"{"title": "a"}"#);
    }

    #[test]
    fn the_run_schema_goes_ahead_of_the_modeline_which_goes_ahead_of_the_search_paths() {
        let scratch = ScratchFolder::new("order");
        scratch.write("stack/local.json", r#"{"title": "modeline"}"#);
        scratch.write("search.json", r#"{"title": "search"}"#);
        scratch.write("work/changed.json", r#"{"title": "run"}"#);
        let copies = SchemaCache::new(scratch.0.join("copies"), MOCK_CAPACITY);
        let search_paths = sources(&["{config_root}/search.json"]);
        let unit = unit_in(&scratch.0);
        let text = "# yaml-language-server: $schema=local.json#/properties/queue\nqueue: {}\n";
        let modeline_read = schemas(&scratch.0, None, &search_paths, &copies)
            .schema_of(&unit, text)
            .unwrap();
        assert_eq!(modeline_read.text, r#"{"title": "modeline"}"#);
        assert_eq!(modeline_read.fragment.as_deref(), Some("/properties/queue"));
        let run_path = scratch.0.join("work/changed.json");
        let run_read = schemas(
            &scratch.0,
            Some(run_path.to_str().unwrap()),
            &search_paths,
            &copies,
        )
        .schema_of(&unit, text)
        .unwrap();
        assert_eq!(run_read.text, r#"{"title": "run"}"#);
        assert_eq!(run_read.fragment.as_deref(), Some("/properties/queue"));
        let searched = schemas(&scratch.0, None, &search_paths, &copies)
            .schema_of(&unit, "queue: {}\n")
            .unwrap();
        assert_eq!(searched.text, r#"{"title": "search"}"#);
        let missing = schemas(&scratch.0, Some("/no/such.json"), &search_paths, &copies)
            .schema_of(&unit, text)
            .unwrap_err();
        assert!(missing.message.contains("is not on disk"));
        assert!(missing.position.is_none());
    }

    #[test]
    fn a_validation_fallback_reads_the_remote_schemas_after_the_schema_found_in_order() {
        let scratch = ScratchFolder::new("fallback-sources");
        scratch.write("stack/local.json", r#"{"title": "local"}"#);
        let copies = SchemaCache::new(scratch.0.join("copies"), MOCK_CAPACITY);
        let (first, second) = (
            "http://127.0.0.1:9/first.schema.json",
            "s3://mock-schemas/second.schema.json",
        );
        let local_first = sources(&["local.json", first, "local.json", second]);
        let remote_first = sources(&[first, "local.json", second]);
        let unit = unit_in(&scratch.0);
        let fallbacks = |text: &str, run_schema: Option<&str>, search_paths: &[SchemaSource]| {
            scratch.write("stack/mock-dsl.yml", text);
            let fallback =
                schemas(&scratch.0, run_schema, search_paths, &copies).validation_fallback(&unit);
            let locations = fallback
                .remote_sources
                .iter()
                .map(|source| source.location.clone())
                .collect::<Vec<_>>();
            (locations, fallback.modeline_named_found_schema)
        };
        let named = "# yaml-language-server: $schema=local.json\nqueue: {}\n";
        let unnamed = "queue: {}\n";
        let named_remote = format!("# yaml-language-server: $schema={first}\nqueue: {{}}\n");
        assert_eq!(
            fallbacks(named, None, &remote_first),
            (vec![first.to_owned(), second.to_owned()], true)
        );
        assert_eq!(
            fallbacks(unnamed, None, &local_first),
            (vec![first.to_owned(), second.to_owned()], false)
        );
        assert_eq!(
            fallbacks(unnamed, None, &remote_first),
            (vec![second.to_owned()], false)
        );
        assert_eq!(
            fallbacks(named, Some("/work/changed.json"), &local_first),
            (Vec::new(), false)
        );
        assert_eq!(
            fallbacks(&named_remote, None, &local_first),
            (Vec::new(), true)
        );
    }

    #[test]
    fn a_url_not_read_is_reported_where_it_was_written() {
        let scratch = ScratchFolder::new("unread");
        let copies = SchemaCache::new(scratch.0.join("copies"), MOCK_CAPACITY);
        let search_paths = sources(&["absent.json", "http://127.0.0.1:9/mock.schema.json"]);
        let unit = unit_in(&scratch.0);
        let searched = schemas(&scratch.0, None, &search_paths, &copies)
            .schema_of(&unit, "queue: {}\n")
            .unwrap_err();
        let position = searched.position.unwrap();
        assert_eq!(
            (position.file.as_ref(), position.line, position.column),
            ("mock-tfgen.yml", 2, 5)
        );
        let text = "# yaml-language-server: $schema=http://127.0.0.1:9/mock.schema.json\n";
        let named = schemas(&scratch.0, None, &search_paths, &copies)
            .schema_of(&unit, text)
            .unwrap_err();
        let position = named.position.unwrap();
        assert_eq!((position.line, position.column), (1, 33));
    }
}
