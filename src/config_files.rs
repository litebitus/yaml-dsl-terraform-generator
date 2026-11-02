//! A config folder's `yaml-dsl.yml` and `tfgen.yml`: the DSLs it declares, and
//! what each generated DSL compiles to.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fancy_regex::Regex;
use yaml_dsl_compiler::{Compiler, FileSource};
use yaml_dsl_spec::json_path::PathPattern;
use yaml_dsl_spec::yaml_reader::parse;
use yaml_dsl_spec::{
    Diagnostic, DslDefinition, Entry, Node, NodeKind, Position, read_dsl_definitions,
};

use crate::disk_source::DiskSource;
use crate::error_report::did_you_mean;
use crate::ref_scope::FixedRoot;
use crate::schema_lookup::SchemaSource;
use crate::template_filling::check_fixed_roots;

pub const DSL_CONFIG_FILE: &str = "yaml-dsl.yml";
pub const TFGEN_CONFIG_FILE: &str = "tfgen.yml";
/// The `tfgen.yml` formats tfgen reads, by their `version`.
const TFGEN_CONFIG_VERSIONS: &[&str] = &["1", "1.0", "1.1"];
/// The `tfgen.yml` format that has no `performance`.
const UNTUNED_CONFIG_VERSION: &str = "1.0";
const DEFAULT_DESCRIPTOR: &str = "stack.tg.hcl";
pub const DEFAULT_SCHEMA_CACHE_CAPACITY: usize = 100;

/// A config folder: every DSL its `yaml-dsl.yml` declares, and its `tfgen.yml`.
pub struct LoadedConfig {
    pub directory: PathBuf,
    pub dsls: Vec<DeclaredDsl>,
    pub tfgen: Option<TfgenConfig>,
}

/// A DSL of `yaml-dsl.yml`: what selects its files, and its compiler where a
/// generator compiles it.
pub struct DeclaredDsl {
    pub id: String,
    pub selection: Compiler,
    pub generated: Option<Compiler>,
    /// The DSL's `schema_search_paths`, as the editor reads them.
    pub search_paths: Vec<SchemaSource>,
}

pub struct TfgenConfig {
    pub file: PathBuf,
    pub descriptor_names: Vec<String>,
    pub globals: Vec<(String, Node)>,
    pub generators: Vec<Generator>,
    pub stack_templates: Vec<(String, StackTemplate)>,
    /// How many schemas the schema cache keeps, the least recently used dropped first.
    pub schema_cache_capacity: usize,
}

pub struct Generator {
    pub dsl_id: String,
    pub position: Position,
    pub written_to: String,
    /// Where the schema is, when a document's modeline names none on disk: a list
    /// of paths, path patterns and URLs, each with refs, the first found taken.
    pub schema: Option<Node>,
    pub builtin_values: Vec<(String, Node)>,
    pub data_files: Vec<PathPattern>,
    pub values: Vec<(String, ValueRule)>,
    pub blocks: Vec<BlockRule>,
    pub references: Vec<ReferenceRule>,
}

pub enum ValueRule {
    Single {
        value: Node,
        default: Option<Node>,
    },
    Each {
        each: PathPattern,
        where_fields: Vec<(String, Node)>,
        defaults: Option<Node>,
        value: Node,
    },
}

pub struct BlockRule {
    pub template: TemplateText,
    pub selection: BlockSelection,
    pub scope_keys: Option<Vec<String>>,
    pub scope_keys_except: Vec<String>,
    pub value_matches: Option<Regex>,
}

pub enum BlockSelection {
    Declaration { scope: String },
    ScopeKey { scope: String },
    Entry { path: PathPattern },
    Once,
}

pub struct ReferenceRule {
    pub scope: String,
    pub scope_keys: Option<Vec<String>>,
    pub path_matches: Option<Regex>,
    pub template: TemplateText,
}

pub struct StackTemplate {
    pub template: TemplateText,
    pub written_to: String,
    pub required_providers: Vec<(String, String)>,
}

/// A template's text and where it was written, so an error names its place.
#[derive(Clone)]
pub struct TemplateText {
    pub text: String,
    pub start: Position,
}

