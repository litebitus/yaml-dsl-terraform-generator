module "{{each.scope_key}}_{{each.name}}" {
  source  = "{{tfgen.modules[each.scope_key].source}}"
  version = "{{tfgen.modules[each.scope_key].version}}"

  {{each.value}}
}
