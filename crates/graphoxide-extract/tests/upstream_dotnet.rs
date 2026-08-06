//! One-to-one executable port of pinned upstream `tests/test_dotnet.py`.

use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use graphoxide_extract::{detect, extract, extract_files};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/upstream")
}

fn extract_fixture(relative: &str) -> Extraction {
    extract(&fixtures().join(relative))
        .unwrap_or_else(|error| panic!("extract {relative}: {error}"))
}

fn labels(result: &Extraction) -> Vec<&str> {
    result
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect()
}

fn context(edge: &Edge) -> Option<&str> {
    edge.extra
        .get("context")
        .and_then(serde_json::Value::as_str)
}

fn view_model_edges(result: &Extraction) -> Vec<&Edge> {
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == "references" && context(edge) == Some("view_model"))
        .collect()
}

fn node_map(result: &Extraction) -> HashMap<&str, &Node> {
    result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect()
}

fn copy_tree(source: &Path, target: &Path) {
    fs::create_dir_all(target).expect("create fixture directory");
    for entry in fs::read_dir(source).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let destination = target.join(entry.file_name());
        if entry.file_type().expect("fixture type").is_dir() {
            copy_tree(&entry.path(), &destination);
        } else {
            fs::copy(entry.path(), destination).expect("copy fixture file");
        }
    }
}

fn collect_suffix(root: &Path, suffixes: &[&str], output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).expect("read project directory") {
        let entry = entry.expect("project entry");
        if entry.file_type().expect("project entry type").is_dir() {
            collect_suffix(&entry.path(), suffixes, output);
        } else if entry
            .path()
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| suffixes.contains(&extension))
        {
            output.push(entry.path());
        }
    }
}

#[test]
fn test_sln_extracts_projects() {
    let result = extract_fixture("sample.sln");
    let labels = labels(&result);
    assert!(labels.contains(&"WebApi"));
    assert!(labels.contains(&"Domain"));
    assert!(labels.contains(&"Tests"));
}

#[test]
fn test_sln_contains_edges() {
    assert_eq!(
        extract_fixture("sample.sln")
            .edges
            .iter()
            .filter(|edge| edge.relation == "contains")
            .count(),
        3
    );
}

#[test]
fn test_sln_project_dependency() {
    assert!(extract_fixture("sample.sln")
        .edges
        .iter()
        .any(|edge| edge.relation == "imports"));
}

#[test]
fn test_sln_solution_folder_ids_are_relative() {
    let temp = tempfile::tempdir().expect("solution fixture");
    let solution = temp.path().join("App.sln");
    fs::write(
        &solution,
        concat!(
            "Microsoft Visual Studio Solution File, Format Version 12.00\n",
            "Project(\"{2150E333-8FDC-42A3-9474-1A3956D46DE8}\") = \"Plugins\", \"Plugins\", \"{11111111-1111-1111-1111-111111111111}\"\n",
            "EndProject\n",
            "Project(\"{FAE04EC0-301F-11D3-BF4B-00C04F79EFBC}\") = \"App\", \"App\\App.csproj\", \"{22222222-2222-2222-2222-222222222222}\"\n",
            "EndProject\n",
        ),
    )
    .expect("write solution");
    let result = extract(&solution).expect("extract solution");
    let folder = result
        .nodes
        .iter()
        .find(|node| node.label == "Plugins")
        .expect("solution folder");
    assert_eq!(folder.id, "plugins");
    assert_eq!(folder.source_file, "Plugins");
    assert!(!folder
        .id
        .contains(&temp.path().to_string_lossy().to_string()));
}

#[test]
fn test_slnx_extracts_projects() {
    let result = extract_fixture("sample.slnx");
    let labels = labels(&result);
    assert!(labels.contains(&"WebApi"));
    assert!(labels.contains(&"Domain"));
    assert!(labels.contains(&"Tests"));
}

#[test]
fn test_slnx_contains_edges() {
    assert_eq!(
        extract_fixture("sample.slnx")
            .edges
            .iter()
            .filter(|edge| edge.relation == "contains")
            .count(),
        3
    );
}

#[test]
fn test_slnx_project_dependency() {
    assert!(extract_fixture("sample.slnx")
        .edges
        .iter()
        .any(|edge| edge.relation == "imports"));
}

