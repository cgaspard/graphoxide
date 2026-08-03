//! One-to-one executable port of pinned Graphify extractor registry,
//! language-resolver registry, and Terraform tests.

use graphoxide_core::{Confidence, Edge, Extraction};
use graphoxide_extract::{
    extract_terraform,
    extractor_registry::{
        registered_extractors, ExtractorFn, ExtractorRegistry, LanguageExtractor,
    },
    resolver_registry::{
        registered_resolvers, run_language_resolvers, LanguageResolver, ResolverRegistry,
    },
};
use graphoxide_graph::build_graph;
use std::{collections::BTreeMap, fs, path::PathBuf};
use tempfile::TempDir;

const SAMPLE: &str = r#"# leading comment so the body is not children[0]
terraform {
  required_providers { azurerm = { source = "hashicorp/azurerm" } }
}

variable "region" { default = "us-east-1" }

provider "aws" { region = var.region }

data "aws_ami" "ubuntu" { most_recent = true }

resource "aws_instance" "web" {
  ami       = data.aws_ami.ubuntu.id
  subnet_id = var.region
  depends_on = [aws_security_group.sg]
}

resource "aws_security_group" "sg" { name = "sg" }

module "vpc" {
  source = "./modules/vpc"
  cidr   = local.cidr
}

locals { cidr = "10.0.0.0/16" }

output "ip" { value = aws_instance.web.private_ip }
"#;

struct Project {
    root: TempDir,
}

impl Project {
    fn new() -> Self {
        Self {
            root: TempDir::new().expect("temporary Terraform project"),
        }
    }

    fn write(&self, relative: &str, source: &str) -> PathBuf {
        let path = self.root.path().join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create Terraform fixture parent");
        }
        fs::write(&path, source).expect("write Terraform fixture");
        path
    }

    fn terraform(&self, relative: &str, source: &str) -> Extraction {
        let path = self.write(relative, source);
        extract_terraform(&path, relative).expect("extract Terraform fixture")
    }
}

fn labels(result: &Extraction) -> Vec<&str> {
    result
        .nodes
        .iter()
        .map(|node| node.label.as_str())
        .collect()
}

fn relation_pairs(result: &Extraction, relation: &str) -> Vec<(String, String)> {
    let labels = result
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect::<BTreeMap<_, _>>();
    result
        .edges
        .iter()
        .filter(|edge| edge.relation == relation)
        .map(|edge| {
            (
                labels
                    .get(edge.true_source())
                    .copied()
                    .unwrap_or(edge.true_source())
                    .to_owned(),
                labels
                    .get(edge.true_target())
                    .copied()
                    .unwrap_or(edge.true_target())
                    .to_owned(),
            )
        })
        .collect()
}

fn marker(relation: &str) -> Edge {
    Edge {
        source: "x".into(),
        target: "y".into(),
        relation: relation.into(),
        confidence: Confidence::Extracted,
        source_file: "a.rb".into(),
        extra: BTreeMap::new(),
    }
}

fn scratch() -> Vec<Extraction> {
    vec![Extraction::default()]
}

fn ruby_resolver(extractions: &mut [Extraction]) -> anyhow::Result<()> {
    extractions[0].edges.push(marker("ruby"));
    Ok(())
}

fn go_resolver(extractions: &mut [Extraction]) -> anyhow::Result<()> {
    extractions[0].edges.push(marker("go"));
    Ok(())
}

fn first_resolver(extractions: &mut [Extraction]) -> anyhow::Result<()> {
    extractions[0].edges.push(marker("first"));
    Ok(())
}

fn second_resolver(extractions: &mut [Extraction]) -> anyhow::Result<()> {
    extractions[0].edges.push(marker("second"));
    Ok(())
}

fn failing_resolver(_: &mut [Extraction]) -> anyhow::Result<()> {
    anyhow::bail!("resolver blew up")
}

fn panicking_resolver(_: &mut [Extraction]) -> anyhow::Result<()> {
    panic!("resolver panicked")
}

