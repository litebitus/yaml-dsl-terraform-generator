# `tfgen.yml` schema changelog

Each version is the format a `tfgen.yml` names in its `version`, and the schema file of that name. A version not yet released is headed `## <version> — unreleased`; the release workflow stamps it with the tfgen release that ships it and the date.

## 1.4 — tfgen v0.1.0, 2026-11-10

- A generator takes `validation_fallback_to_remote_schemas`, default `false`.

## 1.3 — tfgen v0.0.8, 2026-11-06

- The schema publishes `fn.format` in its `x-yaml-dsl-functions` table, for the YAML DSL editor to check its calls.

## 1.2 — tfgen v0.0.7, 2026-11-06

- A value takes `first: declaration`: its `value` written for the first declaration of `scope` under `scope_keys`, in the document's order, read as `each`, else its `default`.

## 1.1 — tfgen v0.0.5, 2026-11-02

- `performance.schema_cache_capacity`: how many schemas the schema cache keeps, the least recently used dropped first.

## 1.0 — tfgen v0.0.1, 2026-11-01

- The first format: `stacks`, `globals`, `generators` and `templates`.
