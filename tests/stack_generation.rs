use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DSL_CONFIG: &str = r#"dsls:
  - id: mock-dsl
    file_includes: ["**/mock-dsl.yml"]
    schema_search_paths: ["{config_root}/mock.schema.json"]
    layers:
      overlay_folders: [dev]
      common_layer_discovery: ancestor
    placeholder:
      pattern: "\\$\\{(?<body>[^}\\n]*)\\}"
      unscanned_paths: []
    locals: { scope_name: local, path_separator: "." }
    scopes:
      ENTRY: { regions: ["$"], named_by_parent_key: true }
      GLOBAL: { regions: ["$"], builtin_names: [env] }
      local: { regions: ["$"] }
    declarations:
      - path: "$.locals.*"
        name_source: key
        key_token: first_word
        name_spelling: as_written
        scope_name: local
      - path: "$.*.*"
        skip_keys: [locals, outputs]
        name_source: key
        key_token: last_word
        name_spelling: dashes_as_underscores
        scope_name: ENTRY
    references:
      - pattern: "^ref (?<type>[a-z]+)\\.(?<name>[a-z0-9_]+)"
        positions: [whole_scalar]
        text_after_name_allowed: true
        scope_name: ENTRY
        scope_group: type
        name_group: name
      - pattern: "^local\\.(?<name>[a-z_]+)"
        positions: [whole_scalar, whole_placeholder, placeholder_in_text]
        text_after_name_allowed: true
        scope_name: local
        name_group: name
      - pattern: "^(?<name>[a-z_]+)$"
        positions: [whole_placeholder, placeholder_in_text]
        text_after_name_allowed: false
        scope_name: GLOBAL
        name_group: name
    validator:
      child_values_from_parent_key:
        - pattern: "^(?<field>name) (?<value>[a-z0-9-]+)$"
"#;

const SCHEMA: &str = r#"{"type": "object"}"#;

const ONE_MODULE_CONFIG: &str = r#"globals:
  dev:
    region: mock-region-1
  full_session_name fn.format*: ["mock_%A", "{{stack.__path__}}"]
  session_name fn.format*: ["%.12B", "{{tfgen.full_session_name}}"]

generators:
  mock-dsl:
    written_to: main.tf
    builtin_values:
      env: "{{stack._env}}"
    values:
      regions:
        value: '{{fold["cloud"]["regions"]}}'
        default: ["{{tfgen[stack._env].region}}"]
    blocks:
      - template: templates/module.hcl
        each: declaration
        scope: ENTRY
      - template: templates/sensitive_output.hcl
        each: entry
        path: "$.outputs.*"
        value_matches: 'password$'
      - template: templates/output.hcl
        each: entry
        path: "$.outputs.*"
      - template: templates/locals.hcl
      - template: templates/stack_facts.hcl
    references:
      - scope: ENTRY
        template: "module.{{reference.scope_key}}_{{reference.name}}{{reference.path}}"
      - scope: local
        template: "local.{{reference.name}}{{reference.path}}"

templates:
  provider:
    file: templates/provider.hcl
    written_to: providers.tf
    required_providers:
      mock: example/mock
"#;

const ONE_MODULE_TEMPLATE: &str = r#"module "{{each.scope_key}}_{{each.name}}" {
  source  = "registry.example.com/mock/components//{{each.scope_key}}"
  version = "{{stack.modules_version}}"
  config  = {{each.value}}
}
"#;

const MODULE_PER_COMPONENT_CONFIG: &str = r#"globals:
  modules:
    queue:
      source: registry.example.com/mock/queue/aws
      version: 1.4.0

generators:
  mock-dsl:
    written_to: main.tf
    blocks:
      - template: templates/module.hcl
        each: declaration
        scope: ENTRY
    references:
      - scope: ENTRY
        template: "module.{{reference.scope_key}}_{{reference.name}}{{reference.path}}"
"#;

const MODULE_PER_COMPONENT_TEMPLATE: &str = r#"module "{{each.scope_key}}_{{each.name}}" {
  source  = "{{tfgen.modules[each.scope_key].source}}"
  version = "{{tfgen.modules[each.scope_key].version}}"

  {{each.value}}
}
"#;

/// A git working tree in the system's temporary folder, removed when dropped.
struct MockWorkspace {
    root: PathBuf,
}

impl MockWorkspace {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("tfgen-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let status = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success());
        Self { root }
    }

