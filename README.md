# tfgen

A YAML DSL over Terraform. A stack's author writes documents in the organization's own language; `tfgen` folds, expands and resolves them with [yaml-dsl-compiler](https://github.com/litebitus/yaml-dsl-compiler) and writes the stack's Terraform, committed and reviewed as code.

## Features

- Nothing about a DSL is built in: its syntax is a block of the `yaml-dsl.yml` the [YAML DSL editor](https://github.com/litebitus/vscode-yaml-dsl-editor) reads, and what it compiles to is `tfgen.yml` and plain HCL templates.
- Layers: an environment's overlay folds over one common layer.
- Generated code reads in the document's order, value by value, in one style `terraform fmt` leaves unchanged.
- Errors name their file, line and column; a stack with an error gets no files.
- A run never walks the repo: stacks come from git, and they generate in parallel.

## Examples

| Layout | Example |
|---|---|
| Every component in one module, at one pin | [`examples/one-module/`](examples/one-module) |
| A module per component, each with its own source and version | [`examples/module-per-component/`](examples/module-per-component) |

## Configuration

| File | Holds |
|---|---|
| `yaml-dsl.yml` | each DSL's syntax |
| `tfgen.yml` | globals, the templates a stack takes, and a generator per DSL |
| `stack.tg.hcl` | a stack's own values, in its `globals` |

`tfgen.yml`'s JSON Schemas are in [schemas/](schemas), one per format version, each field described there.

## Usage

```sh
tfgen                           # every stack of the repo
tfgen --stack services/mock-api # one stack, repeatable
tfgen --no-color                # no color, where the output is a terminal
tfgen --schema mock-dsl=../mock-modules/mock.schema.json  # a schema being changed locally
tfgen --version                 # the release, or `dev` for a local build; -v the same
```

## Install

```sh
brew install litebitus/tap/tfgen
```

The cask installs completion for bash, zsh and fish. For zsh, in `~/.zshrc`:

```sh
fpath=(/opt/homebrew/share/zsh/site-functions $fpath)
autoload -Uz compinit && compinit
zstyle ':completion:*' rehash true
```

`tfgen completion bash | zsh | fish` prints the script for any other install. `--stack` completes to the repo's stacks, and `--schema` to its DSL ids, then to paths.

## Versioning

A schema and tfgen carry separate versions. A schema's version is the version of `tfgen.yml`'s format, and a file states it in its `version` field. tfgen's version is the release.

| Change | Schema version | tfgen version |
|---|---|---|
| a breaking format change | a new major, `v2.0` | a new major |
| an additive format change, such as a new optional field | a new minor, `v1.1` | a new minor |
| tfgen's code only | none | a new patch |

- A minor's schema is a file of its own, frozen once released: `tfgen-v1.0.schema.json`, then `tfgen-v1.1.schema.json` beside it.
- A major floats to its newest minor. `version: "1"` in a `tfgen.yml` is read as the newest 1.x tfgen knows; `version: "1.0"` as 1.0.
- Each release attaches every minor's schema, and each major's as its newest minor, for a modeline to name.

### Modelines

The newest 1.x, from the latest release, so the modeline never needs editing:

```yaml
# yaml-language-server: $schema=https://github.com/litebitus/yaml-dsl-terraform-generator/releases/latest/download/tfgen-v1.schema.json
version: "1"
```

Exactly 1.0, from a fixed release, which never changes, so its copy is kept a year:

```yaml
# yaml-language-server: $schema=https://github.com/litebitus/yaml-dsl-terraform-generator/releases/download/v0.1.0/tfgen-v1.0.schema.json?yaml_dsl_max_age_days=365
version: "1.0"
```

## Development

| Target | Does |
|---|---|
| `make test` | format check, clippy with warnings as errors, every test |
| `make build` | `make test`, then a release build |
| `make completion` | `make build`, then installs its zsh completion into `~/.zsh/completions` (`COMPLETION_DIR` to override), for `tfgen` and `./target/release/tfgen` |
| `make completion-clean` | removes it, so a brew-installed tfgen's completion takes over |

The `Release` workflow is dispatched by hand: it stamps `CHANGELOG.md` and the version, tags, publishes binaries for darwin and linux, arm64 and amd64, to a GitHub release, and pushes the cask to `litebitus/homebrew-tap` with the `HOMEBREW_TAP_TOKEN` secret.

The design is in [docs/design.md](docs/design.md).
