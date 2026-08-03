use graphoxide_core::{make_id, Edge, Extraction, Node};
use graphoxide_extract::extract_project_with_options;
use std::{fs, path::Path};
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

    fn write(&self, relative: &str, body: &str) {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn one(relative: &str, body: &str) -> Vec<Extraction> {
        let project = Self::new();
        project.write(relative, body);
        project.extract()
    }

    fn extract(&self) -> Vec<Extraction> {
        extract_project_with_options(self.root.path(), true).unwrap()
    }
}

fn stem(source_file: &str) -> String {
    Path::new(source_file)
        .with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
}

fn symbol(source_file: &str, name: &str) -> String {
    make_id(&[&stem(source_file), name])
}

fn method(source_file: &str, class: &str, name: &str) -> String {
    make_id(&[&symbol(source_file, class), name])
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|extraction| &extraction.nodes)
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|extraction| &extraction.edges)
}

fn has_node(extractions: &[Extraction], source_file: &str, name: &str) -> bool {
    let id = symbol(source_file, name);
    nodes(extractions).any(|node| node.id == id)
}

fn has_edge(extractions: &[Extraction], source: &str, target: &str, relation: &str) -> bool {
    edges(extractions).any(|edge| {
        edge.true_source() == source && edge.true_target() == target && edge.relation == relation
    })
}

fn has_decorator(extractions: &[Extraction], owner: &str, decorator: &str) -> bool {
    let target_ids = nodes(extractions)
        .filter(|node| node.label == decorator)
        .map(|node| node.id.as_str())
        .collect::<Vec<_>>();
    edges(extractions).any(|edge| {
        edge.true_source() == owner
            && edge.relation == "references"
            && edge
                .extra
                .get("context")
                .and_then(serde_json::Value::as_str)
                == Some("decorator")
            && target_ids.contains(&edge.true_target())
    })
}

#[test]
fn test_generator_declaration_is_node_ts() {
    let result = Project::one(
        "src/g.ts",
        "export function* counter() { yield 1; yield 2; }\n",
    );
    assert!(has_node(&result, "src/g.ts", "counter"));
    assert!(has_edge(
        &result,
        &make_id(&["src/g"]),
        &symbol("src/g.ts", "counter"),
        "contains"
    ));
}

#[test]
fn test_generator_declaration_is_node_js() {
    assert!(has_node(
        &Project::one("src/g.js", "function* gen() { yield 42; }\n"),
        "src/g.js",
        "gen"
    ));
}

#[test]
fn test_generator_expression_is_node() {
    assert!(has_node(
        &Project::one(
            "src/h.ts",
            "export const stream = function* () { yield 'a'; };\n"
        ),
        "src/h.ts",
        "stream"
    ));
}

#[test]
fn test_generator_body_calls_are_attributed() {
    let result = Project::one(
        "src/g.ts",
        "function helper() {}\nfunction* producer() { helper(); yield 1; }\n",
    );
    assert!(has_edge(
        &result,
        &symbol("src/g.ts", "producer"),
        &symbol("src/g.ts", "helper"),
        "calls"
    ));
}

#[test]
fn test_async_generator_declaration_is_node() {
    assert!(has_node(
        &Project::one(
            "src/ag.ts",
            "export async function* pages() { yield await Promise.resolve(1); }\n"
        ),
        "src/ag.ts",
        "pages"
    ));
}

#[test]
fn test_class_decorator_on_exported_class() {
    let result = Project::one(
        "src/c.ts",
        "@Component({ selector: 'app' })\nexport class AppComponent {}\n",
    );
    assert!(has_decorator(
        &result,
        &symbol("src/c.ts", "AppComponent"),
        "Component"
    ));
}

#[test]
fn test_class_decorator_on_plain_class() {
    let result = Project::one("src/s.ts", "@Injectable()\nclass Service {}\n");
    assert!(has_decorator(
        &result,
        &symbol("src/s.ts", "Service"),
        "Injectable"
    ));
}

