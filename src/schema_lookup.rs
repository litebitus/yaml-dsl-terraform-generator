//! A document's schema, found through the YAML DSL standard's schema cache: the
//! schema named for the run first, else the document's modeline and then the
//! generator's `schema` list, or the DSL's `schema_search_paths`; and, for a
//! validation fallback, one remote schema of the list.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use yaml_dsl_compiler::{DocumentSchema, SchemaProvider, Unit};
use yaml_dsl_spec::schema::SchemaCache;
use yaml_dsl_spec::schema::resolution::{
    CandidateNamer, SchemaCandidate, SchemaFinding, SchemaSearchPath, document_modeline,
};
use yaml_dsl_spec::{Diagnostic, Position, Severity};

const FILE_SCHEME: &str = "file";
const RUN_SCHEMA_NAMED: &str = "the schema `--schema` or `TFGEN_SCHEMA` names";

/// How one stack's documents of one DSL find their schema.
pub struct StackSchemas<'s> {
    pub config_root: &'s Path,
    /// The schema named for the run, ahead of the modeline: a path or a URL.
    pub run_schema: Option<&'s str>,
    /// The generator's `schema` list, its refs filled, else the DSL's search paths.
    pub search_paths: &'s [SchemaSource],
    pub schema_cache: &'s SchemaCache,
    /// The one remote schema of the list a validation fallback reads, after the
    /// schema the lookup found rejected the document.
    pub fallback_source: Option<&'s SchemaSource>,
    /// The candidate the last lookup of the document's modeline and the list found.
    pub found_candidate: Mutex<Option<SchemaCandidate>>,
}

/// A path, path pattern or URL a schema is looked for at, and where it was written,
/// so a failure to read it is reported there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaSource {
    pub location: String,
    pub written_at: Position,
}

/// What a validation fallback reads when the schema the lookup found rejects the
/// document, and whether the document's modeline named that schema.
pub struct ValidationFallback<'s> {
    pub remote_sources: Vec<&'s SchemaSource>,
    pub modeline_named_found_schema: bool,
}

impl SchemaProvider for StackSchemas<'_> {
    fn schema_of(&self, unit: &Unit, document_text: &str) -> Result<DocumentSchema, Diagnostic> {
        let modeline_fragment =
            document_modeline(document_text).and_then(|modeline| modeline.schema_fragment);
        if let Some(source) = self.fallback_source {
            let finding = self
                .schema_cache
                .read_schema(&source.location, Some(&source.written_at));
            return document_schema_of(finding, Some(modeline_fragment));
        }
        if let Some(run_schema) = self.run_schema {
            let finding = self.schema_cache.read_schema(run_schema, None);
            return document_schema_of(finding, Some(modeline_fragment))
                .map_err(run_schema_problem);
        }
        let search_paths: Vec<SchemaSearchPath> = self
            .search_paths
            .iter()
            .map(|source| SchemaSearchPath {
                path_text: source.location.clone(),
                written_at: Some(source.written_at.clone()),
            })
            .collect();
        let finding = self.schema_cache.document_schema(
            &unit.file,
            document_text,
            &search_paths,
            self.config_root,
        );
        *self
            .found_candidate
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = finding
            .found_schema
            .as_ref()
            .map(|found| found.found_candidate.clone());
        document_schema_of(finding, None)
    }
}

impl<'s> StackSchemas<'s> {
    /// The lookup of a stack's documents: the run's schema, else each document's
    /// modeline and `search_paths`.
    pub fn new(
        config_root: &'s Path,
        run_schema: Option<&'s str>,
        search_paths: &'s [SchemaSource],
        schema_cache: &'s SchemaCache,
    ) -> Self {
        Self {
            config_root,
            run_schema,
            search_paths,
            schema_cache,
            fallback_source: None,
            found_candidate: Mutex::new(None),
        }
    }