impl LoadedConfig {
    /// The config of `directory`, or every problem its files hold.
    pub fn load(directory: &Path, source: &Arc<DiskSource>) -> Result<Self, Vec<Diagnostic>> {
        let dsl_file = directory.join(DSL_CONFIG_FILE);
        let dsl_text = fs::read_to_string(&dsl_file).map_err(|error| {
            vec![Diagnostic::unplaced(format!(
                "reading {}: {error}",
                dsl_file.display()
            ))]
        })?;
        let declared = read_dsl_definitions(&dsl_file, &dsl_text)?;
        let tfgen_file = directory.join(TFGEN_CONFIG_FILE);
        let tfgen = if tfgen_file.exists() {
            Some(TfgenConfig::read(&tfgen_file, directory).map_err(|problem| vec![problem])?)
        } else {
            None
        };
        let generated_ids: Vec<&str> = tfgen
            .iter()
            .flat_map(|tfgen| &tfgen.generators)
            .map(|generator| generator.dsl_id.as_str())
            .collect();
        let mut dsls = Vec::with_capacity(declared.len());
        for read in declared {
            let block = &read.block;
            let id = read.definition.id.clone();
            let selection_definition = DslDefinition::from_json(&serde_json::json!({
                "id": id,
                "file_includes": read.definition.file_includes,
                "file_excludes": read.definition.file_excludes,
            }))
            .map_err(|message| vec![Diagnostic::at(&block.position, message)])?;
            let compiler = |definition: DslDefinition| {
                let source: Arc<dyn FileSource> = source.clone();
                Compiler::new(definition, directory, source)
                    .map_err(|error| vec![located(error, &block.position, &id)])
            };
            let selection = compiler(selection_definition)?;
            let generated = if generated_ids.contains(&id.as_str()) {
                Some(compiler(read.definition.clone())?)
            } else {
                None
            };
            let search_paths = block
                .get("schema_search_paths")
                .and_then(Node::items)
                .unwrap_or_default()
                .iter()
                .filter_map(|entry| {
                    entry.as_str().map(|location| SchemaSource {
                        location: location.to_owned(),
                        written_at: entry.position.clone(),
                    })
                })
                .collect();
            dsls.push(DeclaredDsl {
                id,
                selection,
                generated,
                search_paths,
            });
        }
        if let Some(tfgen) = &tfgen {
            for generator in &tfgen.generators {
                if !dsls.iter().any(|dsl| dsl.id == generator.dsl_id) {
                    let ids: Vec<String> = dsls.iter().map(|dsl| dsl.id.clone()).collect();
                    return Err(vec![
                        Diagnostic::at(
                            &generator.position,
                            format!("generator `{}` names no DSL", generator.dsl_id),
                        )
                        .with_label(format!(
                            "{DSL_CONFIG_FILE} declares no DSL `{}`",
                            generator.dsl_id
                        ))
                        .with_help(did_you_mean(&generator.dsl_id, &ids)),
                    ]);
                }
            }
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            dsls,
            tfgen,
        })
    }

    pub fn generator(&self, dsl_id: &str) -> Option<&Generator> {
        self.tfgen
            .as_ref()?
            .generators
            .iter()
            .find(|generator| generator.dsl_id == dsl_id)
    }

    pub fn schema_cache_capacity(&self) -> usize {
        self.tfgen
            .as_ref()
            .map_or(DEFAULT_SCHEMA_CACHE_CAPACITY, |tfgen| {
                tfgen.schema_cache_capacity
            })
    }

    pub fn descriptor_names(&self) -> Vec<String> {
        match &self.tfgen {
            Some(tfgen) => tfgen.descriptor_names.clone(),
            None => vec![DEFAULT_DESCRIPTOR.to_owned()],
        }
    }
}

/// Each config folder a run reads, by its directory: its config, or every
/// problem its files hold.
pub type LoadedConfigs = BTreeMap<PathBuf, Result<LoadedConfig, Vec<Diagnostic>>>;

/// Loads `directory`'s config once, however many stacks it configures.
pub fn load_config(configs: &mut LoadedConfigs, directory: &Path, source: &Arc<DiskSource>) {
    configs
        .entry(directory.to_path_buf())
        .or_insert_with(|| LoadedConfig::load(directory, source));
}

fn located(error: Diagnostic, position: &Position, id: &str) -> Diagnostic {
    match error.position {
        Some(_) => error,
        None => Diagnostic::at(position, format!("DSL `{id}`: {}", error.message)),
    }
}