    fn write(&self, path: &str, contents: &str) {
        let file = self.root.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, contents).unwrap();
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.root.join(path)).unwrap()
    }

    fn exists(&self, path: &str) -> bool {
        self.root.join(path).exists()
    }

    fn run(&self, arguments: &[&str]) -> std::process::Output {
        self.run_with(arguments, &[])
    }

    fn run_with(&self, arguments: &[&str], variables: &[(&str, &str)]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_tfgen"))
            .args(arguments)
            .env_remove("TFGEN_SCHEMA")
            .envs(variables.iter().copied())
            .current_dir(&self.root)
            .output()
            .unwrap()
    }
}

impl Drop for MockWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn one_module_workspace(workspace: &MockWorkspace) {
    workspace.write("yaml-dsl.yml", DSL_CONFIG);
    workspace.write("mock.schema.json", SCHEMA);
    workspace.write("tfgen.yml", ONE_MODULE_CONFIG);
    workspace.write("templates/module.hcl", ONE_MODULE_TEMPLATE);
    workspace.write(
        "templates/output.hcl",
        "output \"{{each.key}}\" {\n  value = {{each.value}}\n}\n",
    );
    workspace.write(
        "templates/sensitive_output.hcl",
        "output \"{{each.key}}\" {\n  sensitive = true\n  value     = {{each.value}}\n}\n",
    );
    workspace.write(
        "templates/locals.hcl",
        "locals {\n  {{fold[\"locals\"]}}\n}\n",
    );
    workspace.write(
        "templates/stack_facts.hcl",
        concat!(
            "locals {\n",
            "  data_files = {{stack.__data_files__}}\n",
            "  regions    = {{values.regions}}\n",
            "}\n",
        ),
    );
    workspace.write(
        "templates/provider.hcl",
        concat!(
            "provider \"mock\" {\n",
            "  region  = \"{{tfgen[stack._env].region}}\"\n",
            "  session = \"{{tfgen.session_name}}\"\n",
            "}\n\n",
            "terraform {\n",
            "  required_providers {\n",
            "    {{stack.__required_providers__}}\n",
            "  }\n",
            "}\n",
        ),
    );
    workspace.write(
        "stacks/mock-dsl.yml",
        concat!(
            "queue:\n  name mock-orders:\n    visibility: 30\n    kms_key: ref key.mock_main.arn\n",
            "key:\n  name mock-main:\n    rotation: true\n",
        ),
    );
    workspace.write(
        "stacks/dev/mock-dsl.yml",
        concat!(
            "locals:\n  team: mock-team\n",
            "queue:\n  name mock-orders:\n    visibility: 60\n",
            "    tags: { env: \"${env}\", team: local.team }\n",
            "outputs:\n  orders_url: ref queue.mock_orders.url\n",
            "  orders_password: ref queue.mock_orders.password\n",
        ),
    );
    workspace.write(
        "stacks/dev/stack.tg.hcl",
        concat!(
            "globals {\n",
            "  _env            = \"dev\"\n",
            "  modules_version = \"1.2.3\"\n",
            "  tfgen           = { templates = { provider = {} } }\n",
            "}\n",
        ),
    );
    workspace.write(
        "stacks/dev/old.tf",
        "# DO NOT EDIT. Generated by tfgen from stacks/dev/mock-dsl.yml.\n\nlocals {}\n",
    );
    workspace.write(
        "stacks/dev/migration.tf",
        "moved {\n  from = a.b\n  to   = a.c\n}\n",
    );
    workspace.write(
        "terramate-stack/stack.tg.hcl",
        "globals {\n  _env = \"dev\"\n}\n",
    );
}

fn module_per_component_workspace(workspace: &MockWorkspace) {
    workspace.write("per-component/yaml-dsl.yml", DSL_CONFIG);
    workspace.write("per-component/mock.schema.json", SCHEMA);
    workspace.write("per-component/tfgen.yml", MODULE_PER_COMPONENT_CONFIG);
    workspace.write(
        "per-component/templates/module.hcl",
        MODULE_PER_COMPONENT_TEMPLATE,
    );
    workspace.write(
        "per-component/queue-stack/mock-dsl.yml",
        "queue:\n  name mock-orders:\n    visibility: 30\n",
    );
    workspace.write(
        "per-component/queue-stack/stack.tg.hcl",
        "globals {\n  _env  = \"dev\"\n  tfgen = {}\n}\n",
    );
}