fn add_call_edge(extractions: &mut [Extraction]) -> anyhow::Result<()> {
    extractions[0].edges.push(marker("calls"));
    Ok(())
}

fn dummy_extractor(_: &std::path::Path, _: &str) -> anyhow::Result<Extraction> {
    Ok(Extraction::default())
}

#[test]
fn test_every_registry_extractor_is_reexported_from_facade() {
    assert!(!registered_extractors().is_empty());
    for extractor in registered_extractors() {
        let facade = match extractor.name {
            "terraform" => extract_terraform as ExtractorFn,
            unknown => panic!("registry extractor {unknown:?} has no public facade export"),
        };
        assert!(std::ptr::fn_addr_eq(extractor.extract, facade));
    }
}

#[test]
fn test_terraform_migrated() {
    let terraform = registered_extractors()
        .iter()
        .find(|extractor| extractor.name == "terraform")
        .expect("registered Terraform extractor");
    assert!(std::ptr::fn_addr_eq(
        terraform.extract,
        extract_terraform as ExtractorFn,
    ));
    for path in ["main.tf", "terraform.tfvars", "config.hcl"] {
        assert!(terraform.supports(std::path::Path::new(path)));
    }
}

#[test]
fn test_default_registry_contains_swift_then_python() {
    let names = registered_resolvers()
        .iter()
        .map(|resolver| resolver.name)
        .collect::<Vec<_>>();
    let swift = names
        .iter()
        .position(|name| *name == "swift_member_calls")
        .expect("Swift resolver registered");
    let python = names
        .iter()
        .position(|name| *name == "python_member_calls")
        .expect("Python resolver registered");
    assert!(swift < python);
}

#[test]
fn test_resolver_runs_only_when_suffix_present() {
    let mut result = scratch();
    let failures = run_language_resolvers(
        &[PathBuf::from("a.rb")],
        &mut result,
        &[
            LanguageResolver::new("ruby", &[".rb"], ruby_resolver),
            LanguageResolver::new("go", &[".go"], go_resolver),
        ],
    );
    assert!(failures.is_empty());
    assert_eq!(
        result[0]
            .edges
            .iter()
            .map(|edge| edge.relation.as_str())
            .collect::<Vec<_>>(),
        ["ruby"]
    );
}

#[test]
fn test_resolvers_run_in_given_order() {
    let mut result = scratch();
    run_language_resolvers(
        &[PathBuf::from("a.rb")],
        &mut result,
        &[
            LanguageResolver::new("first", &["rb"], first_resolver),
            LanguageResolver::new("second", &["rb"], second_resolver),
        ],
    );
    assert_eq!(
        result[0]
            .edges
            .iter()
            .map(|edge| edge.relation.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );
}

#[test]
fn test_failing_resolver_is_isolated() {
    let mut result = scratch();
    let failures = run_language_resolvers(
        &[PathBuf::from("a.rb")],
        &mut result,
        &[
            LanguageResolver::new("boom", &["rb"], failing_resolver),
            LanguageResolver::new("after", &["rb"], second_resolver),
        ],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].name, "boom");
    assert_eq!(result[0].edges[0].relation, "second");
}

#[test]
fn test_resolver_mutates_edges_in_place() {
    let mut result = scratch();
    run_language_resolvers(
        &[PathBuf::from("a.rb")],
        &mut result,
        &[LanguageResolver::new("adder", &["rb"], add_call_edge)],
    );
    let edge = &result[0].edges[0];
    assert_eq!(
        (
            edge.source.as_str(),
            edge.target.as_str(),
            edge.relation.as_str()
        ),
        ("x", "y", "calls")
    );
}

#[test]
fn test_no_error_and_all_block_types_become_nodes() {
    let project = Project::new();
    let result = project.terraform("main.tf", SAMPLE);
    let labels = labels(&result);
    for expected in [
        "var.region",
        "provider.aws",
        "data.aws_ami.ubuntu",
        "aws_instance.web",
        "aws_security_group.sg",
        "module.vpc",
        "local.cidr",
        "output.ip",
    ] {
        assert!(
            labels.contains(&expected),
            "missing {expected:?}: {labels:?}"
        );
    }
    assert!(!labels.contains(&"terraform"));
}

