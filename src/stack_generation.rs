//! One stack's generation: its documents compiled by their generators, the
//! templates it takes, and the files they write.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use yaml_dsl_compiler::output::{BodyItem, OutputFile};
use yaml_dsl_compiler::writer::write_file;
use yaml_dsl_compiler::{
    BuiltinValues, Compiler, ResolveOptions, ResolvedUnit, TargetLanguage, Unit,
};
use yaml_dsl_spec::json_path::PathStep;
use yaml_dsl_spec::resolved::{Declared, MapEntry, Value, ValueKind};
use yaml_dsl_spec::{Diagnostic, Position};

use crate::config_files::{
    BlockRule, BlockSelection, DeclaredDsl, Generator, LoadedConfig, TfgenConfig,
};
use crate::error_report::did_you_mean;
use crate::generated_files::header_comment;
use crate::ref_scope::{
    FixedRoot, GeneratorContext, RefFrame, SelectableDeclaration, StackContext,
};
use crate::schema_cache::SchemaCache;
use crate::schema_lookup::{SchemaSource, StackSchemas};
use crate::schema_overrides::SchemaOverrides;
use crate::stack_discovery::git_files;
use crate::template_filling::fill_body;
use crate::value_writing::ValueWriter;
use crate::yaml_values::evaluate_node;

/// Terraform's functions that read a file named by their first argument.
const FILE_READING_FUNCTIONS: [&str; 9] = [
    "file",
    "filebase64",
    "filebase64sha256",
    "filebase64sha512",
    "filemd5",
    "filesha1",
    "filesha256",
    "filesha512",
    "templatefile",
];

/// A stack to generate: its directory, its descriptor's globals and its config.
pub struct StackJob<'c> {
    pub directory: PathBuf,
    pub globals: Value,
    pub config: &'c LoadedConfig,
    pub repository_root: &'c Path,
    /// By DSL id, the schema this run reads the DSL's documents against.
    pub schema_overrides: &'c SchemaOverrides,
    /// Schema URLs read this run, with the copies kept of them.
    pub schema_cache: &'c SchemaCache,
}

/// The files a stack's generation writes, by path.
pub struct StackFiles {
    pub directory: PathBuf,
    pub files: Vec<(PathBuf, String)>,
    /// What generating the stack warns of without failing it.
    pub warnings: Vec<Diagnostic>,
}

struct CompiledDocument<'c> {
    generator: &'c Generator,
    resolved: ResolvedUnit,
}

