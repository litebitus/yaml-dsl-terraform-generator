//! A document's schema, found as the editor finds a file's: the schema named for
//! the run first, then its modeline, then the generator's `schema` list, else the
//! DSL's `schema_search_paths`.

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

impl SchemaProvider for StackSchemas<'_> {
    fn schema_of(&self, unit: &Unit, document_text: &str) -> Result<DocumentSchema, Diagnostic> {
        let file: Arc<str> = Arc::from(unit.file.to_string_lossy().as_ref());
        let modeline = modeline_of(document_text);
        let fragment = modeline
            .as_ref()
            .and_then(|modeline| modeline.fragment.clone());
        let found = |schema: Result<(String, String, Vec<String>), Diagnostic>| {
            schema.map(|(name, text, warnings)| DocumentSchema {
                name,
                text,
                fragment: fragment.clone(),
                warnings,
            })
        };
        if let Some(run_schema) = self.run_schema {
            if is_schema_url(run_schema) {
                return found(self.read_url(run_schema, None));
            }
            return found(read_file(Path::new(run_schema), None).map_err(|_| {
                Diagnostic::unplaced(format!(
                    "{run_schema}, the schema `--schema` or `TFGEN_SCHEMA` names, is not on disk"
                ))
            }));
        }
        if let Some(modeline) = &modeline {
            let (line, column) = modeline.location_position;
            let written_at = Position::new(&file, line, column);
            if is_schema_url(&modeline.location) {
                return found(self.read_url(&modeline.location, Some(&written_at)));
            }
            let path = unit.directory.join(&modeline.location);
            if path.is_file() {
                return found(read_file(&path, Some(&written_at)));
            }
        }
        for source in self.search_paths {
            let entry = &source.location;
            if is_schema_url(entry) {
                return found(self.read_url(entry, Some(&source.written_at)));
            }
            let (base, pattern) = match entry.strip_prefix(CONFIG_ROOT) {
                Some(rest) => (self.config_root, rest.trim_start_matches('/')),
                None => (unit.directory.as_path(), entry.as_str()),
            };
            if let Some(path) = matching_files(base, pattern).into_iter().next() {
                return found(read_file(&path, Some(&source.written_at)));
            }
        }
        Err(Diagnostic::at(
            &Position::new(&file, 1, 1),
            "no schema found: neither the modeline nor the search paths name a file on disk",
        ))
    }
}

impl StackSchemas<'_> {
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