    /// The same lookup reading only `source`, a remote schema of the list.
    pub fn reading_fallback(&self, source: &'s SchemaSource) -> Self {
        Self {
            fallback_source: Some(source),
            ..Self::new(
                self.config_root,
                self.run_schema,
                self.search_paths,
                self.schema_cache,
            )
        }
    }

    /// The remote schemas (any URL but a `file` one) a validation fallback reads,
    /// in the list's order, when the schema the last lookup found rejects the
    /// document: every remote entry when the modeline names a file, the remote
    /// entries after it when the list found it, and none when the run or a
    /// modeline's remote schema named it, which is the schema's own source, or
    /// nothing was found.
    pub fn validation_fallback(&self) -> ValidationFallback<'s> {
        let found_candidate = self
            .found_candidate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let modeline_named_found_schema = found_candidate
            .as_ref()
            .is_some_and(|found| matches!(found.named_by, CandidateNamer::Modeline { .. }));
        let after = found_candidate.and_then(|found| match found.named_by {
            CandidateNamer::SearchPath { index } => Some(index + 1),
            CandidateNamer::Modeline { .. } if !is_remote_location(&found.schema_location) => {
                Some(0)
            }
            _ => None,
        });
        let remote_sources = match after {
            Some(after) => self
                .search_paths
                .iter()
                .skip(after)
                .filter(|source| is_remote_location(&source.location))
                .collect(),
            None => Vec::new(),
        };
        ValidationFallback {
            remote_sources,
            modeline_named_found_schema,
        }
    }
}

/// The schema a finding found, its warnings beside it, and the fragment the
/// document's modeline names where the caller gives one; else the finding's error.
fn document_schema_of(
    finding: SchemaFinding,
    modeline_fragment: Option<Option<String>>,
) -> Result<DocumentSchema, Diagnostic> {
    let SchemaFinding {
        found_schema,
        finding_problems,
        ..
    } = finding;
    let Some(found) = found_schema else {
        return Err(finding_problems
            .into_iter()
            .find(|problem| problem.severity == Severity::Error)
            .unwrap_or_else(|| Diagnostic::unplaced("no schema found")));
    };
    Ok(DocumentSchema {
        name: found.schema_name,
        text: found.schema_text,
        fragment: modeline_fragment.unwrap_or(found.schema_fragment),
        warnings: finding_problems,
    })
}

/// A problem of the schema the run names, which has no place: the run named in
/// its message, beside what the label said.
fn run_schema_problem(problem: Diagnostic) -> Diagnostic {
    if problem.position.is_some() {
        return problem;
    }
    let label = problem.notes.as_ref().and_then(|notes| notes.label.clone());
    let message = match label {
        Some(label) => format!("{}, {RUN_SCHEMA_NAMED}: {label}", problem.message),
        None => format!("{}, {RUN_SCHEMA_NAMED}", problem.message),
    };
    let help = problem.notes.as_ref().and_then(|notes| notes.help.clone());
    Diagnostic::unplaced(message).with_help(help)
}

/// Whether a location is a URL read from somewhere other than the machine.
fn is_remote_location(location: &str) -> bool {
    location.split_once("://").is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && !scheme.eq_ignore_ascii_case(FILE_SCHEME)
            && scheme
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "+-.".contains(character))
    })
}