#[test]
fn test_slnx_invalid_xml() {
    let temp = tempfile::tempdir().expect("invalid slnx fixture");
    let path = temp.path().join("bad.slnx");
    fs::write(&path, "<Solution><Project></Solution>").expect("write invalid slnx");
    assert!(extract(&path).is_err());
}

#[test]
fn test_slnx_missing_file() {
    assert!(extract(Path::new("/nonexistent/file.slnx")).is_err());
}

#[test]
fn test_csproj_packages() {
    let result = extract_fixture("sample.csproj");
    let labels = labels(&result);
    for package in ["MediatR", "FluentValidation", "Swashbuckle"] {
        assert!(labels.iter().any(|label| label.contains(package)));
    }
}

#[test]
fn test_csproj_project_references() {
    let temp = tempfile::tempdir().expect("project-reference fixture");
    let app = temp.path().join("App");
    fs::create_dir_all(&app).expect("create app directory");
    let project = app.join("sample.csproj");
    fs::copy(fixtures().join("sample.csproj"), &project).expect("copy referencing project");
    for relative in [
        "Domain/Domain.csproj",
        "Infrastructure/Infrastructure.csproj",
    ] {
        let referenced = temp.path().join(relative);
        fs::create_dir_all(referenced.parent().expect("referenced project parent"))
            .expect("create referenced project directory");
        fs::write(referenced, "<Project Sdk=\"Microsoft.NET.Sdk\" />")
            .expect("write referenced project");
    }

    assert_eq!(
        extract(&project)
            .expect("extract project with proven references")
            .edges
            .iter()
            .filter(|edge| edge.relation == "imports")
            .count(),
        6
    );
}

#[test]
fn test_dynamic_csproj_reference_cannot_bind_an_admitted_project() {
    let temp = tempfile::tempdir().expect("dynamic project-reference fixture");
    let app = temp.path().join("App/App.csproj");
    let dynamic_collision = temp.path().join("App/Folder/Worker.csproj");
    let static_target = temp.path().join("App/Static/Static.csproj");
    for target in [&dynamic_collision, &static_target] {
        fs::create_dir_all(target.parent().expect("project parent"))
            .expect("create project parent");
        fs::write(target, "<Project Sdk=\"Microsoft.NET.Sdk\" />")
            .expect("write referenced project");
    }
    fs::write(
        &app,
        concat!(
            "<Project Sdk=\"Microsoft.NET.Sdk\"><ItemGroup>",
            "<ProjectReference Include=\"$(Folder)/Worker.csproj\" />",
            "<ProjectReference Include=\"Static/Static.csproj\" />",
            "</ItemGroup></Project>",
        ),
    )
    .expect("write referencing project");

    let result = extract_files(
        &[app, dynamic_collision, static_target],
        Some(temp.path()),
        true,
    )
    .expect("extract project-reference collision fixture");
    let source = make_id(&["App/App"]);
    let dynamic_target = make_id(&["App/Folder/Worker"]);
    let static_target = make_id(&["App/Static/Static"]);
    let imports = result
        .extractions
        .iter()
        .flat_map(|extraction| &extraction.edges)
        .filter(|edge| edge.relation == "imports" && edge.true_source() == source)
        .map(Edge::true_target)
        .collect::<Vec<_>>();

    assert!(!imports.contains(&dynamic_target.as_str()));
    assert!(imports.contains(&static_target.as_str()));
}

#[test]
fn test_dynamic_slnx_path_cannot_bind_an_admitted_project() {
    let temp = tempfile::tempdir().expect("dynamic SLNX fixture");
    let solution = temp.path().join("Workspace.slnx");
    let dynamic_collision = temp.path().join("Folder/Worker.csproj");
    let static_target = temp.path().join("Static/Static.csproj");
    for target in [&dynamic_collision, &static_target] {
        fs::create_dir_all(target.parent().expect("project parent"))
            .expect("create project parent");
        fs::write(target, "<Project Sdk=\"Microsoft.NET.Sdk\" />").expect("write solution project");
    }
    fs::write(
        &solution,
        concat!(
            "<Solution>",
            "<Project Path=\"$(Folder)/Worker.csproj\" />",
            "<Project Path=\"Static/Static.csproj\" />",
            "</Solution>",
        ),
    )
    .expect("write solution");

    let result = extract_files(
        &[solution, dynamic_collision, static_target],
        Some(temp.path()),
        true,
    )
    .expect("extract SLNX collision fixture");
    let source = make_id(&["Workspace"]);
    let dynamic_target = make_id(&["Folder/Worker"]);
    let static_target = make_id(&["Static/Static"]);
    let contains = result
        .extractions
        .iter()
        .flat_map(|extraction| &extraction.edges)
        .filter(|edge| edge.relation == "contains" && edge.true_source() == source)
        .map(Edge::true_target)
        .collect::<Vec<_>>();

    assert!(!contains.contains(&dynamic_target.as_str()));
    assert!(contains.contains(&static_target.as_str()));
}