#[test]
fn test_stacked_class_decorators() {
    let result = Project::one(
        "src/s.ts",
        "@Injectable()\n@Entity()\nexport class Repo {}\n",
    );
    let owner = symbol("src/s.ts", "Repo");
    assert!(has_decorator(&result, &owner, "Injectable"));
    assert!(has_decorator(&result, &owner, "Entity"));
}

#[test]
fn test_method_decorator_attributes_to_method() {
    let result = Project::one(
        "src/c.ts",
        "export class C { @HostListener('click') onClick() {} }\n",
    );
    assert!(has_decorator(
        &result,
        &method("src/c.ts", "C", "onClick"),
        "HostListener"
    ));
    assert!(!has_decorator(
        &result,
        &symbol("src/c.ts", "C"),
        "HostListener"
    ));
}

#[test]
fn test_stacked_method_decorators() {
    let result = Project::one(
        "src/c.ts",
        "export class C { @Get('/') @UseGuards(Auth) list() {} }\n",
    );
    let owner = method("src/c.ts", "C", "list");
    assert!(has_decorator(&result, &owner, "Get"));
    assert!(has_decorator(&result, &owner, "UseGuards"));
}

#[test]
fn test_field_decorator_attributes_to_class() {
    let result = Project::one(
        "src/c.ts",
        "export class C { @Input() name: string; @Column() age: number; }\n",
    );
    let owner = symbol("src/c.ts", "C");
    assert!(has_decorator(&result, &owner, "Input"));
    assert!(has_decorator(&result, &owner, "Column"));
}

#[test]
fn test_parameter_decorator_attributes_to_constructor() {
    let result = Project::one(
        "src/c.ts",
        "export class C { constructor(@Inject(TOKEN) private s: Svc) {} }\n",
    );
    assert!(has_decorator(
        &result,
        &method("src/c.ts", "C", "constructor"),
        "Inject"
    ));
}

#[test]
fn test_namespaced_decorator_uses_property_name() {
    let result = Project::one("src/c.ts", "@core.Component({})\nexport class Widget {}\n");
    assert!(has_decorator(
        &result,
        &symbol("src/c.ts", "Widget"),
        "Component"
    ));
}

#[test]
fn test_external_decorator_stub_disambiguated_per_file() {
    let project = Project::new();
    project.write("src/a.ts", "@Injectable()\nexport class A {}\n");
    project.write("src/b.ts", "@Injectable()\nexport class B {}\n");
    let result = project.extract();
    let targets = |owner: String| {
        edges(&result)
            .filter(|edge| {
                edge.true_source() == owner
                    && edge.relation == "references"
                    && edge
                        .extra
                        .get("context")
                        .and_then(serde_json::Value::as_str)
                        == Some("decorator")
            })
            .map(|edge| edge.true_target().to_owned())
            .collect::<Vec<_>>()
    };
    let a = targets(symbol("src/a.ts", "A"));
    let b = targets(symbol("src/b.ts", "B"));
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_ne!(a, b);
    assert!(nodes(&result).any(|node| node.id == a[0] && node.label == "Injectable"));
    assert!(nodes(&result).any(|node| node.id == b[0] && node.label == "Injectable"));
}

#[test]
fn test_namespace_is_node() {
    assert!(has_node(
        &Project::one(
            "src/n.ts",
            "export namespace Geometry { export const PI = 3.14; }\n"
        ),
        "src/n.ts",
        "Geometry"
    ));
}

#[test]
fn test_module_keyword_is_node() {
    assert!(has_node(
        &Project::one("src/m.ts", "module Legacy { export class Thing {} }\n"),
        "src/m.ts",
        "Legacy"
    ));
}

#[test]
fn test_nested_namespace_name() {
    let result = Project::one(
        "src/nn.ts",
        "namespace App.Core.Util { export const v = 1; }\n",
    );
    assert!(nodes(&result).any(|node| {
        node.id == symbol("src/nn.ts", "App.Core.Util") && node.label == "App.Core.Util"
    }));
}

