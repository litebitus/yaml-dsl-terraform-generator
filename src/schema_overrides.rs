//! Schemas named for one run, by DSL, ahead of every document's modeline: from
//! `TFGEN_SCHEMA`, then `--schema`, a flag winning for the DSL it names.

use std::collections::BTreeMap;

pub const SCHEMA_VARIABLE: &str = "TFGEN_SCHEMA";
const PAIR_SEPARATOR: char = ',';

/// By DSL id, the schema a run reads that DSL's documents against: a path, read
/// from the working directory, or a URL.
pub type SchemaOverrides = BTreeMap<String, String>;

/// A `<dsl id>=<path or URL>` pair.
pub fn schema_pair(text: &str) -> Result<(String, String), String> {
    match text.split_once('=') {
        Some((dsl_id, schema)) if !dsl_id.trim().is_empty() && !schema.trim().is_empty() => {
            Ok((dsl_id.trim().to_owned(), schema.trim().to_owned()))
        }
        _ => Err(format!(
            "`{text}` names no schema: it is `<dsl id>=<path or URL>`"
        )),
    }
}

/// The pairs `TFGEN_SCHEMA` holds, separated by commas.
pub fn variable_pairs(value: &str) -> Result<Vec<(String, String)>, String> {
    value
        .split(PAIR_SEPARATOR)
        .filter(|pair| !pair.trim().is_empty())
        .map(schema_pair)
        .collect::<Result<_, _>>()
        .map_err(|message| format!("{SCHEMA_VARIABLE}: {message}"))
}

/// The run's schemas, the flags' pairs over the variable's.
pub fn schema_overrides(
    variable: &[(String, String)],
    flags: &[(String, String)],
) -> SchemaOverrides {
    variable.iter().chain(flags).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(dsl_id: &str, schema: &str) -> (String, String) {
        (dsl_id.to_owned(), schema.to_owned())
    }

    #[test]
    fn a_flag_wins_over_the_variable() {
        let variable = variable_pairs(
            "mock-dsl=../schemas/a.json, other=https://mock.example/b.json, \
             bucket=s3://mock-bucket/c.json",
        )
        .unwrap();
        let flags = vec![pair("mock-dsl", "c.json")];
        let overrides = schema_overrides(&variable, &flags);
        assert_eq!(
            overrides,
            SchemaOverrides::from([
                pair("bucket", "s3://mock-bucket/c.json"),
                pair("mock-dsl", "c.json"),
                pair("other", "https://mock.example/b.json"),
            ])
        );
    }

    #[test]
    fn a_pair_with_no_dsl_or_no_schema_is_an_error() {
        assert!(schema_pair("mock.json").is_err());
        assert!(schema_pair("mock-dsl=").is_err());
        assert!(
            variable_pairs("mock-dsl")
                .unwrap_err()
                .starts_with(SCHEMA_VARIABLE)
        );
    }
}