#[test]
fn test_reference_edges() {
    let project = Project::new();
    let result = project.terraform("main.tf", SAMPLE);
    let references = relation_pairs(&result, "references");
    for expected in [
        ("provider.aws", "var.region"),
        ("aws_instance.web", "data.aws_ami.ubuntu"),
        ("aws_instance.web", "var.region"),
        ("module.vpc", "local.cidr"),
        ("output.ip", "aws_instance.web"),
    ] {
        assert!(
            references
                .iter()
                .any(|pair| pair.0 == expected.0 && pair.1 == expected.1),
            "missing reference {expected:?}: {references:?}"
        );
    }
}

#[test]
fn test_depends_on_edge() {
    let project = Project::new();
    let result = project.terraform("main.tf", SAMPLE);
    assert!(relation_pairs(&result, "depends_on")
        .iter()
        .any(|pair| pair == &("aws_instance.web".into(), "aws_security_group.sg".into())));
}

#[test]
fn test_file_contains_blocks() {
    let project = Project::new();
    let result = project.terraform("main.tf", SAMPLE);
    let contains = relation_pairs(&result, "contains");
    for expected in ["aws_instance.web", "var.region"] {
        assert!(contains
            .iter()
            .any(|pair| pair == &("main.tf".into(), expected.into())));
    }
}

#[test]
fn test_meta_heads_not_emitted() {
    let project = Project::new();
    let result = project.terraform(
        "main.tf",
        r#"resource "aws_instance" "web" {
  count = 2
  name  = "web-${count.index}"
  tags  = each.value
  dir   = path.module
}
"#,
    );
    let targets = relation_pairs(&result, "references")
        .into_iter()
        .map(|(_, target)| target)
        .collect::<Vec<_>>();
    assert!(targets.iter().all(|target| !target.starts_with("count")
        && !target.starts_with("each")
        && !target.starts_with("path")));
}

#[test]
fn test_cross_file_references_resolve_after_merge() {
    let project = Project::new();
    let definition = project.terraform(
        "main.tf",
        "resource \"azurerm_resource_group\" \"main\" { name = \"rg\" }\n",
    );
    let user = project.terraform(
        "nic.tf",
        "resource \"azurerm_network_interface\" \"nic\" {\n  resource_group_name = azurerm_resource_group.main.name\n}\n",
    );
    let target = definition
        .nodes
        .iter()
        .find(|node| node.label == "azurerm_resource_group.main")
        .expect("resource group definition")
        .id
        .clone();
    assert!(user
        .edges
        .iter()
        .any(|edge| { edge.relation == "references" && edge.true_target() == target }));
    let graph = build_graph(&[definition, user]).expect("merge Terraform graph");
    assert!(graph
        .links
        .iter()
        .any(|edge| { edge.relation == "references" && edge.true_target() == target }));
}

#[test]
fn test_empty_and_commentonly_files_are_safe() {
    let project = Project::new();
    assert_eq!(project.terraform("a.tf", "").nodes.len(), 1);
    assert_eq!(
        project.terraform("b.tf", "# just a comment\n").nodes.len(),
        1
    );
}

#[test]
fn test_tfvars_key_value_is_safe() {
    let project = Project::new();
    let result = project.terraform(
        "terraform.tfvars",
        "region = \"us-east-1\"\nenv = \"prod\"\n",
    );
    assert_eq!(result.nodes.len(), 1);
}

#[test]
fn duplicate_registry_names_are_rejected_without_reordering() {
    let mut resolvers = ResolverRegistry::default();
    resolvers
        .register(LanguageResolver::new("same", &["rb"], first_resolver))
        .unwrap();
    assert!(resolvers
        .register(LanguageResolver::new("same", &["go"], second_resolver))
        .is_err());
    assert_eq!(resolvers.entries().len(), 1);

    let mut extractors = ExtractorRegistry::default();
    extractors
        .register(LanguageExtractor::new("same", &["one"], dummy_extractor))
        .unwrap();
    assert!(extractors
        .register(LanguageExtractor::new("same", &["two"], dummy_extractor))
        .is_err());
    assert_eq!(extractors.entries().len(), 1);
}

