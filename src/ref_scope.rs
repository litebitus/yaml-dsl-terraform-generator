//! What a ref reads: the roots a stack, a generator and a template rule provide,
//! and the walk down a ref's path.

use std::cell::RefCell;
use std::collections::BTreeMap;

use yaml_dsl_compiler::resolved::{MapEntry, Value, ValueKind};
use yaml_dsl_compiler::{Diagnostic, Node, Position};

use crate::config_files::ValueRule;
use crate::error_report::did_you_mean;
use crate::ref_paths::{RefPath, RefStep};
use crate::yaml_values::{evaluate_node, evaluate_value_rule};

/// A root whose fields tfgen fixes: what `each` holds for each kind of block, and
/// what `reference` holds. Its value is built from these fields, and a template's
/// refs to it are checked against them when the config is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixedRoot {
    DeclarationEach,
    ScopeKeyEach,
    EntryEach,
    Reference,
}

impl FixedRoot {
    /// The roots a block's `each` names, in the order its error lists them.
    pub const BLOCK_EACH_ROOTS: [FixedRoot; 3] = [
        FixedRoot::DeclarationEach,
        FixedRoot::ScopeKeyEach,
        FixedRoot::EntryEach,
    ];

    /// The name a ref reads the root by.
    pub fn root_name(self) -> &'static str {
        match self {
            FixedRoot::Reference => "reference",
            _ => "each",
        }
    }

    /// The value a block's `each` takes to fill its template with this root.
    pub fn block_each_kind(self) -> Option<&'static str> {
        match self {
            FixedRoot::DeclarationEach => Some("declaration"),
            FixedRoot::ScopeKeyEach => Some("scope_key"),
            FixedRoot::EntryEach => Some("entry"),
            FixedRoot::Reference => None,
        }
    }

    pub fn fields(self) -> &'static [&'static str] {
        match self {
            FixedRoot::DeclarationEach => &["scope_key", "name", "value"],
            FixedRoot::ScopeKeyEach => &["name", "declarations"],
            FixedRoot::EntryEach => &["key", "value"],
            FixedRoot::Reference => &["scope_key", "name", "path"],
        }
    }

    /// The root's value: its fields, in the order `fields` lists them, holding
    /// `values`.
    pub fn value(self, position: &Position, values: Vec<Value>) -> Value {
        assert_eq!(
            values.len(),
            self.fields().len(),
            "{self:?} takes one value per field"
        );
        let entries = self
            .fields()
            .iter()
            .zip(values)
            .map(|(field, value)| MapEntry {
                key: (*field).to_owned(),
                key_position: position.clone(),
                value,
            })
            .collect();
        Value::new(ValueKind::Map(entries), position.clone())
    }
}

/// A ref reading a key the value it reads from does not hold.
pub fn absent_key(
    path: &RefPath,
    holder: &str,
    key: &str,
    held: &[String],
    position: &Position,
) -> Diagnostic {
    Diagnostic::at(position, format!("`{path}` reads a key that is not there"))
        .with_label(format!("`{holder}` holds no `{key}`"))
        .with_help(did_you_mean(key, held))
}

/// A ref reading a root the text it stands in is not filled with.
pub fn unread_root(path: &RefPath, roots: &[String], position: &Position) -> Diagnostic {
    let read = roots
        .iter()
        .map(|root| format!("`{root}`"))
        .collect::<Vec<_>>()
        .join(", ");
    Diagnostic::at(
        position,
        format!("`{path}` reads a root that is not read here"),
    )
    .with_label(format!("this text reads {read}, and no `{}`", path.root))
    .with_help(did_you_mean(&path.root, roots))
}

/// A stack's roots: `stack`, its descriptor's globals and builtins, and `tfgen`,
/// the config's globals, each computed once, when first read.
pub struct StackContext<'c> {
    pub stack: Value,
    tfgen_globals: &'c [(String, Node)],
    computed_globals: LazyTable,
}

