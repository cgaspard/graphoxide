use graphoxide_core::{make_id, Edge, Extraction, Node};
use graphoxide_extract::{extract, extract_files};
use std::{collections::BTreeSet, fs, path::PathBuf};
use tempfile::TempDir;

fn write(root: &TempDir, relative: &str, source: &str) -> PathBuf {
    let path = root.path().join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, source).unwrap();
    path
}

fn nodes(extraction: &Extraction) -> impl Iterator<Item = &Node> {
    extraction.nodes.iter()
}

fn edges(extraction: &Extraction) -> impl Iterator<Item = &Edge> {
    extraction.edges.iter()
}

fn bare_label(label: &str) -> &str {
    label
        .trim_start_matches('.')
        .strip_suffix("()")
        .unwrap_or(label.trim_start_matches('.'))
}

fn node<'a>(extraction: &'a Extraction, label: &str) -> &'a Node {
    nodes(extraction)
        .find(|node| bare_label(&node.label) == label)
        .unwrap_or_else(|| panic!("missing Dart node {label:?}; nodes={:?}", extraction.nodes))
}

fn edge_to<'a>(
    extraction: &'a Extraction,
    source: Option<&str>,
    target: &str,
    relation: &str,
) -> Option<&'a Edge> {
    edges(extraction).find(|edge| {
        source.is_none_or(|source| edge.true_source() == source)
            && edge.true_target() == target
            && edge.relation == relation
    })
}

fn context_edge<'a>(extraction: &'a Extraction, target: &str, context: &str) -> &'a Edge {
    edges(extraction)
        .find(|edge| {
            edge.true_target() == target
                && edge.extra.get("context").and_then(|value| value.as_str()) == Some(context)
        })
        .unwrap_or_else(|| panic!("missing edge to {target:?} with context {context:?}"))
}

fn owned_method<'a>(extraction: &'a Extraction, owner: &str, method: &str) -> &'a Node {
    let owner = nodes(extraction)
        .find(|node| bare_label(&node.label) == owner && !node.source_file.is_empty())
        .unwrap_or_else(|| panic!("missing Dart owner {owner:?}"));
    let method_id = edges(extraction)
        .find(|edge| {
            edge.relation == "method"
                && edge.true_source() == owner.id
                && nodes(extraction)
                    .find(|node| node.id == edge.true_target())
                    .is_some_and(|node| bare_label(&node.label) == method)
        })
        .map(|edge| edge.true_target())
        .unwrap_or_else(|| panic!("missing Dart method {}.{method}", owner.label));
    nodes(extraction)
        .find(|node| node.id == method_id)
        .expect("owned Dart method node")
}

#[test]
fn test_universal_generic_syntax_extraction() {
    let root = TempDir::new().unwrap();
    let path = write(
        &root,
        "test_app_bloc.dart",
        concat!(
            "import 'package:flutter/material.dart';\n",
            "import 'package:flutter_bloc/flutter_bloc.dart';\n",
            "import 'package:injectable/injectable.dart';\n",
            "export 'package:flutter_bloc/flutter_bloc.dart';\n",
            "@injectable\n",
            "@HiveType(typeId: 10)\n",
            "class UserBloc extends Bloc<UserEvent, UserState> with MyMixin implements Disposable {\n",
            "  UserBloc() : super(InitialState());\n",
            "}\n",
            "@jsonSerializable\n",
            "enum UserRole { admin, user }\n",
            "extension StringExtensions on String {\n",
            "  bool get isEmail => contains('@');\n",
            "}\n",
            "final authServiceProvider = Provider<AuthService>((ref) => AuthService());\n",
            "final myData = 42;\n",
            "void checkDependencies(BuildContext context) {\n",
            "  final custom = context.dependOnInheritedWidgetOfExactType<CustomService>();\n",
            "  final auth = context.read<AuthService>();\n",
            "  final bloc = BlocProvider.of<UserBloc>(context);\n",
            "  final getItService = GetIt.I<DatabaseService>();\n",
            "  final locatorService = locator<api.NetworkFactory>();\n",
            "}\n",
        ),
    );
    let result = extract(&path).unwrap();
    let file = nodes(&result)
        .find(|node| node.label == "test_app_bloc.dart")
        .expect("Dart file node");
    assert_eq!(file.source_file, path.to_string_lossy());
    let bloc = node(&result, "UserBloc");
    assert_eq!(bloc.source_file, path.to_string_lossy());
    node(&result, "UserRole");

    assert!(edge_to(&result, Some(&bloc.id), "bloc", "inherits").is_some());
    assert!(node(&result, "Bloc").source_file.is_empty());
    assert!(edge_to(&result, Some(&bloc.id), "userevent", "references").is_some());
    assert!(edge_to(&result, Some(&bloc.id), "userstate", "references").is_some());
    assert!(node(&result, "UserEvent").source_file.is_empty());

    let injectable = node(&result, "@injectable");
    assert_eq!(injectable.id, "annotation_injectable");
    assert!(injectable.source_file.is_empty());
    assert!(edge_to(&result, Some(&bloc.id), &injectable.id, "configures").is_some());
    assert!(edge_to(&result, Some(&bloc.id), "mymixin", "mixes_in").is_some());
    assert!(edge_to(&result, Some(&bloc.id), "disposable", "implements").is_some());
    assert!(edge_to(&result, Some(&bloc.id), "mymixin", "implements").is_none());
    assert!(edge_to(&result, Some(&bloc.id), "disposable", "mixes_in").is_none());

    let extension = node(&result, "StringExtensions");
    assert!(edge_to(&result, Some(&extension.id), "string", "extends").is_some());
    node(&result, "authServiceProvider");
    assert!(edge_to(&result, Some(&file.id), "customservice", "references").is_some());
    assert!(node(&result, "CustomService").source_file.is_empty());
    assert!(edge_to(&result, Some(&file.id), "networkfactory", "references").is_some());

    let import = nodes(&result)
        .find(|node| node.id == "package_flutter_material_dart")
        .expect("material import");
    assert_eq!(import.label, "package:flutter/material.dart");
    assert!(import.source_file.is_empty());
    let export = nodes(&result)
        .find(|node| node.id == "package_flutter_bloc_flutter_bloc_dart")
        .expect("flutter_bloc export");
    assert!(export.source_file.is_empty());
    assert!(edge_to(&result, Some(&file.id), &export.id, "exports").is_some());
}

