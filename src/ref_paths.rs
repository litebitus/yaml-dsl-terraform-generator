//! Refs, `{{<root>.<path>}}`: where they stand in a text and the path each names.

/// A ref's path: a root and the steps after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefPath {
    pub root: String,
    pub steps: Vec<RefStep>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefStep {
    /// `.key`: a key the path says is there.
    RequiredKey(String),
    /// `["key"]`: a key that may be absent.
    OptionalKey(String),
    /// `[<ref>]`: the key or index another ref's value names.
    IndexedBy(RefPath),
}

/// A ref in a text: its byte range, braces included, and its path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefSpan {
    pub start: usize,
    pub end: usize,
    pub path: RefPath,
}

const OPENER: &str = "{{";
const CLOSER: &str = "}}";

/// Every ref in `text`, in order.
pub fn find_refs(text: &str) -> Result<Vec<RefSpan>, String> {
    let mut spans = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find(OPENER) {
        let start = cursor + offset;
        let inner_start = start + OPENER.len();
        let inner_length = text[inner_start..]
            .find(CLOSER)
            .ok_or_else(|| format!("`{{{{` at byte {start} has no closing `}}}}`"))?;
        let inner = &text[inner_start..inner_start + inner_length];
        let end = inner_start + inner_length + CLOSER.len();
        spans.push(RefSpan {
            start,
            end,
            path: parse_path(inner.trim())?,
        });
        cursor = end;
    }
    Ok(spans)
}

/// The text with no refs, or the path of the one ref it is, whole.
pub fn whole_ref(text: &str) -> Result<Option<RefPath>, String> {
    let spans = find_refs(text)?;
    Ok(match spans.as_slice() {
        [span] if span.start == 0 && span.end == text.len() => Some(span.path.clone()),
        _ => None,
    })
}

pub fn parse_path(text: &str) -> Result<RefPath, String> {
    let mut parser = PathParser { text, position: 0 };
    let path = parser.path()?;
    if parser.position != text.len() {
        return Err(format!(
            "`{text}` is not a ref: unexpected `{}`",
            &text[parser.position..]
        ));
    }
    Ok(path)
}

struct PathParser<'t> {
    text: &'t str,
    position: usize,
}

impl PathParser<'_> {
    fn path(&mut self) -> Result<RefPath, String> {
        let root = self.name(is_root_character)?;
        let mut steps = Vec::new();
        loop {
            let rest = &self.text[self.position..];
            if rest.starts_with('.') {
                self.position += 1;
                steps.push(RefStep::RequiredKey(self.name(is_key_character)?));
            } else if let Some(quoted) = rest.strip_prefix("[\"") {
                let close = quoted
                    .find("\"]")
                    .ok_or_else(|| format!("`{}` has no closing `\"]`", self.text))?;
                steps.push(RefStep::OptionalKey(quoted[..close].to_owned()));
                self.position += 2 + close + 2;
            } else if rest.starts_with('[') {
                self.position += 1;
                let inner = self.path()?;
                if !self.text[self.position..].starts_with(']') {
                    return Err(format!("`{}` has no closing `]`", self.text));
                }
                self.position += 1;
                steps.push(RefStep::IndexedBy(inner));
            } else {
                return Ok(RefPath { root, steps });
            }
        }
    }

    fn name(&mut self, allowed: fn(char) -> bool) -> Result<String, String> {
        let rest = &self.text[self.position..];
        let length = rest
            .char_indices()
            .find(|(_, character)| !allowed(*character))
            .map_or(rest.len(), |(index, _)| index);
        if length == 0 {
            return Err(format!(
                "`{}` is not a ref: a name is expected at `{rest}`",
                self.text
            ));
        }
        self.position += length;
        Ok(rest[..length].to_owned())
    }
}

fn is_root_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

fn is_key_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_' || character == '-'
}

impl std::fmt::Display for RefPath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.root)?;
        for step in &self.steps {
            match step {
                RefStep::RequiredKey(key) => write!(formatter, ".{key}")?,
                RefStep::OptionalKey(key) => write!(formatter, "[\"{key}\"]")?,
                RefStep::IndexedBy(path) => write!(formatter, "[{path}]")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_reads_required_optional_and_indexed_steps() {
        let path = parse_path("tfgen[stack.env][\"region\"].name").unwrap();
        assert_eq!(path.root, "tfgen");
        assert_eq!(
            path.steps,
            [
                RefStep::IndexedBy(RefPath {
                    root: "stack".into(),
                    steps: vec![RefStep::RequiredKey("env".into())],
                }),
                RefStep::OptionalKey("region".into()),
                RefStep::RequiredKey("name".into()),
            ]
        );
        assert_eq!(path.to_string(), "tfgen[stack.env][\"region\"].name");
    }

    #[test]
    fn refs_are_found_in_text_and_a_lone_ref_is_whole() {
        let spans = find_refs("module.{{each.scope_key}}_{{each.name}}").unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!((spans[0].start, spans[0].end), (7, 25));
        assert!(whole_ref("{{fold[\"cloud\"]}}").unwrap().is_some());
        assert!(whole_ref("x{{fold.cloud}}").unwrap().is_none());
        assert!(find_refs("{{each.name").is_err());
        assert!(parse_path("each..name").is_err());
    }
}
