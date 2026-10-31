# tfgen

A YAML DSL over Terraform. A stack's author writes documents in the organization's own language; `tfgen` folds, expands and resolves them with [yaml-dsl-compiler](https://github.com/litebitus/yaml-dsl-compiler) and writes the stack's Terraform, committed and reviewed as code.

## Features

- Nothing about a DSL is built in: its syntax is a block of the `yaml-dsl.yml` the [YAML DSL editor](https://github.com/litebitus/vscode-yaml-dsl-editor) reads, and what it compiles to is `tfgen.yml` and plain HCL templates.
- Layers: an environment's overlay folds over one common layer.
- Generated code reads in the document's order, value by value, in one style `terraform fmt` leaves unchanged.
- Errors name their file, line and column; a stack with an error gets no files.
- A run never walks the repo: stacks come from git, and they generate in parallel.

## Configuration

| File | Holds |
|---|---|
| `yaml-dsl.yml` | each DSL's syntax |
| `tfgen.yml` | globals, the templates a stack takes, and a generator per DSL |
| `stack.tg.hcl` | a stack's own values, in its `globals` |

## Usage

```sh
tfgen                           # every stack of the repo
tfgen --stack services/mock-api # one stack, repeatable
```

## Development

| Target | Does |
|---|---|
| `make test` | format check, clippy with warnings as errors, every test |
| `make build` | `make test`, then a release build |

The `Release` workflow is dispatched by hand: it stamps `CHANGELOG.md` and the version, tags, and publishes binaries for darwin and linux, arm64 and amd64, to a GitHub release.

The design is in [docs/design.md](docs/design.md).