pub fn generate_stack(job: &StackJob<'_>) -> Result<StackFiles, Vec<Diagnostic>> {
    let start = Position::file_start(&std::sync::Arc::from(
        job.directory.to_string_lossy().as_ref(),
    ));
    let one = |diagnostic: Diagnostic| vec![diagnostic];
    let tfgen = job.config.tfgen.as_ref().ok_or_else(|| {
        one(Diagnostic::at(
            &start,
            format!(
                "the stack's globals hold `tfgen`, and {} has no tfgen.yml",
                job.config.directory.display()
            ),
        ))
    })?;
    let request = job
        .globals
        .get("tfgen")
        .map(Value::local_value)
        .ok_or_else(|| {
            one(Diagnostic::at(
                &start,
                "the stack's globals hold no `tfgen`",
            ))
        })?;
    let named_templates = named_templates(request, tfgen).map_err(one)?;
    let stack_path = relative_slash_path(&job.directory, &job.config.directory);
    let required_providers = required_providers(&named_templates, &job.globals.position);
    let mut stack_root = with_entries(
        &job.globals,
        vec![
            (
                "__path__",
                Value::string(stack_path, job.globals.position.clone()),
            ),
            ("__required_providers__", required_providers),
        ],
    );
    let documents = stack_documents(job, request).map_err(one)?;
    let early_context = StackContext::new(stack_root.clone(), &tfgen.globals);
    let mut compiled = Vec::new();
    let mut diagnostics = Vec::new();
    let mut units = Vec::new();
    for (dsl, document) in &documents {
        let Some(generator) = job.config.generator(&dsl.id) else {
            continue;
        };
        match document_unit(dsl, document) {
            Ok(unit) => units.push((*dsl, generator, unit)),
            Err(errors) => diagnostics.extend(errors),
        }
    }
    for dsl in &job.config.dsls {
        let Some(generator) = job.config.generator(&dsl.id) else {
            continue;
        };
        if documents.iter().any(|(listed, _)| listed.id == dsl.id) {
            continue;
        }
        let compiler = generated_compiler(dsl);
        match compiler.overlays_holding_no_documents_at(&job.directory) {
            Ok(found) => units.extend(found.into_iter().map(|unit| (dsl, generator, unit))),
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }
    for (dsl, generator, unit) in &units {
        if compiled
            .iter()
            .any(|existing: &CompiledDocument<'_>| existing.generator.dsl_id == dsl.id)
        {
            diagnostics.push(Diagnostic::at(
                &start,
                format!("the stack holds more than one `{}` document", dsl.id),
            ));
            continue;
        }
        let schema_override = job.schema_overrides.get(&dsl.id).map(String::as_str);
        let compiled_document = compile_document(
            dsl,
            unit,
            generator,
            &early_context,
            schema_override,
            job.schema_cache,
            &job.config.directory,
        );
        match compiled_document {
            Ok(Some(resolved)) => compiled.push(CompiledDocument {
                generator,
                resolved,
            }),
            Ok(None) => {}
            Err(errors) => diagnostics.extend(errors),
        }
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let data_files = data_files(&compiled, &job.directory);
    stack_root = with_entries(
        &stack_root,
        vec![(
            "__data_files__",
            Value::new(
                ValueKind::List(
                    data_files
                        .into_iter()
                        .map(|file| Value::string(file, job.globals.position.clone()))
                        .collect(),
                ),
                job.globals.position.clone(),
            ),
        )],
    );
    let context = StackContext::new(stack_root, &tfgen.globals);
    let mut outputs: Vec<OutputFile> = Vec::new();
    for document in &compiled {
        match generator_file(job, document, &context) {
            Ok(file) => outputs.push(file),
            Err(errors) => diagnostics.extend(errors),
        }
    }
    match template_files(job, tfgen, &named_templates, &context) {
        Ok(files) => outputs.extend(files),
        Err(errors) => diagnostics.extend(errors),
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    let mut files = Vec::with_capacity(outputs.len());
    for output in &outputs {
        let path = job.directory.join(&output.path);
        if files
            .iter()
            .any(|(written, _): &(PathBuf, String)| *written == path)
        {
            diagnostics.push(Diagnostic::at(
                &start,
                format!("two generators write {}", output.path.display()),
            ));
            continue;
        }
        match write_file(output) {
            Ok(contents) => files.push((path, contents)),
            Err(diagnostic) => diagnostics.push(diagnostic),
        }
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    Ok(StackFiles {
        directory: job.directory.clone(),
        files,
        warnings: compiled
            .iter()
            .flat_map(|document| document.resolved.warnings.iter().cloned())
            .collect(),
    })
}

/// The templates the stack's `tfgen.templates` names, each with its settings.
fn named_templates<'t>(
    request: &Value,
    tfgen: &'t TfgenConfig,
) -> Result<Vec<(&'t str, &'t crate::config_files::StackTemplate, Value)>, Diagnostic> {
    let Some(templates) = request.get("templates") else {
        return Ok(Vec::new());
    };
    let entries = templates.local_value().entries().ok_or_else(|| {
        Diagnostic::at(
            &templates.position,
            "`tfgen.templates` maps template names to settings",
        )
    })?;
    entries
        .iter()
        .map(|entry| {
            let (name, template) = tfgen
                .stack_templates
                .iter()
                .find(|(name, _)| *name == entry.key)
                .ok_or_else(|| {
                    let declared: Vec<String> = tfgen
                        .stack_templates
                        .iter()
                        .map(|(name, _)| name.clone())
                        .collect();
                    Diagnostic::at(
                        &entry.key_position,
                        format!("template `{}` is not declared", entry.key),
                    )
                    .with_label(format!("tfgen.yml declares no template `{}`", entry.key))
                    .with_help(did_you_mean(&entry.key, &declared))
                })?;
            Ok((name.as_str(), template, entry.value.clone()))
        })
        .collect()
}

/// `stack.__required_providers__`: each provider the named templates require, as
/// `required_providers` writes it.
fn required_providers(
    templates: &[(&str, &crate::config_files::StackTemplate, Value)],
    position: &Position,
) -> Value {
    let mut providers: Vec<MapEntry> = Vec::new();
    for (_, template, _) in templates {
        for (name, source) in &template.required_providers {
            if providers.iter().any(|provider| provider.key == *name) {
                continue;
            }
            let requirement = Value::new(
                ValueKind::Map(vec![MapEntry {
                    key: "source".to_owned(),
                    key_position: position.clone(),
                    value: Value::string(source.clone(), position.clone()),
                }]),
                position.clone(),
            );
            providers.push(MapEntry {
                key: name.clone(),
                key_position: position.clone(),
                value: requirement,
            });
        }
    }
    Value::new(ValueKind::Map(providers), position.clone())
}

/// The stack's documents with the DSL that selects each: the files of its own
/// directory, then those its `imports` name.
fn stack_documents<'c>(
    job: &StackJob<'c>,
    request: &Value,
) -> Result<Vec<(&'c DeclaredDsl, PathBuf)>, Diagnostic> {
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&job.directory)
        .map_err(|error| {
            Diagnostic::unplaced(format!("reading {}: {error}", job.directory.display()))
        })?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_file())
        .collect();
    candidates.sort();
    if let Some(imports) = request.get("imports") {
        let patterns = imports.local_value().items().ok_or_else(|| {
            Diagnostic::at(&imports.position, "`tfgen.imports` lists paths and globs")
        })?;
        for pattern in patterns {
            let text = pattern.local_value().as_str().ok_or_else(|| {
                Diagnostic::at(&pattern.position, "an import is a path or a glob")
            })?;
            let mut imported = git_files(&job.directory, Some(text))
                .map_err(|message| Diagnostic::at(&pattern.position, message))?;
            imported.sort();
            candidates.extend(imported);
        }
    }
    let mut documents = Vec::new();
    for candidate in candidates {
        let mut selecting = Vec::new();
        for dsl in &job.config.dsls {
            if dsl.selection.selects(&candidate)? {
                selecting.push(dsl);
            }
        }
        match selecting.as_slice() {
            [] => {}
            [dsl] => documents.push((*dsl, candidate)),
            [first, second, ..] => {
                return Err(Diagnostic::unplaced(format!(
                    "{} is a document of two DSLs: `{}` and `{}`",
                    candidate.display(),
                    first.id,
                    second.id
                )));
            }
        }
    }
    Ok(documents)
}

struct StackBuiltins(BTreeMap<String, ValueKind>);

impl BuiltinValues for StackBuiltins {
    fn value(&self, _unit: &Unit, _scope: &str, name: &str) -> Option<ValueKind> {
        self.0.get(name).cloned()
    }
}

/// The compiler of a DSL a generator compiles.
fn generated_compiler(dsl: &DeclaredDsl) -> &Compiler {
    dsl.generated
        .as_ref()
        .expect("a DSL with a generator has its compiler")
}

/// The unit a stack's document compiles in: the document with the common layer it
/// takes.
fn document_unit(dsl: &DeclaredDsl, document: &Path) -> Result<Unit, Vec<Diagnostic>> {
    generated_compiler(dsl)
        .unit_of(document)
        .map_err(|diagnostic| vec![diagnostic])?
        .ok_or_else(|| {
            vec![Diagnostic::unplaced(format!(
                "{} is outside the config folder of DSL `{}`",
                document.display(),
                dsl.id
            ))]
        })
}

fn compile_document(
    dsl: &DeclaredDsl,
    unit: &Unit,
    generator: &Generator,
    context: &StackContext<'_>,
    schema_override: Option<&str>,
    schema_cache: &SchemaCache,
    config_root: &Path,
) -> Result<Option<ResolvedUnit>, Vec<Diagnostic>> {
    let compiler = generated_compiler(dsl);
    let frame = RefFrame::of_stack(context);
    let mut builtins = BTreeMap::new();
    for (name, node) in &generator.builtin_values {
        let value = evaluate_node(node, &frame).map_err(|diagnostic| vec![diagnostic])?;
        if let Some(value) = value {
            builtins.insert(name.clone(), value.local_value().kind.clone());
        }
    }
    let generator_schema = match &generator.schema {
        Some(node) => Some(schema_entries(node, &frame).map_err(|diagnostic| vec![diagnostic])?),
        None => None,
    };
    let schemas = StackSchemas {
        config_root,
        run_schema: schema_override,
        search_paths: generator_schema.as_deref().unwrap_or(&dsl.search_paths),
        schema_cache,
    };
    let options = ResolveOptions {
        builtins: Some(&StackBuiltins(builtins)),
        schema_provider: Some(&schemas),
    };
    let resolved = compiler.resolve_with(unit, &options)?;
    Ok((!resolved.fragment).then_some(resolved))
}

/// A generator's `schema` list, its refs filled for the stack, each entry with
/// where it was written.
fn schema_entries(
    node: &yaml_dsl_spec::Node,
    frame: &RefFrame<'_>,
) -> Result<Vec<SchemaSource>, Diagnostic> {
    let not_a_list = || {
        Diagnostic::at(
            &node.position,
            "`schema` lists paths, path patterns and URLs, the first found taken",
        )
    };
    let entries = evaluate_node(node, frame)?.ok_or_else(not_a_list)?;
    entries
        .items()
        .ok_or_else(not_a_list)?
        .iter()
        .map(|entry| {
            let location = entry.as_str().ok_or_else(not_a_list)?;
            Ok(SchemaSource {
                location: location.to_owned(),
                written_at: entry.position.clone(),
            })
        })
        .collect()
}

/// The files the stack reads, from its directory: its documents, the files its
/// generators' `data_files` name, and the files its calls read.
fn data_files(compiled: &[CompiledDocument<'_>], directory: &Path) -> Vec<String> {
    let mut files = Vec::new();
    for document in compiled {
        let unit = &document.resolved.unit;
        if !unit.holds_no_documents {
            files.push(stack_relative(&unit.file, directory));
        }
        if let Some(common_layer) = &unit.common_layer {
            files.push(stack_relative(common_layer, directory));
        }
        for pattern in &document.generator.data_files {
            for (_, value) in pattern.select(&document.resolved.document) {
                match &value.kind {
                    ValueKind::String(text) => files.push(text.clone()),
                    ValueKind::List(items) => files.extend(
                        items
                            .iter()
                            .filter_map(|item| item.as_str().map(str::to_owned)),
                    ),
                    _ => {}
                }
            }
        }
        collect_call_files(&document.resolved.document, &mut files);
    }
    files.sort();
    files.dedup();
    files
}

fn collect_call_files(value: &Value, files: &mut Vec<String>) {
    match &value.kind {
        ValueKind::Call(call) => {
            if FILE_READING_FUNCTIONS.contains(&call.function.as_str())
                && let Some(text) = call.arguments.first().and_then(Value::as_str)
            {
                files.push(text.to_owned());
            }
            for argument in &call.arguments {
                collect_call_files(argument, files);
            }
        }
        ValueKind::List(items) => items
            .iter()
            .for_each(|item| collect_call_files(item, files)),
        ValueKind::Map(entries) => entries
            .iter()
            .for_each(|entry| collect_call_files(&entry.value, files)),
        _ => {}
    }
}

/// A path from the stack's directory, `./` before a file under it.
fn stack_relative(path: &Path, directory: &Path) -> String {
    let relative = relative_slash_path(path, directory);
    if relative.starts_with("../") {
        relative
    } else {
        format!("./{relative}")
    }
}

/// `path` from `directory`, with `/` separators and `..` where it climbs.
fn relative_slash_path(path: &Path, directory: &Path) -> String {
    let path_parts: Vec<_> = path.components().collect();
    let directory_parts: Vec<_> = directory.components().collect();
    let shared = path_parts
        .iter()
        .zip(&directory_parts)
        .take_while(|(first, second)| first == second)
        .count();
    let mut parts: Vec<String> = vec!["..".to_owned(); directory_parts.len() - shared];
    parts.extend(
        path_parts[shared..]
            .iter()
            .map(|part| part.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

/// A part of the folded document a block writes, where the document holds it.
struct WrittenPart {
    order: Vec<usize>,
    items: Vec<BodyItem>,
}

fn generator_file(
    job: &StackJob<'_>,
    document: &CompiledDocument<'_>,
    context: &StackContext<'_>,
) -> Result<OutputFile, Vec<Diagnostic>> {
    let generator = document.generator;
    let resolved = &document.resolved;
    let generator_context = GeneratorContext::new(
        resolved.document.clone(),
        selectable_declarations(resolved),
        &generator.values,
    );
    let frame = RefFrame {
        stack: context,
        generator: Some(&generator_context),
        each: None,
        reference: None,
    };
    let writer = ValueWriter::new(&generator.references, frame);
    let mut claimed: Vec<Vec<PathStep>> = Vec::new();
    let mut parts = Vec::new();
    let mut once_items = Vec::new();
    let mut diagnostics = Vec::new();
    for block in &generator.blocks {
        let selected = match selected_parts(block, resolved) {
            Ok(selected) => selected,
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                continue;
            }
        };
        let Some(selected) = selected else {
            match fill_body(&block.template, &frame, &writer) {
                Ok(items) => once_items.extend(items),
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
            continue;
        };
        for (path, each) in selected {
            if claimed.contains(&path) {
                continue;
            }
            let each_frame = frame.with_each(&each);
            match fill_body(&block.template, &each_frame, &writer) {
                Ok(items) => parts.push(WrittenPart {
                    order: document_order(&resolved.document, &path),
                    items,
                }),
                Err(diagnostic) => diagnostics.push(diagnostic),
            }
            claimed.push(path);
        }
    }
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }
    parts.sort_by(|first, second| first.order.cmp(&second.order));
    let mut body: Vec<BodyItem> = parts.into_iter().flat_map(|part| part.items).collect();
    body.extend(once_items);
    let mut sources = Vec::new();
    if !resolved.unit.holds_no_documents {
        sources.push(relative_slash_path(
            &resolved.unit.file,
            job.repository_root,
        ));
    }
    if let Some(common_layer) = &resolved.unit.common_layer {
        sources.push(relative_slash_path(common_layer, job.repository_root));
    }
    Ok(OutputFile {
        path: PathBuf::from(&generator.written_to),
        language: TargetLanguage::Hcl,
        comment: Some(header_comment(&sources)),
        body,
    })
}

/// A part of the folded document, by its path, and the value a template reads as
/// `each` for it.
type SelectedPart = (Vec<PathStep>, Value);

/// The parts of the document a block selects, each with its path and the value
/// its template reads as `each`; `None` for a block written once.
fn selected_parts(
    block: &BlockRule,
    resolved: &ResolvedUnit,
) -> Result<Option<Vec<SelectedPart>>, Diagnostic> {
    let keeps_scope_key = |scope_key: Option<&String>| {
        let Some(scope_key) = scope_key else {
            return block.scope_keys.is_none();
        };
        block
            .scope_keys
            .as_ref()
            .is_none_or(|keys| keys.contains(scope_key))
            && !block.scope_keys_except.contains(scope_key)
    };
    let value_kept = |value: &Value| {
        block.value_matches.as_ref().is_none_or(|pattern| {
            value_text(value).is_some_and(|text| pattern.is_match(&text).unwrap_or(false))
        })
    };
    Ok(Some(match &block.selection {
        BlockSelection::Once => return Ok(None),
        BlockSelection::Declaration { scope } => resolved
            .declarations
            .iter()
            .filter(|declared| declared.scope == *scope)
            .filter(|declared| keeps_scope_key(declared.scope_key.as_ref()))
            .filter(|declared| value_kept(&declared.value))
            .map(|declared| (declared.path.clone(), declaration_value(declared)))
            .collect(),
        BlockSelection::ScopeKey { scope } => {
            let mut groups: Vec<(String, Vec<PathStep>, Vec<&Declared>)> = Vec::new();
            for declared in &resolved.declarations {
                let Some(scope_key) = &declared.scope_key else {
                    continue;
                };
                if declared.scope != *scope || !keeps_scope_key(Some(scope_key)) {
                    continue;
                }
                match groups.iter_mut().find(|(key, _, _)| key == scope_key) {
                    Some((_, _, members)) => members.push(declared),
                    None => {
                        let parent = declared.path[..declared.path.len() - 1].to_vec();
                        groups.push((scope_key.clone(), parent, vec![declared]));
                    }
                }
            }
            groups
                .into_iter()
                .map(|(scope_key, path, members)| (path, scope_key_value(&scope_key, &members)))
                .collect()
        }
        BlockSelection::Entry { path } => path
            .select(&resolved.document)
            .into_iter()
            .filter(|(_, value)| value_kept(value))
            .map(|(steps, value)| {
                let key = match steps.last() {
                    Some(PathStep::Key(key)) => key.clone(),
                    Some(PathStep::Index(index)) => index.to_string(),
                    None => String::new(),
                };
                let each = FixedRoot::EntryEach.value(
                    &value.position,
                    vec![Value::string(key, value.position.clone()), value.clone()],
                );
                (steps, each)
            })
            .collect(),
    }))
}

/// The stack's declarations, in the document's order, as a value selects them.
fn selectable_declarations(resolved: &ResolvedUnit) -> Vec<SelectableDeclaration> {
    let mut declarations: Vec<&Declared> = resolved.declarations.iter().collect();
    declarations.sort_by_key(|declared| document_order(&resolved.document, &declared.path));
    declarations
        .into_iter()
        .map(|declared| SelectableDeclaration {
            scope: declared.scope.clone(),
            scope_key: declared.scope_key.clone(),
            each: declaration_value(declared),
        })
        .collect()
}

fn declaration_value(declared: &Declared) -> Value {
    let position = &declared.key_position;
    let scope_key = match &declared.scope_key {
        Some(scope_key) => Value::string(scope_key.clone(), position.clone()),
        None => Value::new(ValueKind::Null, position.clone()),
    };
    FixedRoot::DeclarationEach.value(
        position,
        vec![
            scope_key,
            Value::string(declared.name.clone(), position.clone()),
            declared.value.clone(),
        ],
    )
}

fn scope_key_value(scope_key: &str, members: &[&Declared]) -> Value {
    let position = &members[0].key_position;
    let declarations = Value::new(
        ValueKind::Map(
            members
                .iter()
                .map(|declared| MapEntry {
                    key: declared.name.clone(),
                    key_position: declared.key_position.clone(),
                    value: declared.value.clone(),
                })
                .collect(),
        ),
        position.clone(),
    );
    FixedRoot::ScopeKeyEach.value(
        position,
        vec![Value::string(scope_key, position.clone()), declarations],
    )
}

/// A value's text for `value_matches`: a scalar's text, or a reference as written.
fn value_text(value: &Value) -> Option<String> {
    match &value.kind {
        ValueKind::Reference(reference) => Some(reference.text.clone()),
        _ => value.scalar_text(),
    }
}

/// Where `path` stands in `document`: the index of each of its steps.
fn document_order(document: &Value, path: &[PathStep]) -> Vec<usize> {
    let mut order = Vec::with_capacity(path.len());
    let mut current = Some(document);
    for step in path {
        let (index, next) = match (step, current.map(|value| &value.kind)) {
            (PathStep::Key(key), Some(ValueKind::Map(entries))) => {
                match entries.iter().position(|entry| entry.key == *key) {
                    Some(index) => (index, Some(&entries[index].value)),
                    None => (usize::MAX, None),
                }
            }
            (PathStep::Index(index), Some(ValueKind::List(items))) => (*index, items.get(*index)),
            _ => (usize::MAX, None),
        };
        order.push(index);
        current = next;
    }
    order
}

fn template_files(
    job: &StackJob<'_>,
    tfgen: &TfgenConfig,
    named: &[(&str, &crate::config_files::StackTemplate, Value)],
    context: &StackContext<'_>,
) -> Result<Vec<OutputFile>, Vec<Diagnostic>> {
    let frame = RefFrame::of_stack(context);
    let writer = ValueWriter::new(&[], frame);
    let source = relative_slash_path(&tfgen.file, job.repository_root);
    let mut files: Vec<OutputFile> = Vec::new();
    let mut diagnostics = Vec::new();
    for (_, template, _) in named {
        let items = match fill_body(&template.template, &frame, &writer) {
            Ok(items) => items,
            Err(diagnostic) => {
                diagnostics.push(diagnostic);
                continue;
            }
        };
        let path = PathBuf::from(&template.written_to);
        match files.iter_mut().find(|file| file.path == path) {
            Some(file) => file.body.extend(items),
            None => files.push(OutputFile {
                path,
                language: TargetLanguage::Hcl,
                comment: Some(header_comment(std::slice::from_ref(&source))),
                body: items,
            }),
        }
    }
    if diagnostics.is_empty() {
        Ok(files)
    } else {
        Err(diagnostics)
    }
}

/// `value`, a map, with `entries` set over its own.
fn with_entries(value: &Value, entries: Vec<(&str, Value)>) -> Value {
    let mut merged: Vec<MapEntry> = value
        .entries()
        .map(<[MapEntry]>::to_vec)
        .unwrap_or_default();
    for (key, entry_value) in entries {
        merged.retain(|entry| entry.key != key);
        merged.push(MapEntry {
            key: key.to_owned(),
            key_position: value.position.clone(),
            value: entry_value,
        });
    }
    Value::new(ValueKind::Map(merged), value.position.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_written_from_the_stack_directory() {
        let stack = Path::new("/repo/rds/mock/dev");
        assert_eq!(
            stack_relative(Path::new("/repo/rds/mock/dev/resources.yml"), stack),
            "./resources.yml"
        );
        assert_eq!(
            stack_relative(Path::new("/repo/rds/mock/resources.yml"), stack),
            "../resources.yml"
        );
        assert_eq!(
            relative_slash_path(stack, Path::new("/repo")),
            "rds/mock/dev"
        );
    }
}
