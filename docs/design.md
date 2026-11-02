# tfgen

`tfgen` builds a YAML DSL over Terraform. A DSL's syntax is a block of `yaml-dsl.yml`, the config the [YAML DSL editor](https://github.com/litebitus/vscode-yaml-dsl-editor) reads; what it compiles to is `tfgen.yml` and the templates it names. `tfgen` reads `yaml-dsl.yml` with [yaml-dsl-spec](https://github.com/litebitus/yaml-dsl-spec), which returns each DSL's definition validated or every problem at its field, reads a stack's documents, folds, expands and resolves them with the `yaml-dsl-compiler` crate, and writes the stack's Terraform.

Nothing about a particular DSL is built into `tfgen`. Which keys declare what, what a reference becomes, which block a declaration compiles to: every rule is configuration.

## Why tfgen

### Where it comes from

`tfgen` is built for real needs, out of maintaining a Terraform estate as it grew. Plain Terraform kept growing in one shape: a root module of hundreds of files, every resource written out by hand in its author's own style, environments as data files beside the code, and a change anywhere planned against everything. The code stayed correct and became hard to read, to review and to change.

Each environment lived on a branch of its own, so a change reached the environments one merge at a time, and the branches drifted apart. Layers replace the branches: every environment is an overlay folded over one common layer on one branch, so what the environments share is written once, and what differs is stated where it differs.

A YAML DSL fixes the shape:

- Every stack is the same kind of document, in the same fields, checked against the same schema, so stacks are homogeneous and a coding standard holds across all of them.
- A pattern lives once, in the component a declaration names, and every stack that uses it reads the same way.
- A stack is small enough to own, plan and review alone.

### The problems it solves

| Problem | `tfgen` |
|---|---|
| An organization's infrastructure is a few patterns repeated in many stacks, and HCL writes each one out in full | a stack's author writes the organization's own language: one entry per resource, in the fields its components take |
| Environments differ in a few values, and copies drift | a common layer folds under each environment's overlay, and an overlay states only what differs |
| What a stack really receives is hidden behind templates or computed at plan time | the generated Terraform is committed, written out value by value, in the document's order, so a review sees exactly what Terraform gets |
| A Terraform editor completes a provider's raw resources and attributes, which change with every release, and tells the author nothing about what to use, which attributes go together, or what the organization's patterns are | a DSL is the organization's own, stable vocabulary: the editor reads the same `yaml-dsl.yml` and documents, completes and validates the patterns themselves |
| Generation slows as stacks are added | a run reads only the stacks it generates, compiles them in parallel, and never walks the repo |

### Beside the other tools

| Tool | What an author writes | What `tfgen` does differently |
|---|---|---|
| Terraform | HCL, its modules and variables | the author writes a DSL; `tfgen` writes the HCL, and Terraform runs it unchanged |
| Terramate | HCL templates and globals, evaluated by Terramate's own functions | a stack holds a document; the templates are plain HCL with values filled in, and the DSL's rules are configuration rather than template logic |
| Terragrunt | HCL configuration that assembles each module's Terraform when it runs | the Terraform is written into the repo before any run, so it is reviewed as code |

`tfgen` does one job: it writes code. Running stacks, choosing which, and in what order, is left to Terraform and to whatever runs on top of it.

### What it leaves out

`tfgen` takes Terramate's idea of generated code in stack directories and deliberately leaves the rest:

| Terramate feature | In `tfgen` |
|---|---|
| running stacks, by tag, in an `after` and `before` order | left to the tools that run Terraform |
| change detection and `watch` | every run generates every stack it is given; a file is written only where it changes |
| a template language with its own functions, `lets` and conditions | templates are HCL with values filled in; the one function, `fn.format`, computes a global |
| globals inherited down the directory tree | a stack's own globals, and `tfgen.yml`'s, read by refs that name their file |
| a stack block of metadata: name, id, tags | the descriptor's `globals` only; its other blocks belong to whatever else reads the file |

## Generated code

The generated code is read and reviewed, never edited:

- A value is written out as the document resolves it, so the generated file shows what every block receives.
- A one-line change to a document is a change of the same lines in the generated file. Output follows the document's order, and the same documents give the same bytes.
- Nothing is sorted. A block's attributes come in its template's order, and a value's keys in the document's, so the generated file reads in the order its author wrote.
- The style is the crate's HCL writer: what `terraform fmt` leaves unchanged.
- A generated file carries a header naming the document it comes from.

Nothing reads a document at plan time. Validation runs when `tfgen` generates, against the schema the document declares.