#[test]
fn test_csproj_out_of_root_reference_id_is_portable() {
    let temp = tempfile::tempdir().expect("project fixture");
    let web = temp.path().join("WebApi");
    let core = temp.path().join("Core");
    fs::create_dir_all(&web).expect("create web project");
    fs::create_dir_all(&core).expect("create core project");
    fs::write(
        core.join("Core.csproj"),
        "<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><TargetFramework>net8.0</TargetFramework></PropertyGroup></Project>",
    )
    .expect("write core project");
    let project = web.join("WebApi.csproj");
    fs::write(
        &project,
        "<Project Sdk=\"Microsoft.NET.Sdk\"><ItemGroup><ProjectReference Include=\"..\\Core\\Core.csproj\" /></ItemGroup></Project>",
    )
    .expect("write web project");
    let result = extract_files(&[project], Some(&web), true).expect("extract project files");
    let marker = temp.path().to_string_lossy();
    for node in result.extractions.iter().flat_map(|value| &value.nodes) {
        assert!(!node.id.contains(marker.as_ref()));
        assert!(!node.source_file.contains(marker.as_ref()));
    }
    for edge in result.extractions.iter().flat_map(|value| &value.edges) {
        assert!(!edge.true_source().contains(marker.as_ref()));
        assert!(!edge.true_target().contains(marker.as_ref()));
        assert!(!edge.source_file.contains(marker.as_ref()));
    }
    let core = result
        .extractions
        .iter()
        .flat_map(|value| &value.nodes)
        .find(|node| node.id.to_ascii_lowercase().contains("core"))
        .expect("out-of-root project node");
    assert!(core.id.starts_with("ext_"));
    assert_eq!(core.source_file, "../Core/Core.csproj");
}

#[test]
fn test_csproj_target_framework() {
    assert!(labels(&extract_fixture("sample.csproj")).contains(&"net8.0"));
}

#[test]
fn test_csproj_sdk() {
    assert!(labels(&extract_fixture("sample.csproj")).contains(&"Microsoft.NET.Sdk.Web"));
}

#[test]
fn test_csproj_invalid_xml() {
    let temp = tempfile::tempdir().expect("invalid project fixture");
    let path = temp.path().join("bad.csproj");
    fs::write(&path, "<Project><Invalid></Project>").expect("write invalid project");
    assert!(extract(&path).is_err());
}

#[test]
fn test_xaml_class_resolves_to_codebehind_partial_class() {
    let result = extract_fixture("sample.xaml");
    let class = result
        .nodes
        .iter()
        .find(|node| node.label == "MainWindow" && node.source_file.ends_with("sample.xaml.cs"))
        .expect("code-behind class");
    assert!(result.edges.iter().any(|edge| {
        edge.relation == "references"
            && context(edge) == Some("x_class")
            && edge.true_target() == class.id
    }));
}

#[test]
fn test_xaml_named_controls_and_bindings() {
    let result = extract_fixture("sample.xaml");
    let labels = labels(&result);
    for expected in ["RootPanel", "UserNameBox", "SaveButton", "UserName"] {
        assert!(labels.contains(&expected), "missing {expected}");
    }
    assert!(result
        .edges
        .iter()
        .any(|edge| { edge.relation == "references" && context(edge) == Some("binding_path") }));
}

