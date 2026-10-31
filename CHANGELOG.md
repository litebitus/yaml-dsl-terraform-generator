# Changelog

## Unreleased

- Generation
  - Compiles each stack's documents by its DSL's generator, through templates, in the document's order.
  - Writes the templates a stack names, and the providers they require.
  - Deletes the files a stack no longer produces.
- Command line
  - `tfgen` generates every stack of the repo; `--stack` names one, repeatable.