const ONE_MODULE_MAIN: &str = r#"# DO NOT EDIT. Generated by tfgen from stacks/{dev/mock-dsl.yml, mock-dsl.yml}.

module "queue_mock_orders" {
  source  = "registry.example.com/mock/components//queue"
  version = "1.2.3"

  config = {
    name       = "mock-orders"
    visibility = 60
    kms_key    = module.key_mock_main.arn
    tags       = { env = "dev", team = local.team }
  }
}

module "key_mock_main" {
  source  = "registry.example.com/mock/components//key"
  version = "1.2.3"
  config  = { name = "mock-main", rotation = true }
}

output "orders_url" {
  value = module.queue_mock_orders.url
}

output "orders_password" {
  sensitive = true
  value     = module.queue_mock_orders.password
}

locals {
  team = "mock-team"
}

locals {
  data_files = ["../mock-dsl.yml", "./mock-dsl.yml"]
  regions    = ["mock-region-1"]
}
"#;

const ONE_MODULE_PROVIDERS: &str = r#"# DO NOT EDIT. Generated by tfgen from tfgen.yml.

provider "mock" {
  region  = "mock-region-1"
  session = "-_stacks_dev"
}

terraform {
  required_providers {
    mock = { source = "example/mock" }
  }
}
"#;

const MODULE_PER_COMPONENT_MAIN: &str = r#"# DO NOT EDIT. Generated by tfgen from per-component/queue-stack/mock-dsl.yml.

module "queue_mock_orders" {
  source     = "registry.example.com/mock/queue/aws"
  version    = "1.4.0"
  name       = "mock-orders"
  visibility = 30
}
"#;