#[test]
fn test_xaml_extracts_binding_paths_commands_and_converters() {
    let result = extract_fixture("bindings.xaml");
    let nodes = node_map(&result);
    let references = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "references")
        .map(|edge| (nodes[edge.true_target()].label.as_str(), context(edge)))
        .collect::<Vec<_>>();
    for expected in [
        ("User.Name", Some("binding_path")),
        ("Order.Total", Some("binding_path")),
        ("Invoice.Tax", Some("binding_path")),
        ("SaveCommand", Some("binding_command")),
        ("MoneyConverter", Some("binding_converter")),
        ("TaxConverter", Some("binding_converter")),
    ] {
        assert!(references.contains(&expected), "missing {expected:?}");
    }
    assert!(!references.contains(&("TwoWay", Some("binding_path"))));
}

#[test]
fn test_xaml_element_datacontext_links_real_viewmodel_class() {
    let result = extract_fixture("xaml_viewmodel/Views/ExplicitMainWindow.xaml");
    let nodes = node_map(&result);
    let edges = view_model_edges(&result);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].confidence, Confidence::Extracted);
    assert_eq!(nodes[edges[0].true_target()].label, "MainViewModel");
    assert!(nodes[edges[0].true_target()]
        .source_file
        .ends_with("MainViewModel.cs"));
}

#[test]
fn test_xaml_design_instance_datacontext_links_real_viewmodel_class() {
    let result = extract_fixture("xaml_viewmodel/Views/DesignView.xaml");
    let nodes = node_map(&result);
    let edges = view_model_edges(&result);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].confidence, Confidence::Extracted);
    assert_eq!(nodes[edges[0].true_target()].label, "DesignViewModel");
}

#[test]
fn test_xaml_infers_viewmodel_by_name_only_without_datacontext() {
    let result = extract_fixture("xaml_viewmodel/Views/SettingsView.xaml");
    let nodes = node_map(&result);
    let edges = view_model_edges(&result);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].confidence, Confidence::Inferred);
    assert_eq!(nodes[edges[0].true_target()].label, "SettingsViewModel");
}

#[test]
fn test_xaml_prism_autowire_infers_viewmodel_from_filename() {
    let result = extract_fixture("xaml_viewmodel/Views/PrismOrderView.xaml");
    let nodes = node_map(&result);
    let edges = view_model_edges(&result);
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].confidence, Confidence::Inferred);
    assert_eq!(nodes[edges[0].true_target()].label, "PrismOrderViewModel");
}

#[test]
fn test_xaml_prism_autowire_false_does_not_infer_from_filename() {
    let temp = tempfile::tempdir().expect("Prism fixture");
    let project = temp.path().join("xaml_viewmodel");
    copy_tree(&fixtures().join("xaml_viewmodel"), &project);
    let xaml = project.join("Views/PrismOrderView.xaml");
    let source = fs::read_to_string(&xaml)
        .expect("read Prism XAML")
        .replace("AutoWireViewModel=\"True\"", "AutoWireViewModel=\"False\"");
    fs::write(&xaml, source).expect("disable Prism autowire");
    assert!(view_model_edges(&extract(&xaml).expect("extract Prism XAML")).is_empty());
}

#[test]
fn test_xaml_cs_scan_prunes_noise_dirs_and_stays_bounded() {
    let temp = tempfile::tempdir().expect("bounded XAML fixture");
    let project = temp.path().join("App");
    fs::create_dir_all(project.join("Views")).expect("create Views");
    fs::create_dir_all(project.join("ViewModels")).expect("create ViewModels");
    fs::write(
        project.join("App.csproj"),
        "<Project Sdk=\"Microsoft.NET.Sdk\" />",
    )
    .expect("write project");
    fs::write(
        project.join("Views/MainWindow.xaml"),
        concat!(
            "<Window x:Class=\"App.Views.MainWindow\" xmlns=\"http://schemas.microsoft.com/winfx/2006/xaml/presentation\" xmlns:x=\"http://schemas.microsoft.com/winfx/2006/xaml\" xmlns:vm=\"clr-namespace:App.ViewModels\">",
            "<Window.DataContext><vm:MainWindowViewModel/></Window.DataContext></Window>",
        ),
    )
    .expect("write XAML");
    fs::write(
        project.join("ViewModels/MainWindowViewModel.cs"),
        "namespace App.ViewModels { public class MainWindowViewModel {} }",
    )
    .expect("write ViewModel");
    fs::create_dir_all(project.join("node_modules/pkg")).expect("create noise directory");
    fs::write(
        project.join("node_modules/pkg/Decoy.cs"),
        "namespace App.ViewModels { public class MainWindowViewModel {} }",
    )
    .expect("write decoy");
    let result = extract(&project.join("Views/MainWindow.xaml")).expect("extract XAML");
    let nodes = node_map(&result);
    let edges = view_model_edges(&result);
    assert_eq!(edges.len(), 1);
    let target = nodes[edges[0].true_target()];
    assert_eq!(target.label, "MainWindowViewModel");
    assert!(!target.source_file.contains("node_modules"));
}

