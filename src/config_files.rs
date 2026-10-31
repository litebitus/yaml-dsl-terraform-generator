//! A config folder's `yaml-dsl.yml` and `tfgen.yml`: the DSLs it declares, and
//! what each generated DSL compiles to.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fancy_regex::Regex;
use yaml_dsl_compiler::json_path::PathPattern;
use yaml_dsl_compiler::yaml_reader::parse;
use yaml_dsl_compiler::{Compiler, Diagnostic, DslDefinition, Entry, Node, NodeKind, Position};

use crate::disk_source::DiskSource;

pub const DSL_CONFIG_FILE: &str = "yaml-dsl.yml";
pub const TFGEN_CONFIG_FILE: &str = "tfgen.yml";
const DEFAULT_DESCRIPTOR: &str = "stack.tg.hcl";

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
}

pub struct TfgenConfig {
    pub file: PathBuf,
    pub descriptor_names: Vec<String>,
    pub globals: Vec<(String, Node)>,
    pub generators: Vec<Generator>,
    pub stack_templates: Vec<(String, StackTemplate)>,
}

pub struct Generator {
    pub dsl_id: String,
    pub position: Position,
    pub written_to: String,
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
    pub fn load(directory: &Path, source: &Arc<DiskSource>) -> Result<Self, Diagnostic> {
        let dsl_file = directory.join(DSL_CONFIG_FILE);
        let dsl_root = read_yaml(&dsl_file)?;
        let tfgen_file = directory.join(TFGEN_CONFIG_FILE);
        let tfgen = if tfgen_file.exists() {
            Some(TfgenConfig::read(&tfgen_file, directory)?)
        } else {
            None
        };
        let generated_ids: Vec<&str> = tfgen
            .iter()
            .flat_map(|tfgen| &tfgen.generators)
            .map(|generator| generator.dsl_id.as_str())
            .collect();
        let blocks = dsl_root
            .get("dsls")
            .and_then(Node::items)
            .ok_or_else(|| Diagnostic::at(&dsl_root.position, "`dsls` lists the config's DSLs"))?;
        let mut dsls = Vec::with_capacity(blocks.len());
        for block in blocks {
            let json = block.to_json();
            let id = json["id"]
                .as_str()
                .ok_or_else(|| Diagnostic::at(&block.position, "a DSL block has an `id`"))?
                .to_owned();
            let selection_block = serde_json::json!({
                "id": id,
                "file_includes": json["file_includes"],
                "file_excludes": json.get("file_excludes").cloned().unwrap_or(serde_json::json!([])),
            });
            let compiler = |json: &serde_json::Value| {
                let definition = DslDefinition::from_json(json).map_err(|message| {
                    Diagnostic::at(&block.position, format!("DSL `{id}`: {message}"))
                })?;
                let source: Arc<dyn yaml_dsl_compiler::FileSource> = source.clone();
                Compiler::new(definition, directory, source)
                    .map_err(|error| located(error, &block.position, &id))
            };
            let selection = compiler(&selection_block)?;
            let generated = if generated_ids.contains(&id.as_str()) {
                Some(compiler(&json)?)
            } else {
                None
            };
            dsls.push(DeclaredDsl {
                id,
                selection,
                generated,
            });
        }
        if let Some(tfgen) = &tfgen {
            for generator in &tfgen.generators {
                if !dsls.iter().any(|dsl| dsl.id == generator.dsl_id) {
                    return Err(Diagnostic::at(
                        &generator.position,
                        format!(
                            "generator `{}` names no DSL of {DSL_CONFIG_FILE}",
                            generator.dsl_id
                        ),
                    ));
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

    pub fn descriptor_names(&self) -> Vec<String> {
        match &self.tfgen {
            Some(tfgen) => tfgen.descriptor_names.clone(),
            None => vec![DEFAULT_DESCRIPTOR.to_owned()],
        }
    }
}

fn located(error: Diagnostic, position: &Position, id: &str) -> Diagnostic {
    match error.position {
        Some(_) => error,
        None => Diagnostic::at(position, format!("DSL `{id}`: {}", error.message)),
    }
}

pub fn read_yaml(file: &Path) -> Result<Node, Diagnostic> {
    let text = fs::read_to_string(file)
        .map_err(|error| Diagnostic::unplaced(format!("reading {}: {error}", file.display())))?;
    parse(&Arc::from(file.to_string_lossy().as_ref()), &text, &[])
}

impl TfgenConfig {
    fn read(file: &Path, directory: &Path) -> Result<Self, Diagnostic> {
        let root = read_yaml(file)?;
        let mut config = MapReader::new(&root, "tfgen.yml")?;
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
                generators.push(Generator::read(entry, directory)?);
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
        })
    }
}

impl Generator {
    fn read(entry: &Entry, directory: &Path) -> Result<Self, Diagnostic> {
        let what = format!("generator `{}`", entry.key);
        let mut generator = MapReader::new(&entry.value, &what)?;
        let written_to = text(generator.required("written_to")?, "`written_to`")?;
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
                references.push(ReferenceRule::read(reference)?);
            }
        }
        generator.finish()?;
        Ok(Self {
            dsl_id: entry.key.clone(),
            position: entry.key_position.clone(),
            written_to,
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
        let selection = match block.optional("each") {
            None => BlockSelection::Once,
            Some(each) => match text(each, "`each`")?.as_str() {
                "declaration" => BlockSelection::Declaration {
                    scope: scope(&mut block)?,
                },
                "scope_key" => BlockSelection::ScopeKey {
                    scope: scope(&mut block)?,
                },
                "entry" => BlockSelection::Entry {
                    path: json_path(block.required("path")?)?,
                },
                other => {
                    return Err(Diagnostic::at(
                        &each.position,
                        format!("`each: {other}` is not declaration, scope_key or entry"),
                    ));
                }
            },
        };
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
    fn read(node: &Node) -> Result<Self, Diagnostic> {
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
            start: template_node.position.clone(),
        };
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

fn template_file(node: &Node, directory: &Path) -> Result<TemplateText, Diagnostic> {
    let relative = text(node, "a template file")?;
    let file = directory.join(&relative);
    let text = fs::read_to_string(&file).map_err(|error| {
        Diagnostic::at(
            &node.position,
            format!("reading template {relative}: {error}"),
        )
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
            return Err(Diagnostic::at(&node.position, format!("{what} is a map")));
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
        self.optional(key)
            .ok_or_else(|| Diagnostic::at(&self.node.position, format!("{what} needs `{key}`")))
    }

    fn finish(self) -> Result<(), Diagnostic> {
        for entry in self.node.entries().unwrap_or_default() {
            if !self.read.contains(&entry.key.as_str()) {
                return Err(Diagnostic::at(
                    &entry.key_position,
                    format!("{} has no field `{}`", self.what, entry.key),
                ));
            }
        }
        Ok(())
    }
}

fn entries<'n>(node: &'n Node, what: &str) -> Result<&'n [Entry], Diagnostic> {
    node.entries()
        .ok_or_else(|| Diagnostic::at(&node.position, format!("{what} is a map")))
}

fn items<'n>(node: &'n Node, what: &str) -> Result<&'n [Node], Diagnostic> {
    node.items()
        .ok_or_else(|| Diagnostic::at(&node.position, format!("{what} is a list")))
}

fn text(node: &Node, what: &str) -> Result<String, Diagnostic> {
    node.as_str()
        .map(str::to_owned)
        .ok_or_else(|| Diagnostic::at(&node.position, format!("{what} is text")))
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
    Regex::new(&pattern)
        .map_err(|error| Diagnostic::at(&node.position, format!("pattern `{pattern}`: {error}")))
}