#[test]
fn test_advanced_dart_features() {
    let root = TempDir::new().unwrap();
    let path = write(
        &root,
        "test_advanced.dart",
        concat!(
            "import 'package:riverpod/riverpod.dart';\n",
            "abstract base class MyBaseClass {}\n",
            "abstract interface class MyInterface {}\n",
            "mixin class MyMixinClass {}\n",
            "@riverpod\n",
            "class MyNotifier extends _$MyNotifier {\n",
            "  @override\n",
            "  String build() {\n",
            "    ref.watch(anotherProvider);\n",
            "    return \"hello\";\n",
            "  }\n",
            "}\n",
            "@riverpod\n",
            "String myValue(MyValueRef ref) { return \"world\"; }\n",
            "class MyModel {\n",
            "  late final String lateField;\n",
            "  final int noInitField;\n",
            "  final String initField = \"init\";\n",
            "}\n",
            "final (int, String) typedRecord = (1, \"one\");\n",
            "var (recA, recB) = (10, 20);\n",
            "(double, double) getCoordinates() {\n",
            "    var localVal = switch (typedRecord) {\n",
            "      (int a, String b) => (1.0, 2.0),\n",
            "      _ => (0.0, 0.0),\n",
            "    };\n",
            "    return localVal;\n",
            "}\n",
            "class AuthBloc extends Bloc<AuthEvent, AuthState> {\n",
            "  AuthBloc() : super(AuthInitial()) {\n",
            "    on<AuthLogin>((event, emit) { emit(AuthLoading()); });\n",
            "    on<AuthLogout>((event, emit) { yield AuthSuccess(); });\n",
            "  }\n",
            "}\n",
            "class HomeWidget {\n",
            "  void triggerLogin(BuildContext context) {\n",
            "    context.read<AuthBloc>().add(AuthLogin());\n",
            "  }\n",
            "}\n",
        ),
    );
    let result = extract(&path).unwrap();
    for label in [
        "MyBaseClass",
        "MyInterface",
        "MyMixinClass",
        "lateField",
        "noInitField",
        "initField",
        "typedRecord",
        "recA",
        "recB",
        "getCoordinates",
        "myNotifierProvider",
        "myValueProvider",
    ] {
        node(&result, label);
    }
    assert!(!nodes(&result).any(|node| bare_label(&node.label) == "class"));
    assert!(!nodes(&result).any(|node| bare_label(&node.label) == "localVal"));
    let notifier = node(&result, "MyNotifier");
    let generated_base = node(&result, "_$MyNotifier");
    assert!(edge_to(&result, Some(&notifier.id), &generated_base.id, "inherits").is_some());
    assert!(!nodes(&result).any(|node| node.label == "_"));
    context_edge(&result, "anotherprovider", "riverpod_reference");
    context_edge(&result, "authlogin", "bloc_event");
    context_edge(&result, "authloading", "emit_state");
    context_edge(&result, "authlogin", "bloc_add_event");
    context_edge(&result, "authbloc", "bloc_lookup");
}

