use graphoxide_core::{Edge, Extraction, Node};
use graphoxide_extract::extract_files;
use std::{fs, path::PathBuf};
use tempfile::TempDir;

struct Project {
    root: TempDir,
}

impl Project {
    fn new() -> Self {
        Self {
            root: TempDir::new().unwrap(),
        }
    }

    fn write(&self, relative: &str, source: &str) {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }

    fn extract(&self, files: &[&str]) -> Vec<Extraction> {
        let paths = files
            .iter()
            .map(|file| self.root.path().join(file))
            .collect::<Vec<PathBuf>>();
        extract_files(&paths, Some(self.root.path()), true)
            .unwrap()
            .extractions
    }
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|extraction| &extraction.nodes)
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|extraction| &extraction.edges)
}

fn class_defs<'a>(extractions: &'a [Extraction], label: &str) -> Vec<&'a Node> {
    nodes(extractions)
        .filter(|node| node.label == label && !node.source_file.is_empty())
        .collect()
}

fn source_class(extractions: &[Extraction], label: &str) -> String {
    class_defs(extractions, label)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("missing class {label}"))
        .id
        .clone()
}

fn source_fqn(extractions: &[Extraction], fqn: &str) -> String {
    nodes(extractions)
        .find(|node| {
            !node.source_file.is_empty()
                && node.extra.get("php_fqn").and_then(|value| value.as_str()) == Some(fqn)
        })
        .unwrap_or_else(|| panic!("missing PHP definition {fqn}"))
        .id
        .clone()
}

fn inheritance_target(extractions: &[Extraction], source: &str) -> String {
    edges(extractions)
        .find(|edge| edge.relation == "inherits" && edge.true_source() == source)
        .unwrap_or_else(|| panic!("missing inheritance for {source}"))
        .true_target()
        .to_owned()
}

fn edge_between(extractions: &[Extraction], relation: &str, source: &str, target: &str) -> bool {
    edges(extractions).any(|edge| {
        edge.relation == relation && edge.true_source() == source && edge.true_target() == target
    })
}

fn owned_method(extractions: &[Extraction], owner: &str, name: &str) -> String {
    let target = edges(extractions)
        .find(|edge| {
            edge.relation == "method"
                && edge.true_source() == owner
                && nodes(extractions).any(|node| {
                    node.id == edge.true_target()
                        && node
                            .label
                            .trim_matches(|character| matches!(character, '.' | '(' | ')'))
                            == name
                })
        })
        .unwrap_or_else(|| panic!("missing method {owner}::{name}"))
        .true_target();
    target.to_owned()
}

#[test]
fn test_php_external_namespaced_base_does_not_collapse_onto_internal_class() {
    let project = Project::new();
    project.write(
        "app/Models/Page.php",
        "<?php\nnamespace App\\Models;\nclass Page extends Model {}\n",
    );
    project.write(
        "app/Filament/Pages/ManageSiteSettings.php",
        concat!(
            "<?php\nnamespace App\\Filament\\Pages;\n",
            "use Filament\\Pages\\Page;\n",
            "class ManageSiteSettings extends Page {}\n",
        ),
    );
    let result = project.extract(&[
        "app/Models/Page.php",
        "app/Filament/Pages/ManageSiteSettings.php",
    ]);
    let pages = class_defs(&result, "Page");
    assert_eq!(pages.len(), 1);
    assert!(pages[0].source_file.contains("Models"));
    let internal = pages[0].id.clone();
    let manage = source_class(&result, "ManageSiteSettings");
    let target = inheritance_target(&result, &manage);
    assert_ne!(target, internal);
    let stub = nodes(&result)
        .find(|node| node.id == target)
        .expect("external FQN stub");
    assert!(stub.source_file.is_empty());
    assert_eq!(stub.label, "Filament\\Pages\\Page");
    assert!(edges(&result)
        .filter(
            |edge| edge.relation == "imports" && edge.true_source().contains("managesitesettings")
        )
        .all(|edge| edge.true_target() != internal));
}

#[test]
fn test_php_ambiguous_base_disambiguated_by_use() {
    let project = Project::new();
    project.write(
        "app/Models/Page.php",
        "<?php\nnamespace App\\Models;\nclass Page {}\n",
    );
    project.write(
        "app/Cms/Page.php",
        "<?php\nnamespace App\\Cms;\nclass Page {}\n",
    );
    project.write(
        "app/Cms/Editor.php",
        concat!(
            "<?php\nnamespace App\\Cms;\n",
            "use App\\Cms\\Page;\n",
            "class Editor extends Page {}\n",
        ),
    );
    let result = project.extract(&[
        "app/Models/Page.php",
        "app/Cms/Page.php",
        "app/Cms/Editor.php",
    ]);
    let editor = source_class(&result, "Editor");
    let target = inheritance_target(&result, &editor);
    let target = nodes(&result).find(|node| node.id == target).unwrap();
    assert!(target.source_file.contains("Cms"));
    assert!(!target.source_file.contains("Models"));
}

