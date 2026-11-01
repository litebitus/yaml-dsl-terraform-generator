module "{{each.scope_key}}_{{each.name}}" {
  source  = "registry.example.com/mock-org/components/mock//{{each.scope_key}}"
  version = local.modules_version
  config  = {{each.value}}
}
