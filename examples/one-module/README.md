# One module

Every component is a folder of one module, pinned once per stack. `stacks/mock-dsl.yml` is the common layer and `stacks/dev/mock-dsl.yml` the dev overlay folded over it; `stacks/dev/stack.tg.hcl` is the stack's descriptor, its `modules_version` the pin. `main.tf` and `providers.tf` are what `tfgen --stack examples/one-module/stacks/dev` writes.

| Where | What tfgen writes |
|---|---|
| `key` and `queue` in the documents | a module per declaration, `module "<type>_<name>"`, its `source` the component's folder of the one module and its `config` the folded declaration |
| `visibility_seconds: 60` in the overlay | wins over the common layer's `30` |
| `kms_key: ref key.mock_main.arn` | the reference rule's `module.key_mock_main.arn` |
| `${env}` and `local.team` in `tags` | `"dev"`, from the `env` builtin, and `local.team` |
| `outputs` | an output per entry; `orders_password` matches `value_matches: "password$"`, so its output is sensitive |
| `templates/modules_version.hcl` | `local.modules_version`, the descriptor's pin, which every module's `version` reads |
| `tfgen.templates` in the descriptor | `providers.tf` from the `provider` template, with the provider it requires |
