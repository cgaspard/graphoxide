use graphoxide_core::{normalize_id, Edge, KnowledgeGraph, Node};
use graphoxide_extract::extract_project_with_options_and_output;
use graphoxide_graph::build_graph;
use std::{fs, path::Path};

const SOLUTION: &str = r#"Microsoft Visual Studio Solution File, Format Version 12.00
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "WebApi", "src\WebApi\WebApi.csproj", "{11111111-1111-1111-1111-111111111111}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Domain", "src\Domain\Domain.csproj", "{22222222-2222-2222-2222-222222222222}"
EndProject
Project("{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}") = "Tests", "tests\Tests\Tests.csproj", "{33333333-3333-3333-3333-333333333333}"
EndProject
"#;

const PROJECT: &str = r#"<Project Sdk="Microsoft.NET.Sdk.Web">
  <PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup>
  <ItemGroup>
    <ProjectReference Include="..\Domain\Domain.csproj" />
    <ProjectReference Include="..\Infrastructure\Infrastructure.csproj" />
  </ItemGroup>
</Project>
"#;

const XAML: &str = r#"<Window x:Class="Portable.MainWindow"
        xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation"
        xmlns:x="http://schemas.microsoft.com/winfx/2006/xaml"
        Loaded="Window_Loaded">
    <Button x:Name="SaveButton" Click="Save_Click" />
</Window>
"#;

const CODE_BEHIND: &str = r#"using System.Windows;
namespace Portable;
public partial class MainWindow : Window
{
    private void Window_Loaded(object sender, RoutedEventArgs e) { }
    private void Save_Click(object sender, RoutedEventArgs e) { }
}
"#;

type NodeSignature = (String, String, String);
type EdgeSignature = (String, String, String, String);
type GraphSignature = (Vec<NodeSignature>, Vec<EdgeSignature>);

fn write_fixture(root: &Path) {
    fs::create_dir_all(root).expect("create fixture root");
    fs::write(root.join("sample.sln"), SOLUTION).expect("write solution");
    fs::write(root.join("sample.csproj"), PROJECT).expect("write project");
    fs::write(root.join("sample.xaml"), XAML).expect("write XAML");
    fs::write(root.join("sample.xaml.cs"), CODE_BEHIND).expect("write code-behind");

    // Parent-escaping project references are retained only when the path-owning
    // entrypoint can prove that the referenced project is a real file.
    let parent = root.parent().expect("fixture parent");
    for relative in [
        "Domain/Domain.csproj",
        "Infrastructure/Infrastructure.csproj",
    ] {
        let project = parent.join(relative);
        fs::create_dir_all(project.parent().expect("external project parent"))
            .expect("create external project directory");
        fs::write(project, "<Project />").expect("write external project");
    }
}

fn fixture_graph(parent: &Path, checkout_name: &str) -> (std::path::PathBuf, KnowledgeGraph) {
    let root = parent.join(checkout_name);
    write_fixture(&root);
    let chunks = extract_project_with_options_and_output(
        &root,
        true,
        &parent.join(format!("{checkout_name}-cache")),
    )
    .expect("extract fixture corpus");
    let graph = build_graph(&chunks).expect("build fixture graph");
    (root, graph)
}

fn node<'a>(nodes: &'a [Node], label: &str) -> &'a Node {
    nodes
        .iter()
        .find(|node| node.label == label)
        .unwrap_or_else(|| panic!("missing node {label:?}"))
}

fn assert_no_checkout_path(graph: &KnowledgeGraph, root: &Path) {
    let root_text = root.to_string_lossy().replace('\\', "/");
    let root_id = normalize_id(&root_text);
    for node in &graph.nodes {
        assert!(
            !Path::new(&node.source_file).is_absolute(),
            "absolute node source_file survived: {node:?}"
        );
        assert!(
            !node.source_file.contains(&root_text),
            "checkout path survived in node source_file: {node:?}"
        );
        assert!(
            !node.id.contains(&root_id),
            "checkout-derived node id survived: {node:?}"
        );
    }
    for edge in &graph.links {
        assert!(
            !Path::new(&edge.source_file).is_absolute(),
            "absolute edge source_file survived: {edge:?}"
        );
        assert!(
            !edge.source_file.contains(&root_text),
            "checkout path survived in edge source_file: {edge:?}"
        );
        assert!(
            !edge.true_source().contains(&root_id) && !edge.true_target().contains(&root_id),
            "checkout-derived edge endpoint survived: {edge:?}"
        );
    }
}

fn graph_signature(graph: &KnowledgeGraph) -> GraphSignature {
    let mut nodes = graph
        .nodes
        .iter()
        .map(|node| {
            (
                node.id.clone(),
                node.label.clone(),
                node.source_file.clone(),
            )
        })
        .collect::<Vec<_>>();
    nodes.sort();
    let mut edges = graph
        .links
        .iter()
        .map(|edge: &Edge| {
            (
                edge.true_source().to_owned(),
                edge.true_target().to_owned(),
                edge.relation.clone(),
                edge.source_file.clone(),
            )
        })
        .collect::<Vec<_>>();
    edges.sort();
    (nodes, edges)
}

#[test]
fn dotnet_linked_nodes_use_portable_upstream_identities() {
    let temp = tempfile::tempdir().expect("create test directory");
    let (root, graph) = fixture_graph(temp.path(), "checkout-a");

    assert_no_checkout_path(&graph, &root);

    let expected = [
        ("WebApi", "src_webapi_webapi", "src/WebApi/WebApi.csproj"),
        ("Domain", "src_domain_domain", "src/Domain/Domain.csproj"),
        ("Tests", "tests_tests_tests", "tests/Tests/Tests.csproj"),
        (
            "Domain.csproj",
            "ext_domain_domain_csproj",
            "../Domain/Domain.csproj",
        ),
        (
            "Infrastructure.csproj",
            "ext_infrastructure_infrastructure_csproj",
            "../Infrastructure/Infrastructure.csproj",
        ),
    ];
    for (label, id, source_file) in expected {
        let actual = node(&graph.nodes, label);
        assert_eq!(actual.id, id, "unexpected id for {label}");
        assert_eq!(
            actual.source_file, source_file,
            "unexpected source_file for {label}"
        );
    }

    for handler in [".Window_Loaded()", ".Save_Click()"] {
        let matching = graph
            .nodes
            .iter()
            .filter(|node| node.label == handler)
            .collect::<Vec<_>>();
        assert!(!matching.is_empty(), "missing XAML handler {handler}");
        assert!(
            matching
                .iter()
                .all(|node| node.source_file == "sample.xaml.cs"),
            "XAML handler kept a physical code-behind path: {matching:?}"
        );
    }
}

#[test]
fn dotnet_graph_identity_is_stable_across_checkout_roots() {
    let first = tempfile::tempdir().expect("create first checkout parent");
    let second = tempfile::tempdir().expect("create second checkout parent");
    let (first_root, first_graph) = fixture_graph(first.path(), "alpha-checkout");
    let (second_root, second_graph) = fixture_graph(second.path(), "beta-checkout");

    assert_no_checkout_path(&first_graph, &first_root);
    assert_no_checkout_path(&second_graph, &second_root);
    assert_eq!(
        graph_signature(&first_graph),
        graph_signature(&second_graph),
        "the same .NET corpus produced checkout-dependent graph identity"
    );
}