/// A generator's roots for one stack: `fold`, the stack's folded document, and
/// `values`, computed once, when first read.
pub struct GeneratorContext<'c> {
    pub fold: Value,
    value_rules: &'c [(String, ValueRule)],
    computed_values: LazyTable,
}

/// The roots one ref can read where it stands.
#[derive(Clone, Copy)]
pub struct RefFrame<'f> {
    pub stack: &'f StackContext<'f>,
    pub generator: Option<&'f GeneratorContext<'f>>,
    pub each: Option<&'f Value>,
    pub reference: Option<&'f Value>,
}

#[derive(Default)]
struct LazyTable {
    computed: RefCell<BTreeMap<String, Option<Value>>>,
    computing: RefCell<Vec<String>>,
}

impl LazyTable {
    fn get(
        &self,
        name: &str,
        position: &Position,
        compute: impl FnOnce() -> Result<Option<Value>, Diagnostic>,
    ) -> Result<Option<Value>, Diagnostic> {
        if let Some(value) = self.computed.borrow().get(name) {
            return Ok(value.clone());
        }
        if self
            .computing
            .borrow()
            .iter()
            .any(|computing| computing == name)
        {
            let chain = self.computing.borrow().join(" -> ");
            return Err(Diagnostic::at(
                position,
                format!("`{name}` reads itself: {chain} -> {name}"),
            ));
        }
        self.computing.borrow_mut().push(name.to_owned());
        let computed = compute();
        self.computing.borrow_mut().pop();
        let value = computed?;
        self.computed
            .borrow_mut()
            .insert(name.to_owned(), value.clone());
        Ok(value)
    }
}

impl<'c> StackContext<'c> {
    pub fn new(stack: Value, tfgen_globals: &'c [(String, Node)]) -> Self {
        Self {
            stack,
            tfgen_globals,
            computed_globals: LazyTable::default(),
        }
    }

    fn global(&self, name: &str, position: &Position) -> Result<Option<Value>, Diagnostic> {
        let Some((_, node)) = self.tfgen_globals.iter().find(|(global, _)| global == name) else {
            return Ok(None);
        };
        self.computed_globals.get(name, position, || {
            let frame = RefFrame {
                stack: self,
                generator: None,
                each: None,
                reference: None,
            };
            evaluate_node(node, &frame)
        })
    }
}

impl<'c> GeneratorContext<'c> {
    pub fn new(fold: Value, value_rules: &'c [(String, ValueRule)]) -> Self {
        Self {
            fold,
            value_rules,
            computed_values: LazyTable::default(),
        }
    }

    fn value(
        &self,
        name: &str,
        position: &Position,
        stack: &StackContext<'_>,
    ) -> Result<Option<Value>, Diagnostic> {
        let Some((_, rule)) = self.value_rules.iter().find(|(value, _)| value == name) else {
            return Ok(None);
        };
        self.computed_values.get(name, position, || {
            let frame = RefFrame {
                stack,
                generator: Some(self),
                each: None,
                reference: None,
            };
            evaluate_value_rule(rule, &frame).map(Some)
        })
    }
}

impl<'f> RefFrame<'f> {
    pub fn of_stack(stack: &'f StackContext<'f>) -> Self {
        Self {
            stack,
            generator: None,
            each: None,
            reference: None,
        }
    }

    pub fn with_each(self, each: &'f Value) -> Self {
        Self {
            each: Some(each),
            ..self
        }
    }

    pub fn with_reference(self, reference: &'f Value) -> Self {
        Self {
            reference: Some(reference),
            ..self
        }
    }

    /// The roots a ref reads here.
    fn root_names(&self) -> Vec<String> {
        let mut roots = vec!["stack", "tfgen"];
        if self.generator.is_some() {
            roots.extend(["fold", "values"]);
        }
        if self.each.is_some() {
            roots.push("each");
        }
        if self.reference.is_some() {
            roots.push("reference");
        }
        roots.into_iter().map(str::to_owned).collect()
    }