#[test]
fn test_php_use_alias_resolves() {
    let project = Project::new();
    project.write("src/Foo/Bar.php", "<?php\nnamespace Foo;\nclass Bar {}\n");
    project.write(
        "src/App/X.php",
        concat!(
            "<?php\nnamespace App;\n",
            "use Foo\\Bar as Baz;\n",
            "class X extends Baz {}\n",
        ),
    );
    let result = project.extract(&["src/Foo/Bar.php", "src/App/X.php"]);
    let target = inheritance_target(&result, &source_class(&result, "X"));
    let target = nodes(&result).find(|node| node.id == target).unwrap();
    assert!(target.source_file.contains("Foo"));
}

#[test]
fn test_php_fully_qualified_base_resolves() {
    let project = Project::new();
    project.write(
        "app/Models/Page.php",
        "<?php\nnamespace App\\Models;\nclass Page {}\n",
    );
    project.write(
        "app/Http/Y.php",
        "<?php\nnamespace App\\Http;\nclass Y extends \\App\\Models\\Page {}\n",
    );
    let result = project.extract(&["app/Models/Page.php", "app/Http/Y.php"]);
    let target = inheritance_target(&result, &source_class(&result, "Y"));
    let target = nodes(&result).find(|node| node.id == target).unwrap();
    assert!(target.source_file.contains("Models"));
}

#[test]
fn test_php_plain_no_namespace_inheritance_preserved() {
    let project = Project::new();
    project.write("src/Base.php", "<?php\nclass Base {}\n");
    project.write("src/Child.php", "<?php\nclass Child extends Base {}\n");
    let result = project.extract(&["src/Base.php", "src/Child.php"]);
    let target = inheritance_target(&result, &source_class(&result, "Child"));
    let target = nodes(&result).find(|node| node.id == target).unwrap();
    assert!(!target.source_file.is_empty());
    assert_eq!(target.label, "Base");
}

#[test]
fn php_container_binding_resolves_both_namespaced_endpoints() {
    let project = Project::new();
    project.write(
        "php/Contracts.php",
        concat!(
            "<?php\nnamespace MatrixRuntime;\n",
            "interface Worker { public function process(string $value): string; }\n",
            "class Service implements Worker { public function process(string $value): string { return trim($value); } }\n",
        ),
    );
    project.write(
        "php/Runner.php",
        concat!(
            "<?php\nnamespace MatrixRuntime;\n",
            "class Provider { public function register(): void {\n",
            "  $this->app->bind(Worker::class, Service::class);\n",
            "} }\n",
        ),
    );
    let result = project.extract(&["php/Contracts.php", "php/Runner.php"]);
    let worker = source_class(&result, "Worker");
    let service = source_class(&result, "Service");

    assert!(edge_between(&result, "bound_to", &worker, &service));
}