## Inputs

| Input | Holds |
|---|---|
| `yaml-dsl.yml` | each DSL's syntax: its files, layers, scopes, declarations and references |
| `tfgen.yml` | its format's `version`, `globals`, the `templates` a stack takes, and a generator per DSL |
| templates | HCL files `tfgen.yml` names |
| a stack descriptor | the stack's own values, as the globals of its descriptor file |
| documents | the stack's declarations |

`tfgen.yml` names its format first, as `yaml-dsl.yml` does:

```yaml
version: "1"
```

- `version` is required: `"1.0"` or `"1.1"`, or `"1"` for the newest 1.x tfgen reads; a version tfgen does not read is an error at the value.
- Its JSON Schema, `schemas/tfgen-v<version>.schema.json` for each minor, describes each field. Each release attaches every minor's, and each major's as its newest minor, for a modeline to name.

### Stacks

A stack is a directory holding a stack descriptor, `stack.tg.hcl`. `tfgen` reads the descriptor's `globals` block, literals only, and nothing else in it.

A repo moving from Terramate can also reuse Terramate's stack descriptor, by `tfgen.yml`'s `stacks.descriptors`. A stack moved from Terramate is best renamed to `stack.tg.hcl`, so its file says which tool owns it, and Terramate stops treating it as a stack. A stack that keeps `stack.tm.hcl` still works: `tfgen` reads the same `globals` block in either, and leaves the rest of Terramate's file to Terramate.

```yaml
stacks:
  descriptors: [stack.tg.hcl, stack.tm.hcl]
```

- `descriptors` lists the file names a stack's descriptor may have; left out, it is `[stack.tg.hcl]`.
- A directory holding two of them is an error naming both.

```hcl
globals {
  env = "dev"

  tfgen = {
    templates = {
      aws    = {}
      github = { owner = "mock-org" }
    }
    imports = ["../../dashboards/mock-*.yml"]
  }
}
```

The `tfgen` global is the stack's whole request of `tfgen`, a map so it can grow:

| Key | Holds |
|---|---|
| `templates` | the templates the stack takes, each with its settings |
| `imports` | paths and globs, relative to the stack, naming documents outside its directory that it compiles |

- A stack is `tfgen`'s when its globals hold `tfgen`.
- A stack's documents are those the DSL's `file_includes` select in the stack's directory, and those its `imports` name, each with the common layer the DSL's `layers` give it.

### Several DSLs

A repo holds as many DSLs as its configs declare, and `tfgen` reads them as the editor does:

- A `yaml-dsl.yml` declares any number of DSLs, and a repo may hold several configs. A document belongs to the config of the deepest folder holding it, and only that config's DSLs are matched against it.
- A `tfgen.yml` sits beside a `yaml-dsl.yml` and has the same folder scope: its generators, globals and templates serve the documents and stacks of that config's folder. A DSL with no generator is read by the editor alone.
- A document matching two DSLs is an error naming both.
- A stack takes every generator whose DSL claims one of its documents: a stack holding a `mock-dsl.yml` and a `mock-pipeline.yml` gets the files of both generators. Two generators writing the same file for one stack is an error, and so is a stack holding two documents of one DSL.
- The templates a stack takes are its own, whatever DSLs it holds.

## Generators

A generator compiles one DSL. `tfgen.yml` gives it by the DSL's id in `yaml-dsl.yml`:

| Field | Holds |
|---|---|
| `written_to` | the generated file the generator writes, in the stack's directory |
| `schema` | where the schema is when a document's modeline names none on disk: a path or a URL, as `schema_search_paths` writes one, with refs |
| `builtin_values` | by builtin of the DSL's logical scopes, the value it takes for a stack, as a ref |
| `data_files` | JSONPaths of fields whose values name files the stack reads |
| `values` | values computed once per stack, for templates to read |
| `blocks` | which template each part of the folded document is written through |
| `references` | what each reference of the DSL becomes |

### A worked example

The DSL, in `yaml-dsl.yml`:

```yaml
dsls:
  - id: mock-dsl
    file_includes: ["**/mock-dsl.yml"]
    validator:
      child_values_from_parent_key:
        - pattern: "^(?<field>name) (?<value>[a-z0-9-]+)$"
    declarations:
      - path: "$.*.*"
        skip_keys: [outputs]
        name_source: key
        key_token: last_word
        name_spelling: dashes_as_underscores
        scope_name: ENTRY
    references:
      - pattern: "^ref (?<type>[a-z_]+)\\.(?<name>[a-z0-9_]+)"
        positions: [whole_scalar]
        text_after_name_allowed: true
        scope_name: ENTRY
        scope_group: type
        name_group: name
```