#[test]
fn panicking_resolver_does_not_block_later_resolvers() {
    let mut result = scratch();
    let failures = run_language_resolvers(
        &[PathBuf::from("a.rb")],
        &mut result,
        &[
            LanguageResolver::new("panic", &["rb"], panicking_resolver),
            LanguageResolver::new("after", &["rb"], second_resolver),
        ],
    );
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].name, "panic");
    assert_eq!(result[0].edges[0].relation, "second");
}

#[test]
fn terraform_uses_registered_production_dispatch() {
    let project = Project::new();
    let path = project.write(
        "main.tf",
        "resource \"aws_instance\" \"web\" { ami = var.ami }\n",
    );
    let result = graphoxide_extract::extract(&path).expect("dispatch registered Terraform");
    assert!(labels(&result).contains(&"aws_instance.web"));
    assert!(result.nodes.iter().all(|node| {
        node.extra
            .get("_origin")
            .and_then(serde_json::Value::as_str)
            == Some("terraform")
    }));
}

#[test]
fn terraform_comments_and_plain_strings_do_not_create_phantom_references() {
    let project = Project::new();
    let result = project.terraform(
        "main.tf",
        r#"resource "aws_instance" "web" {
  # fake.example.id
  name = "literal.example.value"
  real = "${var.region}"
}
"#,
    );
    let targets = relation_pairs(&result, "references")
        .into_iter()
        .map(|(_, target)| target)
        .collect::<Vec<_>>();
    assert!(targets.iter().any(|target| target == "var_region"));
    assert!(targets.iter().all(|target| !target.contains("fake")));
    assert!(targets.iter().all(|target| !target.contains("literal")));
}

#[test]
fn terraform_duplicate_addresses_do_not_duplicate_nodes_or_edges() {
    let project = Project::new();
    let result = project.terraform(
        "main.tf",
        "resource \"aws_instance\" \"web\" { ami = var.ami }\nresource \"aws_instance\" \"web\" { ami = var.ami }\n",
    );
    assert_eq!(
        result
            .nodes
            .iter()
            .filter(|node| node.label == "aws_instance.web")
            .count(),
        1
    );
    assert_eq!(
        result
            .edges
            .iter()
            .filter(|edge| edge.relation == "references")
            .count(),
        1
    );
}

#[test]
fn nested_object_keys_do_not_become_top_level_locals() {
    let project = Project::new();
    let result = project.terraform(
        "main.tf",
        r#"locals {
  config = {
    region = var.region
  }
  direct = var.direct
}
"#,
    );
    let found = labels(&result);
    assert!(found.contains(&"local.config"));
    assert!(found.contains(&"local.direct"));
    assert!(!found.contains(&"local.region"));
    let references = relation_pairs(&result, "references");
    assert!(references
        .iter()
        .any(|pair| pair == &("local.config".into(), "var_region".into())));
    assert!(references
        .iter()
        .any(|pair| pair == &("local.direct".into(), "var_direct".into())));
}

#[test]
fn uppercase_suffix_gating_is_case_insensitive() {
    let mut result = scratch();
    run_language_resolvers(
        &[PathBuf::from("A.RB")],
        &mut result,
        &[LanguageResolver::new("ruby", &[".rb"], ruby_resolver)],
    );
    assert_eq!(result[0].edges.len(), 1);
}

#[test]
fn registry_callbacks_mutate_the_original_extraction_allocation() {
    let mut result = scratch();
    let pointer = result.as_ptr();
    run_language_resolvers(
        &[PathBuf::from("a.rb")],
        &mut result,
        &[LanguageResolver::new("adder", &["rb"], add_call_edge)],
    );
    assert_eq!(result.as_ptr(), pointer);
    assert_eq!(result[0].edges.len(), 1);
}