/// A YAML file's tree, and its text.
fn read_yaml_text(file: &Path) -> Result<(Node, String), Diagnostic> {
    let text = fs::read_to_string(file)
        .map_err(|error| Diagnostic::unplaced(format!("reading {}: {error}", file.display())))?;
    let root = parse(&Arc::from(file.to_string_lossy().as_ref()), &text, &[])?;
    Ok((root, text))
}

impl TfgenConfig {
    fn read(file: &Path, directory: &Path) -> Result<Self, Diagnostic> {
        let (root, source) = read_yaml_text(file)?;
        let mut config = MapReader::new(&root, "tfgen.yml")?;
        let version = config_version(config.required("version")?)?;
        let performance = match version.as_str() {
            UNTUNED_CONFIG_VERSION => None,
            _ => config.optional("performance"),
        };
        let schema_cache_capacity = match performance {
            Some(performance) => {
                let mut performance = MapReader::new(performance, "`performance`")?;
                let capacity = match performance.optional("schema_cache_capacity") {
                    Some(capacity) => positive_whole_number(capacity, "`schema_cache_capacity`")?,
                    None => DEFAULT_SCHEMA_CACHE_CAPACITY,
                };
                performance.finish()?;
                capacity
            }
            None => DEFAULT_SCHEMA_CACHE_CAPACITY,
        };
        let descriptor_names = match config.optional("stacks") {
            Some(stacks) => {
                let mut stacks = MapReader::new(stacks, "`stacks`")?;
                let names = match stacks.optional("descriptors") {
                    Some(names) => texts(names, "`stacks.descriptors`")?,
                    None => vec![DEFAULT_DESCRIPTOR.to_owned()],
                };
                stacks.finish()?;
                names
            }
            None => vec![DEFAULT_DESCRIPTOR.to_owned()],
        };
        let globals = match config.optional("globals") {
            Some(globals) => named_rules(globals, "`globals`")?,
            None => Vec::new(),
        };
        let mut generators = Vec::new();
        if let Some(listed) = config.optional("generators") {
            for entry in entries(listed, "`generators`")? {
                generators.push(Generator::read(entry, directory, &source)?);
            }
        }
        let mut stack_templates = Vec::new();
        if let Some(listed) = config.optional("templates") {
            for entry in entries(listed, "`templates`")? {
                stack_templates.push((entry.key.clone(), StackTemplate::read(entry, directory)?));
            }
        }
        config.finish()?;
        Ok(Self {
            file: file.to_path_buf(),
            descriptor_names,
            globals,
            generators,
            stack_templates,
            schema_cache_capacity,
        })
    }
}

impl Generator {
    fn read(entry: &Entry, directory: &Path, source: &str) -> Result<Self, Diagnostic> {
        let what = format!("generator `{}`", entry.key);
        let mut generator = MapReader::new(&entry.value, &what)?;
        let written_to = text(generator.required("written_to")?, "`written_to`")?;
        let schema = generator.optional("schema").cloned();
        let builtin_values = match generator.optional("builtin_values") {
            Some(values) => entries(values, "`builtin_values`")?
                .iter()
                .map(|entry| (entry.key.clone(), entry.value.clone()))
                .collect(),
            None => Vec::new(),
        };
        let data_files = match generator.optional("data_files") {
            Some(paths) => items(paths, "`data_files`")?
                .iter()
                .map(json_path)
                .collect::<Result<_, _>>()?,
            None => Vec::new(),
        };
        let mut values = Vec::new();
        if let Some(listed) = generator.optional("values") {
            for entry in entries(listed, "`values`")? {
                values.push((entry.key.clone(), ValueRule::read(&entry.value)?));
            }
        }
        let mut blocks = Vec::new();
        if let Some(listed) = generator.optional("blocks") {
            for block in items(listed, "`blocks`")? {
                blocks.push(BlockRule::read(block, directory)?);
            }
        }
        let mut references = Vec::new();
        if let Some(listed) = generator.optional("references") {
            for reference in items(listed, "`references`")? {
                references.push(ReferenceRule::read(reference, source)?);
            }
        }
        generator.finish()?;
        Ok(Self {
            dsl_id: entry.key.clone(),
            position: entry.key_position.clone(),
            written_to,
            schema,
            builtin_values,
            data_files,
            values,
            blocks,
            references,
        })
    }
}