A document, `mock-dsl.yml`:

```yaml
key:
  name mock-main:
    rotation: true

queue:
  name mock-orders:
    visibility: 30
    kms_key: ref key.mock_main.arn

outputs:
  orders_url: ref queue.mock_orders.url
```

The generator, in `tfgen.yml`:

```yaml
generators:
  mock-dsl:
    written_to: main.tf
    blocks:
      - template: templates/module.hcl
        each: declaration
        scope: ENTRY
      - template: templates/output.hcl
        each: entry
        path: "$.outputs.*"
    references:
      - scope: ENTRY
        template: "module.{{reference.scope_key}}_{{reference.name}}{{reference.path}}"
```

`templates/module.hcl`:

```hcl
module "{{each.scope_key}}_{{each.name}}" {
  source = "registry.example.com/mock/components//{{each.scope_key}}"
  config = {{each.value}}
}
```

`templates/output.hcl`:

```hcl
output "{{each.key}}" {
  value = {{each.value}}
}
```

The generated `main.tf`:

```hcl
module "key_mock_main" {
  source = "registry.example.com/mock/components//key"

  config = {
    name     = "mock-main"
    rotation = true
  }
}

module "queue_mock_orders" {
  source = "registry.example.com/mock/components//queue"

  config = {
    name       = "mock-orders"
    visibility = 30
    kms_key    = module.key_mock_main.arn
  }
}

output "orders_url" {
  value = module.queue_mock_orders.url
}
```

- `name` comes from the key `name mock-orders`, by the validator's `child_values_from_parent_key`, as the editor reads it.
- `kms_key` is a reference of scope `ENTRY`, written through the `references` template.

### Schema

`tfgen` finds and reads every document's schema and hands it to the compiler, which is pure code and reads none itself. A document is compiled against the schema the editor reads for it, found in this order:

1. the schema named for the run, by `--schema` or `TFGEN_SCHEMA`, for working on a schema locally;
2. its modeline, when the path it names is on disk, or when it names a URL;
3. the generator's `schema`, a list in `schema_search_paths`' grammar with refs filled for the stack, the first entry found taken;
4. the DSL's `schema_search_paths`, for a generator with no `schema`.

A path is relative to the document, or to the config's folder when it starts with `{config_root}`, and a search path's steps may hold `*`, matching within one step, the first file in path order taken. A modeline's path is the path it writes.

A URL is read by the YAML DSL standard's rules for schema URLs, [yaml-dsl-spec](https://github.com/litebitus/yaml-dsl-spec/blob/main/docs/spec.md) under Schema URLs: `yaml_dsl_max_age_days`, the check with `ETag` and `Last-Modified`, the kept copy when the source does not answer, and the read without credentials first, with the machine's credentials only where the link carries `yaml_dsl_machine_credentials=true` and that read is refused. Its copies are kept in the user's cache folder, `$XDG_CACHE_HOME/tfgen/schema-urls`, else `~/Library/Caches/tfgen/schema-urls` on macOS and `~/.cache/tfgen/schema-urls` elsewhere.

`tfgen` also reads an S3 object, `s3://<bucket>/<key>`, or `s3://<bucket>/<key>?assume_role=<role ARN>`, through the AWS command line, `aws s3api get-object`, always with the machine's AWS credentials: `AWS_PROFILE` and the other variables, `~/.aws` profiles with SSO, or a role. With `assume_role`, it reads as that role, assumed first with `aws sts assume-role`.

- A refused credential gives way to the next. Every credential refused before one is let in, or all of them with the kept copy read instead, is a warning naming where the credential came from and what renews it, such as `run aws sso login`. All refused with no kept copy is an error placed at the entry or modeline that named the link, with each refusal's reason and the fixes as its help; for a link `--schema` or `TFGEN_SCHEMA` names, the error names them.
- A GitHub token is asked for once per run, so a renewed login is read on the next.

A schema published with each release of the modules a stack pins takes two entries: the copy a stack's own `terraform init` keeps, and the release asset, for a stack not initialized yet:

```yaml
schema:
  - .terraform/modules/*/mock.schema.json
  - "https://github.com/mock-org/mock-modules/releases/download/v{{stack.modules_version}}/mock.schema.json"
```

An S3 bucket serves the same way, a key per release, read as a role where the machine's own identity is not admitted to the bucket:

```yaml
schema:
  - .terraform/modules/*/mock.schema.json
  - "s3://mock-schemas/mock-modules/{{stack.modules_version}}/mock.schema.json?assume_role={{tfgen.mock_role}}"
```

- A DSL's schema is either published at a URL or already on local disk when `tfgen` runs: `tfgen` downloads no module or package to find it.
- The schema holds the DSL's function table as well as its rules, so a document is never compiled without one.
- What reading a schema warns of, such as a credential refused, is reported once per run as a warning, and the stack is still generated.

### Blocks

Each entry of `blocks` writes one template for every part of the folded document it selects. The generated file follows the document, so a reader finds a declaration's block where the declaration stands:

- Blocks written for parts of the document come in the document's order, whichever entries of `blocks` write them.
- Blocks written once for the stack, such as a `locals` block, come after them, in the order of `blocks`.

| `each` | Selects | `each` in the template holds |
|---|---|---|
| `declaration` | every name declared in `scope` | `scope_key`, `name` as the declaration spells it, `value` |
| `scope_key` | every scope key of `scope`, a scope named by its parent key | `name`, `declarations`: each declaration under it, by name, with its value |
| `entry` | every entry the JSONPath `path` selects | `key`, `value` |
| left out | the stack, once | nothing; the template has no `each` |

A block narrows what it selects with `scope_keys`, `scope_keys_except` and `value_matches`, a pattern the value's text matches. A part is written by the first block that selects it, so a block for an exception goes before the general one:

```yaml
blocks:
  - template: templates/sensitive_output.hcl
    each: entry
    path: "$.outputs.*"
    value_matches: '^ref [a-z_]+\.[a-z0-9_]+\.(password|token)'
  - template: templates/output.hcl
    each: entry
    path: "$.outputs.*"
```

### References

Each entry of `references` is a template for the references of one scope, tried in order; the first that matches writes the reference. `scope_keys` and `path_matches`, a pattern the reference's path matches, narrow an entry:

```yaml
references:
  - scope: ENTRY
    scope_keys: [lookup]
    template: 'module.lookup.o["{{reference.name}}"]{{reference.path}}'
  - scope: ENTRY
    template: "module.{{reference.scope_key}}_{{reference.name}}{{reference.path}}"
```

With these rules, `ref lookup.mock_vpc.id` is written `module.lookup.o["mock_vpc"].id`, and `ref key.mock_main.arn` is written `module.key_mock_main.arn`.

### Values

`values` computes literals once per stack, for templates that need what the document leaves implicit:

```yaml
values:
  stack_regions:
    value: '{{fold["cloud"]["regions"]}}'
    default: ["{{tfgen[stack.env].region}}"]
  dependency_stacks:
    each: "$.lookup.*"
    where: { type: remote_state }
    defaults: { repo: mock-infra }
    value: { path: "{{each.stack_path}}", repo: "{{each.repo}}" }
```

| Form | Gives |
|---|---|
| `value`, `default` | the value, or the default where the value's optional path is absent |
| `each`, `where`, `defaults`, `value` | a list: for every entry the JSONPath `each` selects whose fields equal `where`, `value` with the entry under `defaults` |

For a stack with no `cloud` block and two lookups, one of `type: remote_state` naming no `repo`, these are `["mock-region-1"]` and `[{ path = "network/dev", repo = "mock-infra" }]`.

### Template roots

A template reads the refs of every root below. A root a rule provides exists only in that rule's template:

| Root | Provided by | Fields |
|---|---|---|
| `each` | a block with `each`, or a `values` entry with `each` | what the block selects, as in Blocks; the selected entry's fields for a `values` entry |
| `reference` | a `references` template | `scope_key`, `name`, `path`: the text after the name |
| `values` | a generator's templates | the names under its `values` |

A collection writes one block for all declarations of a scope key:

```hcl
module "{{each.name}}" {
  source = "registry.example.com/mock/components//{{each.name}}"
  config = {{each.declarations}}
}
```

```hcl
module "lookup" {
  source = "registry.example.com/mock/components//lookup"

  config = {
    mock_vpc = {
      type = "vpc"
      name = "mock-network"
    }
  }
}
```

### Module sources and versions

A module's source, version and inputs are what its template writes. `tfgen` gives none of them a meaning, so any layout of modules is configuration:

| Modules | Source and version | The template reads |
|---|---|---|
| one module holding every component, by path | the stack's one pin, a global of its descriptor | `{{stack.modules_version}}` |
| a module per component | a map by component type, in `tfgen.yml`, or in the descriptor for pins per stack | `{{tfgen.modules[each.scope_key].version}}` |