#[test]
fn test_xaml_links_communitytoolkit_generated_members_and_event_to_command() {
    let result = extract_fixture("xaml_viewmodel/Views/ToolkitView.xaml");
    let nodes = node_map(&result);
    let definitions = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "defines")
        .map(|edge| (nodes[edge.true_target()].label.as_str(), context(edge)))
        .collect::<Vec<_>>();
    for expected in [
        ("UserName", Some("communitytoolkit_observable_property")),
        ("Email", Some("communitytoolkit_observable_property")),
        ("SaveCommand", Some("communitytoolkit_relay_command")),
        ("RefreshCommand", Some("communitytoolkit_relay_command")),
    ] {
        assert!(definitions.contains(&expected), "missing {expected:?}");
    }
    assert!(!definitions
        .iter()
        .any(|(label, _)| *label == "IgnoredName" || *label == "IgnoredCommand"));
    for (label, binding_context) in [
        ("UserName", "binding_path"),
        ("Email", "binding_path"),
        ("SaveCommand", "binding_command"),
        ("RefreshCommand", "binding_command"),
    ] {
        assert!(result.edges.iter().any(|edge| {
            edge.relation == "references"
                && context(edge) == Some(binding_context)
                && edge.confidence == Confidence::Inferred
                && nodes[edge.true_target()].label == label
                && nodes[edge.true_target()]
                    .source_file
                    .ends_with("ToolkitViewModel.cs")
        }));
    }
}

#[test]
fn test_extract_preserves_xaml_viewmodel_edge_after_id_remap() {
    let temp = tempfile::tempdir().expect("whole project fixture");
    let project = temp.path().join("xaml_viewmodel");
    copy_tree(&fixtures().join("xaml_viewmodel"), &project);
    let mut files = Vec::new();
    collect_suffix(&project, &["xaml", "cs"], &mut files);
    files.sort();
    let result = extract_files(&files, Some(&project), true).expect("extract whole project");
    let mut merged = Extraction::default();
    for mut extraction in result.extractions {
        merged.nodes.append(&mut extraction.nodes);
        merged.edges.append(&mut extraction.edges);
    }
    let nodes = node_map(&merged);
    let edges = view_model_edges(&merged);
    assert!(edges
        .iter()
        .any(|edge| nodes[edge.true_target()].label == "MainViewModel"));
    assert!(edges
        .iter()
        .any(|edge| nodes[edge.true_target()].label == "DesignViewModel"));
    assert!(edges.iter().any(|edge| {
        nodes[edge.true_target()].label == "SettingsViewModel"
            && edge.confidence == Confidence::Inferred
    }));
}

#[test]
fn test_extract_xaml_viewmodel_resolution_stays_inside_cache_root() {
    let temp = tempfile::tempdir().expect("bounded cache fixture");
    let project = temp.path().join("xaml_viewmodel");
    copy_tree(&fixtures().join("xaml_viewmodel"), &project);
    let xaml = project.join("Views/ExplicitMainWindow.xaml");
    let result = extract_files(&[xaml], Some(&project.join("Views")), true)
        .expect("extract boundary-limited XAML");
    assert!(result
        .extractions
        .iter()
        .flat_map(view_model_edges)
        .next()
        .is_none());
}

#[test]
fn test_xaml_viewmodel_resolution_respects_graphifyignore() {
    let temp = tempfile::tempdir().expect("ignored ViewModel fixture");
    let project = temp.path().join("xaml_viewmodel");
    copy_tree(&fixtures().join("xaml_viewmodel"), &project);
    fs::write(
        project.join(".graphifyignore"),
        "ViewModels/MainViewModel.cs\n",
    )
    .expect("write graphifyignore");
    let result = extract(&project.join("Views/ExplicitMainWindow.xaml")).expect("extract XAML");
    assert!(view_model_edges(&result).is_empty());
}