#[test]
fn test_namespace_members_still_extracted() {
    let result = Project::one(
        "src/n.ts",
        "namespace Shapes { export class Circle {} export function area() { return 0; } }\n",
    );
    for name in ["Shapes", "Circle", "area"] {
        assert!(has_node(&result, "src/n.ts", name), "{name}");
    }
}

#[test]
fn test_ambient_string_module_quotes_stripped() {
    let result = Project::one(
        "src/amb.ts",
        "declare module \"pkg-name\" { export const z = 3; }\n",
    );
    assert!(nodes(&result)
        .any(|node| { node.id == symbol("src/amb.ts", "pkg-name") && node.label == "pkg-name" }));
}

#[test]
fn test_namespace_node_not_emitted_in_js() {
    assert!(has_node(
        &Project::one("src/p.js", "function ok() {}\n"),
        "src/p.js",
        "ok"
    ));
}

fn assert_inherits(
    result: &[Extraction],
    source_file: &str,
    source_name: &str,
    target_file: &str,
    target_name: &str,
    relation: &str,
) {
    assert!(has_edge(
        result,
        &symbol(source_file, source_name),
        &symbol(target_file, target_name),
        relation
    ));
}

#[test]
fn test_interface_extends_same_file() {
    let result = Project::one(
        "src/a.ts",
        "export interface Base { x: number; }\nexport interface Derived extends Base { y: number; }\n",
    );
    assert_inherits(
        &result, "src/a.ts", "Derived", "src/a.ts", "Base", "inherits",
    );
}

#[test]
fn test_interface_extends_multiple_same_file() {
    let result = Project::one(
        "src/a.ts",
        "interface A { a: number; }\ninterface B { b: number; }\ninterface M extends A, B { m: number; }\n",
    );
    assert_inherits(&result, "src/a.ts", "M", "src/a.ts", "A", "inherits");
    assert_inherits(&result, "src/a.ts", "M", "src/a.ts", "B", "inherits");
}

#[test]
fn test_class_extends_same_file() {
    let result = Project::one("src/a.ts", "class Animal {}\nclass Dog extends Animal {}\n");
    assert_inherits(&result, "src/a.ts", "Dog", "src/a.ts", "Animal", "inherits");
}

#[test]
fn test_interface_extends_generic_base_same_file() {
    let result = Project::one(
        "src/a.ts",
        "interface Base<T> { x: T; }\ninterface G extends Base<number> { y: number; }\n",
    );
    assert_inherits(&result, "src/a.ts", "G", "src/a.ts", "Base", "inherits");
}

#[test]
fn test_interface_extends_imported() {
    let project = Project::new();
    project.write("src/b.ts", "export interface Imported { z: number; }\n");
    project.write(
        "src/a.ts",
        "import { Imported } from './b';\nexport interface D extends Imported { d: number; }\n",
    );
    let result = project.extract();
    assert_inherits(&result, "src/a.ts", "D", "src/b.ts", "Imported", "inherits");
}

#[test]
fn test_imported_class_extends_still_works() {
    let project = Project::new();
    project.write("src/b.ts", "export class Imported {}\n");
    project.write(
        "src/a.ts",
        "import { Imported } from './b';\nclass Cat extends Imported {}\n",
    );
    let result = project.extract();
    assert_inherits(
        &result, "src/a.ts", "Cat", "src/b.ts", "Imported", "inherits",
    );
}

#[test]
fn test_class_implements_same_file_interface() {
    let result = Project::one(
        "src/a.ts",
        "interface Walker { walk(): void; }\nclass Person implements Walker { walk() {} }\n",
    );
    assert_inherits(
        &result,
        "src/a.ts",
        "Person",
        "src/a.ts",
        "Walker",
        "implements",
    );
}