#[test]
fn test_namespace_and_spaced_generics() {
    let root = TempDir::new().unwrap();
    let path = write(
        &root,
        "test_namespaces.dart",
        concat!(
            "class MyWidget extends foo.Bar<Map<String, int>> implements ui.Widget, db.Model {}\n",
            "final Map<String, int> myVar = 10;\n",
            "const List<Map<String, int>> myList = [];\n",
            "late final auth.AuthService authService;\n",
            "Map<String, Map<String, int>> myMethod(String a) {}\n",
            "auth.AuthService init() {}\n",
        ),
    );
    let result = extract(&path).unwrap();
    let widget = node(&result, "MyWidget");
    let inheritance = edges(&result)
        .find(|edge| edge.true_source() == widget.id && edge.relation == "inherits")
        .expect("namespaced inheritance");
    assert_ne!(inheritance.true_target(), "foo");
    for label in ["myVar", "myList", "authService", "myMethod", "init"] {
        node(&result, label);
    }
}

#[test]
fn test_dart_and_flutter_specifics() {
    let root = TempDir::new().unwrap();
    let path = write(
        &root,
        "test_specifics.dart",
        concat!(
            "mixin AuthMixin on BaseWidget {}\n",
            "typedef JsonMap = Map<String, dynamic>;\n",
            "extension type UserId(int value) implements Object {}\n",
            "class MyService {\n",
            "  final AuthService api;\n",
            "  MyService(this.api);\n",
            "  factory MyService.fromJson() {}\n",
            "  void navigate(BuildContext context) {\n",
            "    context.go('/home');\n",
            "    Navigator.pushNamed(context, Routes.login);\n",
            "    context.router.push(ProfileRoute());\n",
            "  }\n",
            "}\n",
        ),
    );
    let result = extract(&path).unwrap();
    let mixin = node(&result, "AuthMixin");
    assert!(edge_to(&result, Some(&mixin.id), "basewidget", "inherits").is_some());
    node(&result, "JsonMap");
    node(&result, "api");
    context_edge(&result, "authservice", "variable_type");
    node(&result, "fromJson");
    context_edge(&result, "route_home", "route_path");
    context_edge(&result, "route_routes_login", "route_const");
    context_edge(&result, "profileroute", "route_object");
    let user_id = node(&result, "UserId");
    assert!(edge_to(&result, Some(&user_id.id), "object", "implements").is_some());
}

#[test]
fn test_roadmap_bug_fixes() {
    let root = TempDir::new().unwrap();
    let parent = write(
        &root,
        "parent_lib.dart",
        "library parent_lib;\npart 'child_part.dart';\n",
    );
    let child = write(
        &root,
        "child_part.dart",
        concat!(
            "part of 'parent_lib.dart';\n",
            "class ChildClass extends Bloc<Pair<UserEvent, MyState>, State> {}\n",
            "var User(name: myVar, age: myAge) = user;\n",
            "void runDI(BuildContext context) {\n",
            "  final repo = locator<Repository<User>>();\n",
            "  context.go('/home?id=123&type=auth');\n",
            "}\n",
        ),
    );
    let result = extract(&child).unwrap();
    assert!(!nodes(&result).any(|node| node.label == "child_part.dart"));
    let child_class = node(&result, "ChildClass");
    let parent_id = make_id(&[&fs::canonicalize(parent).unwrap().to_string_lossy()]);
    assert!(edge_to(&result, Some(&parent_id), &child_class.id, "defines").is_some());
    node(&result, "Pair");
    node(&result, "State");
    assert!(!nodes(&result).any(|node| node.id.contains("mystate")));
    node(&result, "Repository");
    node(&result, "myVar");
    node(&result, "myAge");
    assert!(!nodes(&result).any(|node| { matches!(bare_label(&node.label), "name" | "age") }));
    context_edge(&result, "route_home_id_123_type_auth", "route_path");
}