#[test]
fn test_xaml_ambiguous_viewmodel_names_emit_no_edge() {
    let temp = tempfile::tempdir().expect("ambiguous ViewModel fixture");
    fs::create_dir(temp.path().join("Views")).expect("create Views");
    fs::create_dir(temp.path().join("ViewModels")).expect("create ViewModels");
    fs::write(
        temp.path().join("App.csproj"),
        "<Project Sdk=\"Microsoft.NET.Sdk\" />",
    )
    .expect("write project");
    fs::write(
        temp.path().join("Views/MainWindow.xaml"),
        "<Window x:Class=\"Demo.MainWindow\" xmlns=\"http://schemas.microsoft.com/winfx/2006/xaml/presentation\" xmlns:x=\"http://schemas.microsoft.com/winfx/2006/xaml\"></Window>",
    )
    .expect("write XAML");
    fs::write(
        temp.path().join("ViewModels/MainWindowViewModel.cs"),
        "namespace Demo { public class MainWindowViewModel {} }",
    )
    .expect("write first ViewModel");
    fs::write(
        temp.path().join("ViewModels/MainViewModel.cs"),
        "namespace Demo { public class MainViewModel {} }",
    )
    .expect("write second ViewModel");
    let result = extract(&temp.path().join("Views/MainWindow.xaml")).expect("extract XAML");
    assert!(view_model_edges(&result).is_empty());
}

#[test]
fn test_xaml_events_resolve_to_codebehind_methods() {
    let result = extract_fixture("sample.xaml");
    let methods = result
        .nodes
        .iter()
        .filter(|node| node.source_file.ends_with("sample.xaml.cs"))
        .map(|node| (node.label.trim_matches(['.', '(', ')']), node.id.as_str()))
        .collect::<HashMap<_, _>>();
    for method in ["Window_Loaded", "UserNameChanged", "Save_Click"] {
        let id = methods
            .get(method)
            .unwrap_or_else(|| panic!("missing {method}"));
        assert!(result.edges.iter().any(|edge| {
            edge.relation == "references"
                && context(edge) == Some("event")
                && edge.true_target() == *id
        }));
    }
}

#[test]
fn test_xaml_event_match_requires_handler_signature() {
    let temp = tempfile::tempdir().expect("handler signature fixture");
    let xaml = temp.path().join("view.xaml");
    fs::write(
        &xaml,
        "<Window x:Class=\"Demo.MainWindow\" xmlns=\"http://schemas.microsoft.com/winfx/2006/xaml/presentation\" xmlns:x=\"http://schemas.microsoft.com/winfx/2006/xaml\"><Button Content=\"Refresh\" Click=\"Refresh\"/></Window>",
    )
    .expect("write XAML");
    fs::write(
        temp.path().join("view.xaml.cs"),
        "using System.Windows; namespace Demo { public partial class MainWindow : Window { public void Refresh() {} }}",
    )
    .expect("write code-behind");
    let result = extract(&xaml).expect("extract XAML");
    assert!(!result
        .edges
        .iter()
        .any(|edge| context(edge) == Some("event")));
}

#[test]
fn test_xaml_non_event_attribute_value_does_not_fabricate_event() {
    let temp = tempfile::tempdir().expect("non-event attribute fixture");
    let xaml = temp.path().join("view.xaml");
    fs::write(
        &xaml,
        "<Window x:Class=\"Demo.MainWindow\" xmlns=\"http://schemas.microsoft.com/winfx/2006/xaml/presentation\" xmlns:x=\"http://schemas.microsoft.com/winfx/2006/xaml\"><Button x:Name=\"B1\" Content=\"Save_Click\" Tag=\"OnLoaded\" Click=\"Save_Click\"/></Window>",
    )
    .expect("write XAML");
    fs::write(
        temp.path().join("view.xaml.cs"),
        "using System.Windows; namespace Demo { public partial class MainWindow : Window { private void Save_Click(object sender, RoutedEventArgs e) {} private void OnLoaded(object sender, RoutedEventArgs e) {} }}",
    )
    .expect("write code-behind");
    let result = extract(&xaml).expect("extract XAML");
    let event_edges = result
        .edges
        .iter()
        .filter(|edge| context(edge) == Some("event"))
        .collect::<Vec<_>>();
    assert_eq!(event_edges.len(), 1);
    assert_eq!(
        result
            .nodes
            .iter()
            .find(|node| node.id == event_edges[0].true_target())
            .expect("event target")
            .label,
        ".Save_Click()"
    );
}

