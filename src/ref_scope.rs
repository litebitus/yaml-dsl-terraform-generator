//! What a ref reads: the roots a stack, a generator and a template rule provide,
//! and the walk down a ref's path.

use std::cell::RefCell;
use std::collections::BTreeMap;

use yaml_dsl_compiler::resolved::{Value, ValueKind};
use yaml_dsl_compiler::{Diagnostic, Node, Position};

use crate::config_files::ValueRule;
use crate::ref_paths::{RefPath, RefStep};
use crate::yaml_values::{evaluate_node, evaluate_value_rule};

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

    /// The value `path` names, or `None` where an optional key is absent.
    pub fn read(&self, path: &RefPath, position: &Position) -> Result<Option<Value>, Diagnostic> {
        let missing_root = || {
            Diagnostic::at(
                position,
                format!("`{path}`: no root `{}` is read here", path.root),
            )
        };
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
                let value = if path.root == "tfgen" {
                    self.stack.global(&name, position)?
                } else {
                    let generator = self.generator.ok_or_else(missing_root)?;
                    generator.value(&name, position, self.stack)?
                };
                if value.is_none() && required {
                    return Err(Diagnostic::at(
                        position,
                        format!("`{path}`: {} holds no `{name}`", path.root),
                    ));
                }
                (value, rest)
            }
            _ => return Err(missing_root()),
        };
        let mut current = start;
        for step in steps {
            let Some(value) = current else {
                return Ok(None);
            };
            current = self.step(&value, step, path, position)?;
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
            return Err(Diagnostic::at(
                position,
                format!("`{path}`: no `{key}` is there"),
            ));
        }
        Ok(found)
    }
}