impl ValueRule {
    fn read(node: &Node) -> Result<Self, Diagnostic> {
        let mut rule = MapReader::new(node, "a value")?;
        let value_rule = match rule.optional("each") {
            Some(each) => {
                let where_fields = match rule.optional("where") {
                    Some(fields) => entries(fields, "`where`")?
                        .iter()
                        .map(|entry| (entry.key.clone(), entry.value.clone()))
                        .collect(),
                    None => Vec::new(),
                };
                ValueRule::Each {
                    each: json_path(each)?,
                    where_fields,
                    defaults: rule.optional("defaults").cloned(),
                    value: rule.required("value")?.clone(),
                }
            }
            None => ValueRule::Single {
                value: rule.required("value")?.clone(),
                default: rule.optional("default").cloned(),
            },
        };
        rule.finish()?;
        Ok(value_rule)
    }
}

impl BlockRule {
    fn read(node: &Node, directory: &Path) -> Result<Self, Diagnostic> {
        let mut block = MapReader::new(node, "a block")?;
        let template = template_file(block.required("template")?, directory)?;
        let scope = |block: &mut MapReader<'_>| -> Result<String, Diagnostic> {
            text(block.required("scope")?, "`scope`")
        };
        let each_root = match block.optional("each") {
            None => None,
            Some(each) => Some(block_each_root(each)?),
        };
        let selection = match each_root {
            None => BlockSelection::Once,
            Some(FixedRoot::DeclarationEach) => BlockSelection::Declaration {
                scope: scope(&mut block)?,
            },
            Some(FixedRoot::ScopeKeyEach) => BlockSelection::ScopeKey {
                scope: scope(&mut block)?,
            },
            Some(FixedRoot::EntryEach | FixedRoot::Reference) => BlockSelection::Entry {
                path: json_path(block.required("path")?)?,
            },
        };
        let fixed_roots: Vec<FixedRoot> = each_root.into_iter().collect();
        check_fixed_roots(&template, &fixed_roots, GENERATOR_ROOTS)?;
        let scope_keys = match block.optional("scope_keys") {
            Some(keys) => Some(texts(keys, "`scope_keys`")?),
            None => None,
        };
        let scope_keys_except = match block.optional("scope_keys_except") {
            Some(keys) => texts(keys, "`scope_keys_except`")?,
            None => Vec::new(),
        };
        let value_matches = match block.optional("value_matches") {
            Some(pattern) => Some(regex(pattern)?),
            None => None,
        };
        block.finish()?;
        Ok(Self {
            template,
            selection,
            scope_keys,
            scope_keys_except,
            value_matches,
        })
    }
}

impl ReferenceRule {
    fn read(node: &Node, source: &str) -> Result<Self, Diagnostic> {
        let mut rule = MapReader::new(node, "a reference")?;
        let scope = text(rule.required("scope")?, "`scope`")?;
        let scope_keys = match rule.optional("scope_keys") {
            Some(keys) => Some(texts(keys, "`scope_keys`")?),
            None => None,
        };
        let path_matches = match rule.optional("path_matches") {
            Some(pattern) => Some(regex(pattern)?),
            None => None,
        };
        let template_node = rule.required("template")?;
        let template = TemplateText {
            text: text(template_node, "`template`")?,
            start: text_start(template_node, source),
        };
        check_fixed_roots(&template, &[FixedRoot::Reference], GENERATOR_ROOTS)?;
        rule.finish()?;
        Ok(Self {
            scope,
            scope_keys,
            path_matches,
            template,
        })
    }
}

impl StackTemplate {
    fn read(entry: &Entry, directory: &Path) -> Result<Self, Diagnostic> {
        let what = format!("template `{}`", entry.key);
        let mut template = MapReader::new(&entry.value, &what)?;
        let file = template_file(template.required("file")?, directory)?;
        check_fixed_roots(&file, &[], STACK_ROOTS)?;
        let written_to = text(template.required("written_to")?, "`written_to`")?;
        let required_providers = match template.optional("required_providers") {
            Some(providers) => entries(providers, "`required_providers`")?
                .iter()
                .map(|entry| Ok((entry.key.clone(), text(&entry.value, "a provider source")?)))
                .collect::<Result<_, Diagnostic>>()?,
            None => Vec::new(),
        };
        template.finish()?;
        Ok(Self {
            template: file,
            written_to,
            required_providers,
        })
    }
}