/// Where the schema cache keeps its entries: the user's cache folder, across runs.
pub fn schema_cache_directory() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let cache = match std::env::var_os("XDG_CACHE_HOME") {
        Some(cache) => PathBuf::from(cache),
        None if cfg!(target_os = "macos") => home.join("Library/Caches"),
        None => home.join(".cache"),
    };
    cache.join("tfgen/schema-cache")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use super::*;

    struct ScratchFolder(PathBuf);

    impl ScratchFolder {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("tfgen-lookup-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(fs::canonicalize(path).unwrap())
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

    #[test]
    fn the_run_schema_goes_ahead_of_the_modeline_which_goes_ahead_of_the_search_paths() {
        let scratch = ScratchFolder::new("order");
        scratch.write("stack/local.json", r#"{"title": "modeline"}"#);
        scratch.write("search.json", r#"{"title": "search"}"#);
        scratch.write("work/changed.json", r#"{"title": "run"}"#);
        let cache = SchemaCache::default();
        let search_paths = sources(&["{config_root}/search.json"]);
        let unit = unit_in(&scratch.0);
        let text = "# yaml-language-server: $schema=local.json#/properties/queue\nqueue: {}\n";
        let modeline_read = StackSchemas::new(&scratch.0, None, &search_paths, &cache)
            .schema_of(&unit, text)
            .unwrap();
        assert_eq!(modeline_read.text, r#"{"title": "modeline"}"#);
        assert_eq!(modeline_read.fragment.as_deref(), Some("/properties/queue"));
        let run_path = scratch.0.join("work/changed.json");
        let run_location = run_path.to_str().unwrap();
        let run_read = StackSchemas::new(&scratch.0, Some(run_location), &search_paths, &cache)
            .schema_of(&unit, text)
            .unwrap();
        assert_eq!(run_read.text, r#"{"title": "run"}"#);
        assert_eq!(run_read.fragment.as_deref(), Some("/properties/queue"));
        let searched = StackSchemas::new(&scratch.0, None, &search_paths, &cache)
            .schema_of(&unit, "queue: {}\n")
            .unwrap();
        assert_eq!(searched.text, r#"{"title": "search"}"#);
        assert_eq!(searched.fragment, None);
        let missing = StackSchemas::new(&scratch.0, Some("/no/such.json"), &search_paths, &cache)
            .schema_of(&unit, text)
            .unwrap_err();
        assert_eq!(
            missing.message,
            format!("`/no/such.json` is missing, {RUN_SCHEMA_NAMED}: no file is there")
        );
        assert!(missing.position.is_none());
    }

    #[test]
    fn a_validation_fallback_reads_the_remote_schemas_after_the_schema_found_in_order() {
        let scratch = ScratchFolder::new("fallback-sources");
        scratch.write("stack/local.json", r#"{"title": "local"}"#);
        let cache = SchemaCache::default();
        let (first, second) = (
            "http://127.0.0.1:9/first.schema.json",
            "s3://mock-schemas/second.schema.json",
        );
        let local_first = sources(&["local.json", first, "local.json", second]);
        let unit = unit_in(&scratch.0);
        let fallbacks = |text: &str, search_paths: &[SchemaSource]| {
            let schemas = StackSchemas::new(&scratch.0, None, search_paths, &cache);
            let _ = schemas.schema_of(&unit, text);
            let fallback = schemas.validation_fallback();
            let locations = fallback
                .remote_sources
                .iter()
                .map(|source| source.location.clone())
                .collect::<Vec<_>>();
            (locations, fallback.modeline_named_found_schema)
        };
        let named = "# yaml-language-server: $schema=local.json\nqueue: {}\n";
        let unnamed = "queue: {}\n";
        assert_eq!(
            fallbacks(named, &local_first),
            (vec![first.to_owned(), second.to_owned()], true)
        );
        assert_eq!(
            fallbacks(unnamed, &local_first),
            (vec![first.to_owned(), second.to_owned()], false)
        );
        let run_schemas =
            StackSchemas::new(&scratch.0, Some("/work/changed.json"), &local_first, &cache);
        let _ = run_schemas.schema_of(&unit, named);
        assert!(run_schemas.validation_fallback().remote_sources.is_empty());
    }

    #[test]
    fn a_location_is_remote_when_it_is_a_url_other_than_a_file_one() {
        assert!(is_remote_location("s3://mock-bucket/a.json"));
        assert!(is_remote_location("HTTPS://mock.example/a.json"));
        assert!(!is_remote_location("file:///mock/a.json"));
        assert!(!is_remote_location("schemas/a.json"));
        assert!(!is_remote_location("mock/https://a"));
    }
}
