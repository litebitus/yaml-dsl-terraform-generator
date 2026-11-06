use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const DSL_CONFIG: &str = r#"version: "1"
dsls:
  - id: mock-dsl
    file_includes: ["**/mock-dsl.yml"]
    file_excludes: []
    schema_search_paths: ["{config_root}/mock.schema.json"]
    layers:
      overlay_folders: [dev]
      common_layer_discovery: ancestor
      duplicate_check: { depth: 2, key_depths: {}, skip_keys: [locals] }
    placeholder:
      pattern: "\\$\\{(?<body>[^}\\n]*)\\}"
      unscanned_paths: []
    locals: { scope_name: local, path_separator: "." }
    key_sort_orders: []
    scopes:
      ENTRY:
        regions: ["$"]
        later_items_of_declaring_list: false
        named_by_parent_key: true
        builtin_names: []
      GLOBAL:
        regions: ["$"]
        later_items_of_declaring_list: false
        named_by_parent_key: false
        builtin_names: [env]
      local:
        regions: ["$"]
        later_items_of_declaring_list: false
        named_by_parent_key: false
        builtin_names: []
    declarations:
      - path: "$.locals.*"
        skip_keys: []
        exclude_candidates: []
        name_source: key
        key_token: first_word
        name_spelling: as_written
        meta_argument_name: null
        declares_every_name: false
        scope_name: local
      - path: "$.*.*"
        skip_keys: [locals, outputs]
        exclude_candidates: []
        name_source: key
        key_token: last_word
        name_spelling: dashes_as_underscores
        meta_argument_name: null
        declares_every_name: false
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
        scope_group: null
        name_group: name
      - pattern: "^(?<name>[a-z_]+)$"
        positions: [whole_placeholder, placeholder_in_text]
        text_after_name_allowed: false
        scope_name: GLOBAL
        scope_group: null
        name_group: name
    validator:
      child_values_from_parent_key:
        - pattern: "^(?<field>name) (?<value>[a-z0-9-]+)$"
"#;

const SCHEMA: &str = r#"{"type": "object"}"#;

const ONE_MODULE_CONFIG: &str = r#"version: "1"

globals:
  dev:
    region: mock-region-1
  full_session_name fn.format*: ["mock_%A", "{{stack.__path__}}"]
  session_name fn.format*: ["%.12B", "{{tfgen.full_session_name}}"]

generators:
  mock-dsl:
    written_to: main.tf
    builtin_values:
      env: "{{stack.env}}"
    values:
      regions:
        value: '{{fold["cloud"]["regions"]}}'
        default: ["{{tfgen[stack.env].region}}"]
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

const MODULE_PER_COMPONENT_CONFIG: &str = r#"version: "1"

globals:
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
            .env("XDG_CACHE_HOME", self.root.join(".cache"))
            .envs(variables.iter().copied())
            .current_dir(&self.root)
            .output()
            .unwrap()
    }
}

impl MockWorkspace {
    /// Copies `relative`, a folder of this repo, to the same place in the workspace.
    fn copy_from_repo(&self, relative: &str) {
        copy_folder(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(relative),
            &self.root.join(relative),
        );
    }
}