/// `tfgen.yml`'s `version`, where it names a format tfgen reads.
fn config_version(node: &Node) -> Result<String, Diagnostic> {
    let version = text(node, "`version`")?;
    if TFGEN_CONFIG_VERSIONS.contains(&version.as_str()) {
        return Ok(version);
    }
    let versions: Vec<String> = TFGEN_CONFIG_VERSIONS
        .iter()
        .map(|version| (*version).to_owned())
        .collect();
    let listed = versions
        .iter()
        .map(|version| format!("`\"{version}\"`"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(
        Diagnostic::at(&node.position, format!("`version` must be one of {listed}"))
            .with_label(format!("`version` is one of {listed}")),
    )
}

/// The roots every template reads.
const STACK_ROOTS: &[&str] = &["stack", "tfgen"];

/// The roots a generator's templates read, beside `each` and `reference`.
const GENERATOR_ROOTS: &[&str] = &["stack", "tfgen", "fold", "values"];

/// The root a block's `each` names.
fn block_each_root(each: &Node) -> Result<FixedRoot, Diagnostic> {
    let kind = text(each, "`each`")?;
    let kinds: Vec<String> = FixedRoot::BLOCK_EACH_ROOTS
        .iter()
        .filter_map(|root| root.block_each_kind())
        .map(str::to_owned)
        .collect();
    FixedRoot::BLOCK_EACH_ROOTS
        .into_iter()
        .find(|root| root.block_each_kind() == Some(kind.as_str()))
        .ok_or_else(|| {
            let listed = kinds
                .iter()
                .map(|kind| format!("`{kind}`"))
                .collect::<Vec<_>>()
                .join(", ");
            let allowed = kinds
                .iter()
                .map(|kind| format!("`\"{kind}\"`"))
                .collect::<Vec<_>>()
                .join(", ");
            Diagnostic::at(&each.position, format!("`each` must be one of {allowed}"))
                .with_label(format!("`each` is one of {listed}"))
                .with_help(did_you_mean(&kind, &kinds))
        })
}

/// Where a scalar's text starts in its file: past its opening quote, where it is
/// quoted.
fn text_start(node: &Node, source: &str) -> Position {
    let position = &node.position;
    let quoted = source
        .lines()
        .nth(position.line.saturating_sub(1))
        .and_then(|line| line.chars().nth(position.column.saturating_sub(1)))
        .is_some_and(|character| matches!(character, '"' | '\''));
    if quoted {
        Position::new(&position.file, position.line, position.column + 1)
    } else {
        position.clone()
    }
}

/// What a node is, for a report of a node of the wrong kind.
fn kind_of(node: &Node) -> &'static str {
    match node.kind {
        NodeKind::Null => "null",
        NodeKind::Bool(_) => "a boolean",
        NodeKind::Number(_) => "a number",
        NodeKind::String(_) => "text",
        NodeKind::List(_) => "a list",
        NodeKind::Map(_) => "a map",
    }
}

fn template_file(node: &Node, directory: &Path) -> Result<TemplateText, Diagnostic> {
    let relative = text(node, "a template file")?;
    let file = directory.join(&relative);
    let text = fs::read_to_string(&file).map_err(|error| {
        Diagnostic::at(
            &node.position,
            format!("template file {relative} cannot be read"),
        )
        .with_label(error.to_string())
    })?;
    Ok(TemplateText {
        text,
        start: Position::file_start(&Arc::from(file.to_string_lossy().as_ref())),
    })
}

/// Each entry of a map, by name; a key that calls a function, such as
/// `name fn.format*`, names its entry by the name before the call.
fn named_rules(node: &Node, what: &str) -> Result<Vec<(String, Node)>, Diagnostic> {
    entries(node, what)?
        .iter()
        .map(|entry| match crate::yaml_values::call_key(&entry.key) {
            Some(call) => {
                let name = call.name.clone().ok_or_else(|| {
                    Diagnostic::at(
                        &entry.key_position,
                        format!("{what}: a call names its entry"),
                    )
                })?;
                let call_text =
                    entry.key[entry.key.len() - call_text_length(&entry.key)..].to_owned();
                let wrapped = Node::new(
                    NodeKind::Map(vec![Entry {
                        key: call_text,
                        key_position: entry.key_position.clone(),
                        value: entry.value.clone(),
                    }]),
                    entry.value.position.clone(),
                );
                Ok((name, wrapped))
            }
            None => Ok((entry.key.clone(), entry.value.clone())),
        })
        .collect()
}

fn call_text_length(key: &str) -> usize {
    key.trim_end().rsplit(' ').next().map_or(0, str::len)
}

struct MapReader<'n> {
    node: &'n Node,
    what: String,
    read: Vec<&'static str>,
}