One module, one pin per stack:

```hcl
globals {
  modules_version = "1.2.3"
}
```

```hcl
module "{{each.scope_key}}_{{each.name}}" {
  source  = "registry.example.com/mock/components//{{each.scope_key}}"
  version = "{{stack.modules_version}}"
  config  = {{each.value}}
}
```

A module per component, each taking its inputs as variables:

```yaml
globals:
  modules:
    key:
      source: registry.example.com/mock/key/aws
      version: 2.0.1
    queue:
      source: registry.example.com/mock/queue/aws
      version: 1.4.0
```

```hcl
module "{{each.scope_key}}_{{each.name}}" {
  source  = "{{tfgen.modules[each.scope_key].source}}"
  version = "{{tfgen.modules[each.scope_key].version}}"

  {{each.value}}
}
```

```hcl
module "queue_mock_orders" {
  source     = "registry.example.com/mock/queue/aws"
  version    = "1.4.0"
  name       = "mock-orders"
  visibility = 30
}
```

- A type the map does not hold is an error naming it.
- Types whose modules differ in more than source and version get `blocks` entries of their own, selected by `scope_keys`.

### What a value becomes

| In the folded document | Written as |
|---|---|
| a scalar, list or map | itself |
| a reference | the `references` template that matches it |
| a builtin with a value, such as `${env}` | the value `builtin_values` gives it |
| a local | the `references` template of its scope, such as `local.{{reference.name}}{{reference.path}}`; the local itself is written by a block, like any part of the document |
| a call | a Terraform call of the same function, its arguments written the same way |

## Templates a stack takes

Beside a generator's templates, which the document selects, `tfgen.yml` declares templates a stack takes by name, such as a provider and the data sources that configure it. A stack names them in its `tfgen` global's `templates`, each with its settings for that template; `tfgen.yml` declares each one:

| Field | Holds |
|---|---|
| `file` | the template's file, relative to the config's folder |
| `written_to` | the generated file its blocks go to |
| `required_providers` | each provider the template requires, by local name, with its registry source |

```yaml
templates:
  github:
    file: templates/github.hcl
    written_to: providers.tf
    required_providers:
      github: integrations/github
```

```hcl
provider "github" {
  owner = "{{stack.tfgen.templates.github.owner}}"
}
```

- A template is written into every stack that names it, and into no other. Two stacks needing different blocks name two templates.
- The providers the named templates require are one map, `stack.__required_providers__`, which one template writes as the module's one `required_providers` block:

  ```hcl
  terraform {
    required_providers {
      {{stack.__required_providers__}}
    }
  }
  ```

## Refs

A ref, `{{<root>.<path>}}`, stands in a template, a global or a generator setting for a value `tfgen` reads:

| Root | Reads |
|---|---|
| `stack` | the globals of the stack's descriptor: `stack.env` |
| `fold` | the stack's folded document: `fold.cloud.regions` |
| `tfgen` | the globals of `tfgen.yml`: `tfgen[stack.env].region` |

- A path names keys as written. `[…]` holding another ref indexes by that ref's value.
- `.key` is a key the path says is there, and a miss is an error naming the path. `["key"]` is a key that may be absent, and a miss answers nothing.
- What no file holds about a stack is a builtin of the `stack` root, spelled `stack.__<name>__`:

  | Builtin | Holds |
  |---|---|
  | `stack.__path__` | the stack's directory from the config's folder |
  | `stack.__data_files__` | the files the stack reads: its documents, the files `data_files` name, and the files calls read |
  | `stack.__required_providers__` | each provider the stack's named templates require, by local name, with its `source` |

- A ref standing as a whole HCL value is written as that value. A ref inside a quoted string is written as text, and its value must be a scalar. A ref standing alone in a block's body holds a map, and is written as one attribute per entry:

  ```hcl
  locals {
    {{fold["locals"]}}
  }
  ```
- A template is HCL once its refs are written; `tfgen` writes them, then formats the file.

## Functions

A global may be a function call, computed once per stack:

```yaml
globals:
  session_name fn.format*: ["mock_%A", "{{stack.__path__}}"]
```

A function is called in a key, `<name> fn.<function>:` for one argument and `<name> fn.<function>*:` for a list of them. `fn.format` is the one function: a layout and its operands, with Go's printing verbs and these:

| Verb | Writes its operand |
|---|---|
| `%u` | upper-cased |
| `%l` | lower-cased |
| `%A` | with each `/` as `_` |
| `%B` | whole where it fits the precision, else `-` and its last characters, one fewer than the precision |

## Command line

```sh
tfgen                           # every stack of the repo holding the working directory, tracked or not
tfgen --stack services/mock-api # one stack, repeatable
tfgen --no-color                # no color, where the output is a terminal
tfgen --schema mock-dsl=../mock-modules/mock.schema.json  # a DSL's schema for this run, repeatable
TFGEN_SCHEMA=mock-dsl=../mock-modules/mock.schema.json tfgen  # the same, pairs separated by commas
tfgen --version                 # the release, or `dev` for a build that is none; -v the same
tfgen completion zsh            # the shell's completion script: bash, zsh or fish
```

- `tfgen` writes a file only where its contents change.
- A run lists each stack it changed, in path order, with the files it changed under it, marked as `terramate generate` marks them: `[+]` created, `[~]` changed, `[-]` deleted. It ends with how many of the stacks it compared changed, `1 of 141 stacks changed`; a stack with errors is not compared.
- A stack with an error gets no files, and the run exits non-zero. A stack's errors are reported in file order, then a line counting them.
- An error is reported as a compiler reports one:

  ```
  error: `brnch` is not a field of `publish`
    --> services/mock-api/mock-dsl.yml:57:5
     |
  52 | workflow:
  53 |   publish:
  ...
  56 |     branch: main
  57 |     brnch: main
     |     ^^^^^ `publish` has no field `brnch`
  58 |     triggers:
  59 |       push:
     |
     = rule: unevaluatedProperties, at mock.schema.json#/$defs/workflow/unevaluatedProperties
     = help: did you mean `branch`?
  ```

  The source shows two lines on each side of the wrong one, and above them each line it is nested under. An error is placed at the text that caused it: a document's, a template's, `tfgen.yml`'s or a stack descriptor's. A misspelt name carries the closest one the compiler suggests as its help.
- A template's refs to `each` and `reference` are checked when the config is read: each root is read only in the templates filled with it, and only for its fields.
- A report's lines divide to the terminal's width as the devops SDK lays a record out: nothing is cut, a line divides where the Unicode line breaking rules allow and after a machine string's separator, never inside a comparison, what wraps continues one level in, and where a row has no room its indentation gives first. A wrapped source line is underlined once, under the row the wrong text ends on, and a label that does not fit beside the underline takes its own rows under it. Output that is not a terminal is never divided.
- Color is on where the output is a terminal and `NO_COLOR` is unset; `--color` and `--no-color` decide it.
- `--schema` and `TFGEN_SCHEMA` name, by DSL id, the schema the run reads that DSL's documents against, ahead of their modelines; a flag wins over the variable for the DSL it names, and a path is read from the working directory. A document's modeline is never changed for it.
- A generated file a stack no longer produces is deleted, so a stack's files always match its documents. Only a file carrying `tfgen`'s header, in a stack the run generates, is ever deleted.
- Completion is the binary's: a script calls `tfgen __complete` with the words typed so far, and offers what it answers. What is typed is matched anywhere in a candidate, and in zsh in each part split at `:` or `/`. `--stack` completes to the repo's stacks whose globals hold `tfgen`, relative to the working directory. `--schema` completes to `<dsl id>=` for each DSL a generator compiles, then to the files and folders of the path typed after the `=`.

## Performance

A run's cost grows with the stacks it compiles and never with the size of the repo:

- `tfgen` never walks the repo. `--stack` reads the named stack's descriptor, its documents, the documents its `imports` name and the common layers its DSL's layers name. A run of every stack takes the stacks from git, `git ls-files --cached --others --exclude-standard`, so a stack counts before it is tracked and ignored paths are never entered.
- A document is parsed once, however many stacks read it, and a schema is read and compiled once per run.
- Stacks compile in parallel.

`tfgen.yml`'s `performance`, from format 1.1, tunes a run:

- `schema_cache_capacity` is how many schema URL copies the cache folder keeps, a whole number from 1, its default stated in the schema. After a run, the least recently used copies beyond it are dropped. A run reading several config folders keeps the largest capacity among them.

## Release

GoReleaser builds `tfgen` for darwin and linux, on arm64 and amd64, on a macOS runner so the darwin binaries link against the macOS SDK, and publishes a GitHub release with a Homebrew cask in `litebitus/homebrew-tap`. Each archive carries the completion scripts, generated from the binary being released, and the cask installs them.