#[test]
fn every_stack_of_the_repo_is_generated_in_both_module_layouts() {
    let workspace = MockWorkspace::new("whole-repo");
    one_module_workspace(&workspace);
    module_per_component_workspace(&workspace);
    let output = workspace.run(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(listed.contains("stacks/dev\n  [+] main.tf\n"), "{listed}");
    assert!(listed.contains("  [-] old.tf\n"), "{listed}");
    assert!(listed.ends_with("2 of 2 stacks changed\n"), "{listed}");
    assert_eq!(workspace.read("stacks/dev/main.tf"), ONE_MODULE_MAIN);
    assert_eq!(
        workspace.read("stacks/dev/providers.tf"),
        ONE_MODULE_PROVIDERS
    );
    assert_eq!(
        workspace.read("per-component/queue-stack/main.tf"),
        MODULE_PER_COMPONENT_MAIN
    );
    assert!(!workspace.exists("stacks/dev/old.tf"));
    assert!(workspace.exists("stacks/dev/migration.tf"));
    assert!(!workspace.exists("terramate-stack/main.tf"));
    let second = workspace.run(&[]);
    assert!(second.status.success());
    assert_eq!(
        String::from_utf8_lossy(&second.stdout),
        "0 of 2 stacks changed\n"
    );
}

#[test]
fn a_named_stack_alone_is_generated() {
    let workspace = MockWorkspace::new("named-stack");
    one_module_workspace(&workspace);
    module_per_component_workspace(&workspace);
    let output = workspace.run(&["--stack", "per-component/queue-stack"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(workspace.exists("per-component/queue-stack/main.tf"));
    assert!(!workspace.exists("stacks/dev/main.tf"));
}

#[test]
fn a_generator_schema_is_the_fallback_when_the_modeline_names_none() {
    let workspace = MockWorkspace::new("generator-schema");
    module_per_component_workspace(&workspace);
    workspace.write(
        "per-component/yaml-dsl.yml",
        &DSL_CONFIG.replace(
            "schema_search_paths: [\"{config_root}/mock.schema.json\"]",
            "schema_search_paths: []",
        ),
    );
    workspace.write(
        "per-component/tfgen.yml",
        &MODULE_PER_COMPONENT_CONFIG.replace(
            "    written_to: main.tf\n",
            concat!(
                "    written_to: main.tf\n",
                "    schema:\n",
                "      - .terraform/modules/*/mock.schema.json\n",
                "      - \"{config_root}/schemas/{{stack._env}}.schema.json\"\n",
            ),
        ),
    );
    let missing = workspace.run(&[]);
    assert!(!missing.status.success());
    let errors = String::from_utf8_lossy(&missing.stderr);
    assert!(errors.contains("no schema found"), "{errors}");
    workspace.write(
        "per-component/queue-stack/.terraform/modules/queue_mock_orders/mock.schema.json",
        r#"{"type": "object", "required": ["mock_initialized"]}"#,
    );
    let initialized = workspace.run(&[]);
    let errors = String::from_utf8_lossy(&initialized.stderr);
    assert!(
        errors.contains("field `mock_initialized` is required"),
        "{errors}"
    );
    workspace.write(
        "per-component/queue-stack/.terraform/modules/queue_mock_orders/mock.schema.json",
        SCHEMA,
    );
    let output = workspace.run(&[]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        workspace.read("per-component/queue-stack/main.tf"),
        MODULE_PER_COMPONENT_MAIN
    );
}

#[test]
fn a_schema_named_for_the_run_goes_ahead_of_the_modeline() {
    let workspace = MockWorkspace::new("schema-override");
    module_per_component_workspace(&workspace);
    workspace.write(
        "per-component/queue-stack/mock-dsl.yml",
        concat!(
            "# yaml-language-server: $schema=../mock.schema.json\n",
            "queue:\n  name mock-orders:\n    visibility: 30\n",
        ),
    );
    workspace.write(
        "local/changed.schema.json",
        r#"{"type": "object", "required": ["mock_changed"]}"#,
    );
    let flagged = workspace.run(&["--schema", "mock-dsl=local/changed.schema.json"]);
    let errors = String::from_utf8_lossy(&flagged.stderr);
    assert!(
        errors.contains("field `mock_changed` is required"),
        "{errors}"
    );
    let variable = workspace.run_with(
        &[],
        &[("TFGEN_SCHEMA", "mock-dsl=local/changed.schema.json")],
    );
    let errors = String::from_utf8_lossy(&variable.stderr);
    assert!(
        errors.contains("field `mock_changed` is required"),
        "{errors}"
    );
    let flag_wins = workspace.run_with(
        &["--schema", "mock-dsl=per-component/mock.schema.json"],
        &[("TFGEN_SCHEMA", "mock-dsl=local/changed.schema.json")],
    );
    assert!(
        flag_wins.status.success(),
        "{}",
        String::from_utf8_lossy(&flag_wins.stderr)
    );
}

#[test]
fn an_error_names_its_place_and_its_stack_gets_no_files() {
    let workspace = MockWorkspace::new("stack-error");
    module_per_component_workspace(&workspace);
    workspace.write(
        "per-component/queue-stack/mock-dsl.yml",
        "topic:\n  name mock-events:\n    retention: 7\n",
    );
    let output = workspace.run(&[]);
    assert!(!output.status.success());
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("module.hcl:2:"), "{errors}");
    assert!(errors.contains("tfgen.modules[each.scope_key]"), "{errors}");
    assert!(!Path::new(&workspace.root.join("per-component/queue-stack/main.tf")).exists());
}

#[test]
fn a_config_error_is_reported_at_its_file_with_the_closest_name() {
    let workspace = MockWorkspace::new("config-error");
    one_module_workspace(&workspace);
    workspace.write(
        "tfgen.yml",
        &ONE_MODULE_CONFIG.replacen(
            "    written_to: main.tf\n",
            "    writen_to: main.tf\n    written_to: main.tf\n",
            1,
        ),
    );
    let output = workspace.run(&["--no-color"]);
    assert!(!output.status.success());
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("--> tfgen.yml:9:5"), "{errors}");
    assert!(errors.contains("did you mean `written_to`?"), "{errors}");
    assert!(
        errors.contains("tfgen.yml holds an error, so no stack it configures was generated"),
        "{errors}"
    );
    assert!(!workspace.exists("stacks/dev/main.tf"));
}

#[test]
fn a_template_reading_a_field_its_root_lacks_fails_when_the_config_is_read() {
    let workspace = MockWorkspace::new("template-field");
    one_module_workspace(&workspace);
    workspace.write(
        "tfgen.yml",
        &ONE_MODULE_CONFIG.replacen("{{reference.name}}", "{{reference.nam}}", 1),
    );
    let output = workspace.run(&["--no-color"]);
    assert!(!output.status.success());
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("`reference` holds no `nam`"), "{errors}");
    assert!(errors.contains("did you mean `name`?"), "{errors}");
    assert!(!workspace.exists("stacks/dev/main.tf"));
}