impl<'n> MapReader<'n> {
    fn new(node: &'n Node, what: &str) -> Result<Self, Diagnostic> {
        if !node.is_map() {
            return Err(type_problem(node, what, "object"));
        }
        Ok(Self {
            node,
            what: what.to_owned(),
            read: Vec::new(),
        })
    }

    fn optional(&mut self, key: &'static str) -> Option<&'n Node> {
        self.read.push(key);
        self.node.get(key)
    }

    fn required(&mut self, key: &'static str) -> Result<&'n Node, Diagnostic> {
        let what = self.what.clone();
        self.optional(key).ok_or_else(|| {
            Diagnostic::at(&self.node.position, format!("`{key}` is required"))
                .with_label(format!("{what} has no field `{key}`"))
        })
    }

    fn finish(self) -> Result<(), Diagnostic> {
        for entry in self.node.entries().unwrap_or_default() {
            if !self.read.contains(&entry.key.as_str()) {
                let fields: Vec<String> = self.read.iter().map(|key| (*key).to_owned()).collect();
                return Err(Diagnostic::at(
                    &entry.key_position,
                    format!("`{}` is not a field of {}", entry.key, self.what),
                )
                .with_label(format!("{} has no field `{}`", self.what, entry.key))
                .with_help(did_you_mean(&entry.key, &fields)));
            }
        }
        Ok(())
    }
}

/// A node of the wrong kind: the JSON type it must be, and what it is.
fn type_problem(node: &Node, what: &str, expected_type: &str) -> Diagnostic {
    Diagnostic::at(&node.position, format!("{what} must be {expected_type}"))
        .with_label(format!("is {}", kind_of(node)))
}

fn entries<'n>(node: &'n Node, what: &str) -> Result<&'n [Entry], Diagnostic> {
    node.entries()
        .ok_or_else(|| type_problem(node, what, "object"))
}

fn items<'n>(node: &'n Node, what: &str) -> Result<&'n [Node], Diagnostic> {
    node.items()
        .ok_or_else(|| type_problem(node, what, "array"))
}

fn text(node: &Node, what: &str) -> Result<String, Diagnostic> {
    node.as_str()
        .map(str::to_owned)
        .ok_or_else(|| type_problem(node, what, "string"))
}

fn positive_whole_number(node: &Node, what: &str) -> Result<usize, Diagnostic> {
    let whole = match &node.kind {
        NodeKind::Number(number) => number.parse::<i64>().ok(),
        _ => None,
    }
    .ok_or_else(|| type_problem(node, what, "integer"))?;
    usize::try_from(whole)
        .ok()
        .filter(|whole| *whole >= 1)
        .ok_or_else(|| {
            Diagnostic::at(&node.position, format!("{what} must be >= 1"))
                .with_label(format!("is {whole}"))
        })
}

fn texts(node: &Node, what: &str) -> Result<Vec<String>, Diagnostic> {
    items(node, what)?
        .iter()
        .map(|item| text(item, what))
        .collect()
}

fn json_path(node: &Node) -> Result<PathPattern, Diagnostic> {
    let path = text(node, "a JSONPath")?;
    PathPattern::parse(&path, &[]).map_err(|error| Diagnostic::at(&node.position, error.message))
}

fn regex(node: &Node) -> Result<Regex, Diagnostic> {
    let pattern = text(node, "a pattern")?;
    Regex::new(&pattern).map_err(|error| {
        Diagnostic::at(
            &node.position,
            format!("pattern `{pattern}` is not a regular expression"),
        )
        .with_label(error.to_string())
    })
}