    /// The value `path` names, or `None` where an optional key is absent.
    pub fn read(&self, path: &RefPath, position: &Position) -> Result<Option<Value>, Diagnostic> {
        let missing_root = || unread_root(path, &self.root_names(), position);
        let (start, steps) = match path.root.as_str() {
            "stack" => (Some(self.stack.stack.clone()), &path.steps[..]),
            "fold" => (
                Some(self.generator.ok_or_else(missing_root)?.fold.clone()),
                &path.steps[..],
            ),
            "each" => (
                Some(self.each.ok_or_else(missing_root)?.clone()),
                &path.steps[..],
            ),
            "reference" => (
                Some(self.reference.ok_or_else(missing_root)?.clone()),
                &path.steps[..],
            ),
            "tfgen" | "values" => {
                let Some((first, rest)) = path.steps.split_first() else {
                    return Err(Diagnostic::at(
                        position,
                        format!("`{path}` names no {} entry", path.root),
                    ));
                };
                let (name, required) = self.step_key(first, path, position)?;
                let (value, held): (_, Vec<&String>) = if path.root == "tfgen" {
                    let held = self.stack.tfgen_globals.iter().map(|(name, _)| name);
                    (self.stack.global(&name, position)?, held.collect())
                } else {
                    let generator = self.generator.ok_or_else(missing_root)?;
                    let held = generator.value_rules.iter().map(|(name, _)| name);
                    (
                        generator.value(&name, position, self.stack)?,
                        held.collect(),
                    )
                };
                if value.is_none() && required {
                    let held: Vec<String> = held.into_iter().cloned().collect();
                    return Err(absent_key(path, &path.root, &name, &held, position));
                }
                (value, rest)
            }
            _ => return Err(missing_root()),
        };
        let read_before = path.steps.len() - steps.len();
        let mut current = start;
        for (index, step) in steps.iter().enumerate() {
            let Some(value) = current else {
                return Ok(None);
            };
            let holder = RefPath {
                root: path.root.clone(),
                steps: path.steps[..read_before + index].to_vec(),
            };
            current = self.step(&value, step, path, &holder, position)?;
        }
        Ok(current)
    }

    /// The key a step names, and whether the path says it is there.
    fn step_key(
        &self,
        step: &RefStep,
        path: &RefPath,
        position: &Position,
    ) -> Result<(String, bool), Diagnostic> {
        Ok(match step {
            RefStep::RequiredKey(key) => (key.clone(), true),
            RefStep::OptionalKey(key) => (key.clone(), false),
            RefStep::IndexedBy(index_path) => {
                let index = self.read(index_path, position)?.ok_or_else(|| {
                    Diagnostic::at(position, format!("`{path}`: `{index_path}` is absent"))
                })?;
                let key = index.scalar_text().ok_or_else(|| {
                    Diagnostic::at(
                        position,
                        format!("`{path}`: `{index_path}` holds no scalar to index by"),
                    )
                })?;
                (key, true)
            }
        })
    }

    fn step(
        &self,
        value: &Value,
        step: &RefStep,
        path: &RefPath,
        holder: &RefPath,
        position: &Position,
    ) -> Result<Option<Value>, Diagnostic> {
        let (key, required) = self.step_key(step, path, position)?;
        let value = value.local_value();
        let found = match &value.kind {
            ValueKind::Map(_) => value.get(&key).cloned(),
            ValueKind::List(items) => key
                .parse::<usize>()
                .ok()
                .and_then(|index| items.get(index).cloned()),
            _ => {
                return Err(Diagnostic::at(
                    position,
                    format!("`{path}`: `{key}` is read from a value that is not a map or a list"),
                ));
            }
        };
        if found.is_none() && required {
            let held: Vec<String> = value
                .entries()
                .unwrap_or_default()
                .iter()
                .map(|entry| entry.key.clone())
                .collect();
            return Err(absent_key(path, &holder.to_string(), &key, &held, position));
        }
        Ok(found)
    }
}
