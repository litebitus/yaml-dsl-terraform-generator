# Changelog

## Unreleased

- Generation
  - Compiles each stack's documents by its DSL's generator, through templates, in the document's order.
  - Writes the templates a stack names, and the providers they require.
  - Deletes the files a stack no longer produces.
  - Finds a document's schema by its modeline, then the generator's `schema` list of paths, path patterns and URLs, and hands it to the compiler.
  - Reads private schema links with the machine's credentials: GitHub release assets with `GH_TOKEN`, `GITHUB_TOKEN` or the `gh` login, `s3://` objects through the AWS command line, as the role an `assume_role` parameter names where it names one, other hosts with `.netrc`.
- Command line
  - `tfgen` generates every stack of the repo; `--stack` names one, repeatable.
  - `--schema` and `TFGEN_SCHEMA` name a DSL's schema for one run, ahead of its documents' modelines.
  - Reports errors as a compiler does, with the source around them, in color on a terminal, divided to its width; `--color` and `--no-color` decide it.
  - Checks templates' `each` and `reference` refs when the config is read.
