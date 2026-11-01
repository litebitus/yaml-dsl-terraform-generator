# Module per component

Each component is its own module, with its own source and version, kept in `tfgen.yml`'s `modules` global. The documents are those of [`one-module`](../one-module); only the module template and its pins differ. `main.tf` and `providers.tf` are what `tfgen --stack examples/module-per-component/stacks/dev` writes.

| Where | What tfgen writes |
|---|---|
| `key` and `queue` in the documents | a module per declaration, its `source` and `version` read from `tfgen.modules[each.scope_key]` |
| `{{each.value}}` alone on its line in `templates/module.hcl` | the declaration's fields as the module's arguments, in the document's order |
| `kms_key: ref key.mock_main.arn` | the reference rule's `module.key_mock_main.arn` |
| `outputs` | an output per entry; `orders_password` matches `value_matches: "password$"`, so its output is sensitive |
| `tfgen.templates` in the descriptor | `providers.tf` from the `provider` template, with the provider it requires |