fn copy_folder(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_folder(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

const EXAMPLES: [&str; 2] = ["examples/one-module", "examples/module-per-component"];

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
            "  region  = \"{{tfgen[stack.env].region}}\"\n",
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
            "  env             = \"dev\"\n",
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
        "globals {\n  env = \"dev\"\n}\n",
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
        "globals {\n  env   = \"dev\"\n  tfgen = {}\n}\n",
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
fn a_stack_holding_no_documents_compiles_its_common_layer_alone() {
    let workspace = MockWorkspace::new("holding-no-documents");
    one_module_workspace(&workspace);
    workspace.write(
        "yaml-dsl.yml",
        &DSL_CONFIG.replacen("overlay_folders: [dev]", "overlay_folders: [dev, uat]", 1),
    );
    workspace.write(
        "tfgen.yml",
        &ONE_MODULE_CONFIG.replacen(
            "  dev:\n    region: mock-region-1\n",
            "  dev:\n    region: mock-region-1\n  uat:\n    region: mock-region-2\n",
            1,
        ),
    );
    workspace.write(
        "stacks/uat/stack.tg.hcl",
        concat!(
            "globals {\n",
            "  env             = \"uat\"\n",
            "  modules_version = \"1.2.3\"\n",
            "  tfgen           = { templates = {} }\n",
            "}\n",
        ),
    );
    let output = workspace.run(&["--no-color", "--stack", "stacks/uat"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let main = workspace.read("stacks/uat/main.tf");
    assert!(
        main.starts_with("# DO NOT EDIT. Generated by tfgen from stacks/mock-dsl.yml.\n"),
        "{main}"
    );
    assert!(main.contains("visibility = 30"), "{main}");
    assert!(!main.contains("orders_url"), "{main}");
    assert!(
        main.contains("data_files = [\"../mock-dsl.yml\"]"),
        "{main}"
    );
    assert!(main.contains("regions    = [\"mock-region-2\"]"), "{main}");
    assert!(!workspace.exists("stacks/uat/mock-dsl.yml"));
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
                "      - \"{config_root}/schemas/{{stack.env}}.schema.json\"\n",
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
        errors.contains("`mock_initialized` is required"),
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
    assert!(errors.contains("`mock_changed` is required"), "{errors}");
    let variable = workspace.run_with(
        &[],
        &[("TFGEN_SCHEMA", "mock-dsl=local/changed.schema.json")],
    );
    let errors = String::from_utf8_lossy(&variable.stderr);
    assert!(errors.contains("`mock_changed` is required"), "{errors}");
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
    assert!(errors.contains("--> tfgen.yml:11:5"), "{errors}");
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

#[test]
fn a_config_of_a_version_tfgen_does_not_read_is_an_error_at_the_value() {
    let workspace = MockWorkspace::new("config-version");
    one_module_workspace(&workspace);
    workspace.write(
        "tfgen.yml",
        &ONE_MODULE_CONFIG.replacen("version: \"1\"", "version: \"2\"", 1),
    );
    let output = workspace.run(&["--no-color"]);
    assert!(!output.status.success());
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("--> tfgen.yml:1:10"), "{errors}");
    assert!(
        errors.contains(
            "`version` must be one of `\"1\"`, `\"1.0\"`, `\"1.1\"`, `\"1.2\"`, `\"1.3\"`"
        ),
        "{errors}"
    );
    assert!(
        errors.contains("`version` is one of `\"1\"`, `\"1.0\"`, `\"1.1\"`, `\"1.2\"`, `\"1.3\"`"),
        "{errors}"
    );
}

#[test]
fn the_schema_cache_capacity_is_a_whole_number_from_one_of_format_1_1() {
    let workspace = MockWorkspace::new("schema-cache-capacity");
    one_module_workspace(&workspace);
    let cases = [
        ("1.1", "0", "`schema_cache_capacity` must be >= 1"),
        ("1.1", "many", "`schema_cache_capacity` must be integer"),
        ("1.0", "100", "`performance` is not a field of tfgen.yml"),
    ];
    for (version, capacity, message) in cases {
        let config = ONE_MODULE_CONFIG.replacen(
            "version: \"1\"",
            &format!("version: \"{version}\"\nperformance:\n  schema_cache_capacity: {capacity}"),
            1,
        );
        workspace.write("tfgen.yml", &config);
        let output = workspace.run(&["--no-color"]);
        assert!(!output.status.success());
        let errors = String::from_utf8_lossy(&output.stderr);
        assert!(errors.contains(message), "{errors}");
    }
    let config = ONE_MODULE_CONFIG.replacen(
        "version: \"1\"",
        "version: \"1.1\"\nperformance:\n  schema_cache_capacity: 1",
        1,
    );
    workspace.write("tfgen.yml", &config);
    let output = workspace.run(&["--no-color"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_config_problem_is_worded_as_the_standard_words_it() {
    let workspace = MockWorkspace::new("config-wording");
    one_module_workspace(&workspace);
    let cases = [
        (
            "each: declaration",
            "each: declarations",
            "`each` must be one of",
        ),
        (
            "written_to: main.tf",
            "writen_to: main.tf",
            "`written_to` is required",
        ),
        (
            "scope: ENTRY",
            "scope: ENTRY\n        scop: ENTRY",
            "`scop` is not a field of",
        ),
        (
            "each: declaration",
            "each: [declaration]",
            "`each` must be string",
        ),
    ];
    for (written, misspelt, expected) in cases {
        workspace.write(
            "tfgen.yml",
            &ONE_MODULE_CONFIG.replacen(written, misspelt, 1),
        );
        let output = workspace.run(&["--no-color"]);
        assert!(!output.status.success());
        let errors = String::from_utf8_lossy(&output.stderr);
        assert!(errors.contains(expected), "{errors}");
    }
}

#[test]
fn a_config_naming_its_minor_version_is_read() {
    let workspace = MockWorkspace::new("config-minor-version");
    one_module_workspace(&workspace);
    workspace.write(
        "tfgen.yml",
        &ONE_MODULE_CONFIG.replacen("version: \"1\"", "version: \"1.0\"", 1),
    );
    let output = workspace.run(&["--no-color"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_first_value_is_written_for_the_first_declaration_of_its_scope_else_its_default() {
    let workspace = MockWorkspace::new("first-value");
    one_module_workspace(&workspace);
    let first_values = concat!(
        "    values:\n",
        "      first_queue:\n",
        "        first: declaration\n",
        "        scope: ENTRY\n",
        "        scope_keys: [queue]\n",
        "        value: \"{{each.value.name}}\"\n",
        "      first_topic:\n",
        "        first: declaration\n",
        "        scope: ENTRY\n",
        "        scope_keys: [topic]\n",
        "        value: \"{{each.value.name}}\"\n",
        "        default: mock-no-topic\n",
    );
    let config = ONE_MODULE_CONFIG.replacen("    values:\n", first_values, 1);
    workspace.write("tfgen.yml", &config);
    workspace.write(
        "templates/stack_facts.hcl",
        concat!(
            "locals {\n",
            "  first_queue = \"{{values.first_queue}}\"\n",
            "  first_topic = \"{{values.first_topic}}\"\n",
            "}\n",
        ),
    );
    let output = workspace.run(&["--no-color"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let main = workspace.read("stacks/dev/main.tf");
    assert!(main.contains("first_queue = \"mock-orders\""), "{main}");
    assert!(main.contains("first_topic = \"mock-no-topic\""), "{main}");

    let without_default = config.replacen("        default: mock-no-topic\n", "", 1);
    workspace.write("tfgen.yml", &without_default);
    let output = workspace.run(&["--no-color"]);
    assert!(!output.status.success());
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(
        errors.contains("`first` selects no declaration and has no `default`"),
        "{errors}"
    );

    let format_1_1 = config.replacen("version: \"1\"", "version: \"1.1\"", 1);
    workspace.write("tfgen.yml", &format_1_1);
    let output = workspace.run(&["--no-color"]);
    assert!(!output.status.success());
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("`first`"), "{errors}");
}

#[test]
fn each_example_generates_the_files_it_commits() {
    let workspace = MockWorkspace::new("examples");
    for example in EXAMPLES {
        workspace.copy_from_repo(example);
    }
    let output = workspace.run(&["--no-color"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("0 of {} stacks changed\n", EXAMPLES.len())
    );
}

#[test]
fn every_tfgen_yml_of_the_repo_is_valid_against_the_published_schema() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let schema: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(manifest.join("schemas/tfgen-v1.3.schema.json")).unwrap(),
    )
    .unwrap();
    let validator = jsonschema::draft202012::new(&schema).unwrap();
    let mut configs: Vec<(String, String)> = vec![
        ("ONE_MODULE_CONFIG".to_owned(), ONE_MODULE_CONFIG.to_owned()),
        (
            "MODULE_PER_COMPONENT_CONFIG".to_owned(),
            MODULE_PER_COMPONENT_CONFIG.to_owned(),
        ),
    ];
    for example in EXAMPLES {
        let file = format!("{example}/tfgen.yml");
        configs.push((
            file.clone(),
            fs::read_to_string(manifest.join(&file)).unwrap(),
        ));
    }
    for (name, text) in configs {
        let node =
            yaml_dsl_spec::yaml_reader::parse(&Arc::from(name.as_str()), &text, &[]).unwrap();
        let errors: Vec<String> = validator
            .iter_errors(&node.to_json())
            .map(|error| format!("{error} at {}", error.instance_path()))
            .collect();
        assert!(errors.is_empty(), "{name}: {errors:?}");
    }
}

#[test]
fn a_misspelt_field_of_the_dsl_definition_is_an_error_at_that_field() {
    let workspace = MockWorkspace::new("definition-field");
    one_module_workspace(&workspace);
    workspace.write(
        "yaml-dsl.yml",
        &DSL_CONFIG.replacen("    file_excludes: []", "    file_exclude: []", 1),
    );
    let output = workspace.run(&["--no-color"]);
    assert!(!output.status.success());
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("--> yaml-dsl.yml:5:5"), "{errors}");
    assert!(errors.contains("did you mean `file_excludes`?"), "{errors}");
    assert!(!workspace.exists("stacks/dev/main.tf"));
}

#[test]
fn the_stack_flag_completes_to_the_repo_s_tfgen_stacks_only() {
    let workspace = MockWorkspace::new("complete-stacks");
    one_module_workspace(&workspace);
    let output = workspace.run(&["__complete", "--stack", ""]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "stacks/dev\n");
}

#[test]
fn the_schema_flag_completes_to_the_generated_dsls_then_their_paths() {
    let workspace = MockWorkspace::new("complete-schemas");
    one_module_workspace(&workspace);
    let ids = workspace.run(&["__complete", "--schema", ""]);
    assert_eq!(String::from_utf8_lossy(&ids.stdout), "mock-dsl=\n");
    let paths = workspace.run(&["__complete", "--schema", "mock-dsl=temp"]);
    assert!(
        String::from_utf8_lossy(&paths.stdout).contains("mock-dsl=templates/\n"),
        "{}",
        String::from_utf8_lossy(&paths.stdout)
    );
}

#[test]
fn the_completion_scripts_are_printed_for_each_shell() {
    let workspace = MockWorkspace::new("completion-scripts");
    let zsh = workspace.run(&["completion", "zsh"]);
    assert!(String::from_utf8_lossy(&zsh.stdout).starts_with("#compdef tfgen\n"));
    let unknown = workspace.run(&["completion", "mock-shell"]);
    assert_eq!(unknown.status.code(), Some(2));
}

/// What bash's completion offers for `typed`, a command line after the binary.
fn bash_completion(workspace: &MockWorkspace, typed: &str) -> String {
    let script = workspace.run(&["completion", "bash"]);
    workspace.write("completion.bash", &String::from_utf8_lossy(&script.stdout));
    let output = Command::new("bash")
        .args([
            "-c",
            concat!(
                "source completion.bash\n",
                "COMP_LINE=\"$TFGEN_BINARY $TYPED\"\n",
                "COMP_POINT=${#COMP_LINE}\n",
                "_tfgen 2>/dev/null\n",
                "printf '%s\\n' \"${COMPREPLY[@]}\"\n",
            ),
        ])
        .env("TFGEN_BINARY", env!("CARGO_BIN_EXE_tfgen"))
        .env("TYPED", typed)
        .current_dir(&workspace.root)
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn bash_completes_a_stack_and_a_schema_path_after_its_equals_sign() {
    let workspace = MockWorkspace::new("bash-completion");
    one_module_workspace(&workspace);
    assert_eq!(bash_completion(&workspace, "--stack st"), "stacks/dev\n");
    assert_eq!(bash_completion(&workspace, "--stack dev"), "stacks/dev\n");
    assert_eq!(bash_completion(&workspace, "--sch"), "--schema\n");
    assert_eq!(
        bash_completion(&workspace, "--schema mock-dsl=mock"),
        "mock.schema.json\n"
    );
}

/// What zsh's completion function hands `compadd` for `typed`, the words after the
/// binary, the last the one being completed: each call's options, then the values
/// and displays it was given, one per line.
fn zsh_completion(workspace: &MockWorkspace, typed: &[&str]) -> Option<String> {
    Command::new("zsh").arg("--version").output().ok()?;
    let script = workspace.run(&["completion", "zsh"]);
    workspace.write("_tfgen", &String::from_utf8_lossy(&script.stdout));
    let output = Command::new("zsh")
        .args([
            "-f",
            "-c",
            concat!(
                "compdef() { : }\n",
                "compadd() {\n",
                "    local -a said listed\n",
                "    while (( $# )); do\n",
                "        if [[ $1 == -[ad] ]]; then\n",
                "            said+=(\"$1\"); listed+=(\"${(@P)2}\"); shift 2\n",
                "        else\n",
                "            said+=(\"$1\"); shift\n",
                "        fi\n",
                "    done\n",
                "    print -r -- \"compadd ${(j: :)${(@qq)said}}\"\n",
                "    print -rl -- \"  \"${^listed}\n",
                "}\n",
                "words=(\"$TFGEN_BINARY\" \"$@\")\n",
                "CURRENT=${#words}\n",
                "source ./_tfgen\n",
                "_tfgen\n",
                "print -r -- \"postfuncs ${comppostfuncs[*]}\"\n",
            ),
            "zsh",
        ])
        .args(typed)
        .env("TFGEN_BINARY", env!("CARGO_BIN_EXE_tfgen"))
        .current_dir(&workspace.root)
        .output()
        .unwrap();
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn zsh_lists_each_flag_with_its_description_and_its_short_form_beside_it() {
    let workspace = MockWorkspace::new("zsh-flags");
    let Some(listed) = zsh_completion(&workspace, &[""]) else {
        eprintln!("zsh is not installed; skipped");
        return;
    };
    let matcher = "'-Q' '-M' 'l:|=* l:[:/]|=* r:|=*'";
    for display in [
        format!("  {:<14}  -- a stack to generate, repeatable\n", "--stack"),
        format!("  {:<14}  -- print the release\n", "--version | -v"),
    ] {
        assert!(listed.contains(&display), "{display:?} in {listed}");
    }
    assert!(
        listed.contains(&format!(
            "compadd '-V' 'options' '-n' {matcher} '-a'\n  -v\n"
        )),
        "{listed}"
    );
}

#[test]
fn zsh_is_handed_the_candidates_with_a_matcher_taking_a_run_typed_anywhere() {
    let workspace = MockWorkspace::new("zsh-stacks");
    one_module_workspace(&workspace);
    let Some(listed) = zsh_completion(&workspace, &["--stack", "dev"]) else {
        eprintln!("zsh is not installed; skipped");
        return;
    };
    assert_eq!(
        listed,
        concat!(
            "compadd '-Q' '-M' 'l:|=* l:[:/]|=* r:|=*' '-a'\n  stacks/dev\n",
            "postfuncs __tfgen_type_no_shared_run\n",
        )
    );
}

#[test]
fn zsh_lists_a_lone_flag_s_description_and_never_types_a_shared_run() {
    let workspace = MockWorkspace::new("zsh-lone-flag");
    let Some(lone) = zsh_completion(&workspace, &["--sta"]) else {
        eprintln!("zsh is not installed; skipped");
        return;
    };
    assert!(
        lone.ends_with("postfuncs __tfgen_type_no_shared_run __tfgen_list_a_lone_description\n"),
        "{lone}"
    );
    let every = zsh_completion(&workspace, &[""]).unwrap();
    assert!(
        every.ends_with("postfuncs __tfgen_type_no_shared_run\n"),
        "{every}"
    );
}