#[test]
fn php_return_statement_is_not_a_field_but_typed_property_is() {
    let project = Project::new();
    project.write(
        "src/Runner.php",
        concat!(
            "<?php\nnamespace App;\n",
            "class Service { public function process(string $value): string { return $value; } }\n",
            "class Runner {\n",
            "  private Service $service;\n",
            "  public function execute(string $value): string {\n",
            "    return $this->service->process($value);\n",
            "  }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["src/Runner.php"]);
    let runner = source_class(&result, "Runner");
    let service = source_class(&result, "Service");
    assert!(edges(&result).any(|edge| {
        edge.relation == "references"
            && edge.true_source() == runner
            && edge.true_target() == service
            && edge.extra.get("context").and_then(|value| value.as_str()) == Some("field")
    }));
    assert!(!nodes(&result).any(|node| node.label.eq_ignore_ascii_case("return")));
}

#[test]
fn php_duplicate_fqn_is_left_unresolved_instead_of_fanning_out() {
    let project = Project::new();
    project.write("a/Page.php", "<?php\nnamespace Vendor;\nclass Page {}\n");
    project.write("b/Page.php", "<?php\nnamespace Vendor;\nclass Page {}\n");
    project.write(
        "app/Child.php",
        concat!(
            "<?php\nnamespace App;\n",
            "use Vendor\\Page;\n",
            "class Child extends Page {}\n",
        ),
    );
    let result = project.extract(&["a/Page.php", "b/Page.php", "app/Child.php"]);
    let definitions = class_defs(&result, "Page")
        .into_iter()
        .map(|node| node.id.clone())
        .collect::<Vec<_>>();
    assert_eq!(definitions.len(), 2);
    let target = inheritance_target(&result, &source_class(&result, "Child"));
    assert!(!definitions.contains(&target));
    let stub = nodes(&result).find(|node| node.id == target).unwrap();
    assert_eq!(stub.label, "Vendor\\Page");
    assert!(stub.source_file.is_empty());
}

#[test]
fn php_semicolon_method_declarations_do_not_capture_the_next_class_body() {
    let project = Project::new();
    project.write(
        "src/Contracts.php",
        concat!(
            "<?php\nnamespace App;\n",
            "interface Contract { public function execute(Service $service): Result; }\n",
            "abstract class Base { abstract public function prepare(): Result; }\n",
            "class Service {}\n",
            "class Result {}\n",
            "class Runner extends Base implements Contract {\n",
            "  private Service $service;\n",
            "  public function execute(Service $service): Result { return helper(); }\n",
            "  public function prepare(): Result { return helper(); }\n",
            "  private function helper(): Result { return new Result(); }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["src/Contracts.php"]);
    let runner = source_fqn(&result, "App\\Runner");
    let service = source_fqn(&result, "App\\Service");
    let helper = nodes(&result)
        .find(|node| node.label == ".helper()" && !node.source_file.is_empty())
        .expect("Runner::helper")
        .id
        .clone();

    assert!(edges(&result).any(|edge| {
        edge.relation == "references"
            && edge.true_source() == runner
            && edge.true_target() == service
            && edge.extra.get("context").and_then(|value| value.as_str()) == Some("field")
    }));
    assert_eq!(
        edges(&result)
            .filter(|edge| edge.relation == "calls" && edge.true_target() == helper)
            .count(),
        2,
        "only the two concrete method bodies should call helper"
    );
}

#[test]
fn php_trait_parameter_return_and_property_types_use_exact_namespace_identity() {
    let project = Project::new();
    project.write(
        "vendor/Types.php",
        concat!(
            "<?php\nnamespace Vendor;\n",
            "trait Audited {}\n",
            "class Payload {}\n",
            "class Result {}\n",
            "class Event {}\n",
            "class Listener {}\n",
        ),
    );
    project.write(
        "app/Types.php",
        concat!(
            "<?php\nnamespace App;\n",
            "trait Audited {}\n",
            "class Payload {}\n",
            "class Result {}\n",
            "class Event {}\n",
            "class Listener {}\n",
            "class Consumer {\n",
            "  use Audited;\n",
            "  private Result $result;\n",
            "  public function run(Payload $payload): Result { return $this->result; }\n",
            "}\n",
            "class Provider {\n",
            "  public function listeners(): array { return [Event::class => [Listener::class]]; }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["vendor/Types.php", "app/Types.php"]);
    let consumer = source_fqn(&result, "App\\Consumer");
    let app_trait = source_fqn(&result, "App\\Audited");
    let app_payload = source_fqn(&result, "App\\Payload");
    let app_result = source_fqn(&result, "App\\Result");
    let vendor_trait = source_fqn(&result, "Vendor\\Audited");
    let vendor_payload = source_fqn(&result, "Vendor\\Payload");
    let vendor_result = source_fqn(&result, "Vendor\\Result");
    let app_event = source_fqn(&result, "App\\Event");
    let app_listener = source_fqn(&result, "App\\Listener");
    let vendor_event = source_fqn(&result, "Vendor\\Event");
    let vendor_listener = source_fqn(&result, "Vendor\\Listener");
    let run = nodes(&result)
        .find(|node| node.label == ".run()" && node.source_file.contains("app/"))
        .expect("Consumer::run")
        .id
        .clone();

    assert!(edge_between(&result, "mixes_in", &consumer, &app_trait));
    assert!(!edge_between(&result, "mixes_in", &consumer, &vendor_trait));
    assert!(edges(&result).any(|edge| {
        edge.relation == "references"
            && edge.true_source() == consumer
            && edge.true_target() == app_result
            && edge.extra.get("context").and_then(|value| value.as_str()) == Some("field")
    }));
    assert!(edges(&result).any(|edge| {
        edge.relation == "references"
            && edge.true_source() == run
            && edge.true_target() == app_payload
            && edge.extra.get("context").and_then(|value| value.as_str()) == Some("parameter_type")
    }));
    assert!(edges(&result).any(|edge| {
        edge.relation == "references"
            && edge.true_source() == run
            && edge.true_target() == app_result
            && edge.extra.get("context").and_then(|value| value.as_str()) == Some("return_type")
    }));
    assert!(!edges(&result).any(|edge| {
        edge.relation == "references"
            && matches!(edge.true_target(), target if target == vendor_payload || target == vendor_result)
    }));
    assert!(edge_between(
        &result,
        "listened_by",
        &app_event,
        &app_listener
    ));
    assert!(!edges(&result).any(|edge| {
        edge.relation == "listened_by"
            && (edge.true_source() == vendor_event || edge.true_target() == vendor_listener)
    }));
}

#[test]
fn php_untyped_member_call_does_not_bind_to_an_unrelated_unique_method() {
    let project = Project::new();
    project.write(
        "vendor/Foreign.php",
        concat!(
            "<?php\nnamespace Vendor;\n",
            "class Foreign {\n",
            "  public function ping(): string { return 'foreign'; }\n",
            "}\n",
        ),
    );
    project.write(
        "app/Runner.php",
        concat!(
            "<?php\nnamespace App;\n",
            "class Runner {\n",
            "  public function run($value): string { return $value->ping(); }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["vendor/Foreign.php", "app/Runner.php"]);
    let run = nodes(&result)
        .find(|node| node.label == ".run()")
        .expect("Runner::run")
        .id
        .clone();
    let ping = nodes(&result)
        .find(|node| node.label == ".ping()")
        .expect("Foreign::ping")
        .id
        .clone();

    assert!(!edge_between(&result, "calls", &run, &ping));
}

#[test]
fn php_this_and_self_calls_use_the_nearest_current_type_hierarchy() {
    let project = Project::new();
    project.write(
        "app/Worker.php",
        concat!(
            "<?php\nnamespace App;\n",
            "interface Worker {\n",
            "  public function process(string $value): string;\n",
            "}\n",
            "class Service implements Worker {\n",
            "  public function process(string $value): string { return $value; }\n",
            "}\n",
            "class Runner extends Service {\n",
            "  private static function helper(): string { return 'ok'; }\n",
            "  public function execute(string $value): string { return $this->process($value); }\n",
            "  public static function executeSelf(): string { return self::helper(); }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["app/Worker.php"]);
    let worker = source_fqn(&result, "App\\Worker");
    let service = source_fqn(&result, "App\\Service");
    let runner = source_fqn(&result, "App\\Runner");
    let worker_process = owned_method(&result, &worker, "process");
    let service_process = owned_method(&result, &service, "process");
    let execute = owned_method(&result, &runner, "execute");
    let execute_self = owned_method(&result, &runner, "executeSelf");
    let helper = owned_method(&result, &runner, "helper");

    assert!(edge_between(&result, "calls", &execute, &service_process));
    assert!(!edge_between(&result, "calls", &execute, &worker_process));
    assert!(edge_between(&result, "calls", &execute_self, &helper));
}

#[test]
fn php_equal_distance_interface_methods_remain_ambiguous() {
    let project = Project::new();
    project.write(
        "app/Ambiguous.php",
        concat!(
            "<?php\nnamespace App;\n",
            "interface Left {\n",
            "  public function ping(): string;\n",
            "}\n",
            "interface Right {\n",
            "  public function ping(): string;\n",
            "}\n",
            "abstract class Runner implements Left, Right {\n",
            "  public function run(): string { return $this->ping(); }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["app/Ambiguous.php"]);
    let left = source_fqn(&result, "App\\Left");
    let right = source_fqn(&result, "App\\Right");
    let runner = source_fqn(&result, "App\\Runner");
    let left_ping = owned_method(&result, &left, "ping");
    let right_ping = owned_method(&result, &right, "ping");
    let run = owned_method(&result, &runner, "run");

    assert!(!edge_between(&result, "calls", &run, &left_ping));
    assert!(!edge_between(&result, "calls", &run, &right_ping));
}

#[test]
fn php_comments_and_strings_do_not_manufacture_container_or_listener_edges() {
    let project = Project::new();
    project.write(
        "app/Provider.php",
        concat!(
            "<?php\nnamespace App;\n",
            "class FakeContract {}\n",
            "class FakeService {}\n",
            "class FakeEvent {}\n",
            "class FakeListener {}\n",
            "class Provider {\n",
            "  public function register(): void {\n",
            "    // $this->app->bind(FakeContract::class, FakeService::class);\n",
            "    $example = 'bind(FakeContract::class, FakeService::class)';\n",
            "    /* FakeEvent::class => [FakeListener::class] */\n",
            "    $listeners = \"FakeEvent::class => [FakeListener::class]\";\n",
            "  }\n",
            "}\n",
        ),
    );
    let result = project.extract(&["app/Provider.php"]);

    assert!(!edges(&result).any(|edge| edge.relation == "bound_to"));
    assert!(!edges(&result).any(|edge| edge.relation == "listened_by"));
}
