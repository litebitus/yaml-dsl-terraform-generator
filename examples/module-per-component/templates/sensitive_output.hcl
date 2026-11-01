output "{{each.key}}" {
  sensitive = true
  value     = {{each.value}}
}