#[test]
fn test_xaml_viewmodel_with_non_utf8_codebehind_does_not_crash() {
    let temp = tempfile::tempdir().expect("non-UTF8 ViewModel fixture");
    let project = temp.path().join("xaml_viewmodel");
    copy_tree(&fixtures().join("xaml_viewmodel"), &project);
    let view_model = project.join("ViewModels/SettingsViewModel.cs");
    let mut bytes = vec![0xff];
    bytes.extend_from_slice(b"// stray byte\n");
    bytes.extend(fs::read(&view_model).expect("read ViewModel"));
    fs::write(&view_model, bytes).expect("write non-UTF8 ViewModel");
    let result = extract(&project.join("Views/SettingsView.xaml")).expect("extract XAML");
    let nodes = node_map(&result);
    let edges = view_model_edges(&result);
    assert_eq!(edges.len(), 1);
    assert_eq!(nodes[edges[0].true_target()].label, "SettingsViewModel");
}

#[test]
fn test_razor_using_and_inject() {
    let result = extract_fixture("sample.razor");
    let nodes = node_map(&result);
    let targets = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "imports")
        .map(|edge| nodes[edge.true_target()].label.to_ascii_lowercase())
        .collect::<Vec<_>>();
    assert!(targets.iter().any(|target| target.contains("microsoft")));
    assert!(targets
        .iter()
        .any(|target| target.contains("counterservice")));
}

#[test]
fn test_razor_components() {
    let result = extract_fixture("sample.razor");
    let nodes = node_map(&result);
    let targets = result
        .edges
        .iter()
        .filter(|edge| edge.relation == "calls")
        .map(|edge| nodes[edge.true_target()].label.to_ascii_lowercase())
        .collect::<Vec<_>>();
    assert!(targets
        .iter()
        .any(|target| target.contains("weatherdisplay")));
    assert!(targets.iter().any(|target| target.contains("datagrid")));
}

#[test]
fn test_razor_page_route() {
    assert!(labels(&extract_fixture("sample.razor"))
        .iter()
        .any(|label| label.contains("/counter")));
}

#[test]
fn test_razor_inherits() {
    assert!(extract_fixture("sample.razor")
        .edges
        .iter()
        .any(|edge| edge.relation == "inherits"));
}

#[test]
fn test_razor_code_methods() {
    let result = extract_fixture("sample.razor");
    let labels = labels(&result);
    assert!(labels.contains(&"IncrementCount"));
    assert!(labels.contains(&"LoadData"));
}

#[test]
fn test_razor_missing_file() {
    assert!(extract(Path::new("/nonexistent/file.razor")).is_err());
}

#[test]
fn test_dispatch_table() {
    let temp = TempDir::new().expect("dispatch fixture");
    for (extension, contents) in [
        (
            "sln",
            "Microsoft Visual Studio Solution File, Format Version 12.00\n",
        ),
        ("slnx", "<Solution />"),
        ("csproj", "<Project />"),
        ("fsproj", "<Project />"),
        ("vbproj", "<Project />"),
        ("xaml", "<Window />"),
        ("razor", "@code { }"),
        ("cshtml", "<p>Hello</p>"),
    ] {
        let path = temp.path().join(format!("foo.{extension}"));
        fs::write(&path, contents).expect("write dispatch fixture");
        let result = extract(&path).unwrap_or_else(|error| panic!("{extension}: {error}"));
        assert!(
            !result.nodes.is_empty(),
            "{extension} has no extractor output"
        );
    }
}

#[test]
fn test_code_extensions() {
    let temp = TempDir::new().expect("extension fixture");
    for extension in [
        "sln", "slnx", "csproj", "fsproj", "vbproj", "xaml", "razor", "cshtml",
    ] {
        let path = temp.path().join(format!("foo.{extension}"));
        assert_eq!(detect::classify_file(&path), Some(detect::FileType::Code));
    }
}
