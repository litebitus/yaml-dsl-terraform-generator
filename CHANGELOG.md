# Changelog

## Unreleased

- Schemas
  - A schema URL is read without credentials first, and with the machine's credentials only where it carries `yaml_dsl_machine_credentials=true` and that read is refused; a link to a GitHub release's `latest` or through `www.github.com` is read the same way, and a link no credential reads is reported as a missing release or asset, an unseen repository or a refused credential.

## v0.0.3 — 2026-11-01

- Generation
  - Reads `yaml-dsl.yml` up to v1.2, through yaml-dsl-compiler 0.1.2 and yaml-dsl-spec 0.1.4.
  - Validates a document by its DSL's `validator` block: `merge_base_keys`, `allow_quoted_values` and requirement markers read, and each problem worded as the YAML DSL editor words it.
  - A `tfgen.yml` problem is worded as a document's is: `` `key` is required ``, `` `key` is not a field of … ``, `` `version` must be one of … ``, `` `each` must be string ``.

## v0.0.2 — 2026-11-01

- Release
  - darwin binaries are built on macOS, linked against the macOS SDK.

## v0.0.1 — 2026-11-01

- Generation
  - `tfgen.yml` states its format, `version: "1"` or `"1.0"`; each release attaches its JSON Schema, every minor's and each major's as its newest minor.
  - Examples of both module layouts: one module at one pin, and a module per component.
  - Reads `yaml-dsl.yml` through yaml-dsl-spec: a definition is validated, every problem reported at its field.
  - Compiles each stack's documents by its DSL's generator, through templates, in the document's order.
  - Writes the templates a stack names, and the providers they require.
  - Deletes the files a stack no longer produces.
  - Finds a document's schema by its modeline, then the generator's `schema` list of paths, path patterns and URLs, and hands it to the compiler.
  - Reads private schema links with the machine's credentials: GitHub release assets with `GH_TOKEN`, `GITHUB_TOKEN` or the `gh` login, `s3://` objects through the AWS command line, as the role an `assume_role` parameter names where it names one, other hosts with `.netrc`.
- Command line
  - `tfgen` generates every stack of the repo; `--stack` names one, repeatable.
  - `--version` and `-v` print the release, or `dev` for a local build.
  - `tfgen completion bash | zsh | fish` prints the shell's completion script; `--stack` completes to the repo's stacks, `--schema` to its DSL ids, then paths.
  - Installs with Homebrew, `brew install litebitus/tap/tfgen`, with completion for each shell.
  - Lists each changed stack's files as `terramate generate` does, then how many of the stacks compared changed.
  - `--schema` and `TFGEN_SCHEMA` name a DSL's schema for one run, ahead of its documents' modelines.
  - Reports errors as a compiler does, with the source around them, in color on a terminal, divided to its width; `--color` and `--no-color` decide it.
  - Checks templates' `each` and `reference` refs when the config is read.