#[test]
fn dart_ambiguous_external_type_lookup_does_not_fan_out() {
    let root = TempDir::new().unwrap();
    let paths = [
        write(&root, "a/service.dart", "class Service {}\n"),
        write(&root, "b/service.dart", "class Service {}\n"),
        write(
            &root,
            "app.dart",
            concat!(
                "// class Phantom {}\n",
                "void use() {\n",
                "  final value = locator<Service>();\n",
                "}\n",
            ),
        ),
    ];
    let result = extract_files(&paths, Some(root.path()), true)
        .unwrap()
        .extractions;
    let definitions = result
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .filter(|node| bare_label(&node.label) == "Service" && !node.source_file.is_empty())
        .map(|node| node.id.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(definitions.len(), 2);
    assert!(!result
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .any(|node| bare_label(&node.label) == "Phantom"));
    let targets = result
        .iter()
        .flat_map(|extraction| &extraction.edges)
        .filter(|edge| {
            edge.relation == "references"
                && edge.extra.get("context").and_then(|value| value.as_str()) == Some("type_lookup")
        })
        .map(|edge| edge.true_target().to_owned())
        .collect::<BTreeSet<_>>();
    assert_eq!(targets.len(), 1);
    assert!(targets.is_disjoint(&definitions));
}

#[test]
fn dart_unique_imported_types_resolve_without_cross_runtime_welding() {
    let root = TempDir::new().unwrap();
    let paths = [
        write(
            &root,
            "dart/worker.dart",
            "class Service {}\nclass Worker {}\n",
        ),
        write(
            &root,
            "dart/runner.dart",
            concat!(
                "import 'worker.dart';\n",
                "class Runner extends Service {}\n",
                "final Service service = Service();\n",
            ),
        ),
        write(&root, "php/Service.php", "<?php class Service {}\n"),
    ];
    let extractions = extract_files(&paths, Some(root.path()), true)
        .unwrap()
        .extractions;
    let dart_service = extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .find(|node| node.source_file == "dart/worker.dart" && bare_label(&node.label) == "Service")
        .expect("Dart Service definition");
    let runner = extractions
        .iter()
        .flat_map(|extraction| &extraction.nodes)
        .find(|node| node.source_file == "dart/runner.dart" && bare_label(&node.label) == "Runner")
        .expect("Dart Runner definition");
    let relevant = extractions
        .iter()
        .flat_map(|extraction| &extraction.edges)
        .filter(|edge| edge.source_file == "dart/runner.dart")
        .collect::<Vec<_>>();
    assert!(relevant.iter().any(|edge| {
        edge.true_source() == runner.id
            && edge.true_target() == dart_service.id
            && edge.relation == "inherits"
    }));
    assert!(relevant.iter().any(|edge| {
        edge.true_target() == dart_service.id
            && edge.relation == "references"
            && edge.extra.get("context").and_then(|value| value.as_str()) == Some("variable_type")
    }));
    assert!(!relevant.iter().any(|edge| {
        edge.true_target().contains("php") && edge.true_target().contains("service")
    }));
}

#[test]
fn dart_bodyless_methods_do_not_borrow_the_next_declarations_body() {
    let root = TempDir::new().unwrap();
    let path = write(
        &root,
        "body_boundaries.dart",
        concat!(
            "abstract class Contract {\n",
            "  void declared();\n",
            "  String concise() => 'ok';\n",
            "}\n",
            "class Later {\n",
            "  void navigate() {\n",
            "    context.go('/later');\n",
            "  }\n",
            "}\n",
        ),
    );
    let result = extract(&path).unwrap();
    let declared = owned_method(&result, "Contract", "declared");
    let concise = owned_method(&result, "Contract", "concise");
    let navigate = owned_method(&result, "Later", "navigate");
    let route = nodes(&result)
        .find(|node| node.label == "Route /later")
        .expect("later route");

    assert!(edge_to(&result, Some(&navigate.id), &route.id, "navigates").is_some());
    assert!(edge_to(&result, Some(&declared.id), &route.id, "navigates").is_none());
    assert!(edge_to(&result, Some(&concise.id), &route.id, "navigates").is_none());
}

#[test]
fn dart_same_named_methods_are_scoped_to_their_owning_classes() {
    let root = TempDir::new().unwrap();
    let path = write(
        &root,
        "method_identity.dart",
        concat!(
            "class Alpha {\n",
            "  void run() { context.go('/alpha'); }\n",
            "}\n",
            "class Beta {\n",
            "  void run() { context.go('/beta'); }\n",
            "}\n",
        ),
    );
    let result = extract(&path).unwrap();
    let alpha_run = owned_method(&result, "Alpha", "run");
    let beta_run = owned_method(&result, "Beta", "run");
    let alpha_route = nodes(&result)
        .find(|node| node.label == "Route /alpha")
        .expect("alpha route");
    let beta_route = nodes(&result)
        .find(|node| node.label == "Route /beta")
        .expect("beta route");

    assert_ne!(alpha_run.id, beta_run.id);
    assert!(edge_to(&result, Some(&alpha_run.id), &alpha_route.id, "navigates").is_some());
    assert!(edge_to(&result, Some(&beta_run.id), &beta_route.id, "navigates").is_some());
    assert!(edge_to(&result, Some(&alpha_run.id), &beta_route.id, "navigates").is_none());
    assert!(edge_to(&result, Some(&beta_run.id), &alpha_route.id, "navigates").is_none());
}
