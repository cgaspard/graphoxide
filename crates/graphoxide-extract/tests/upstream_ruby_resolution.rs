use graphoxide_core::{make_id, Confidence, Edge, Extraction, Node};
use graphoxide_extract::extract_project_with_options;
use std::{collections::BTreeSet, fs, path::PathBuf};
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

    fn write(&self, relative: &str, body: &str) -> PathBuf {
        let path = self.root.path().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    fn extract(&self) -> Vec<Extraction> {
        extract_project_with_options(self.root.path(), true).unwrap()
    }

    fn raw(&self, relative: &str) -> Extraction {
        graphoxide_extract::extract(&self.root.path().join(relative)).unwrap()
    }
}

fn nodes(extractions: &[Extraction]) -> impl Iterator<Item = &Node> {
    extractions.iter().flat_map(|extraction| &extraction.nodes)
}

fn edges(extractions: &[Extraction]) -> impl Iterator<Item = &Edge> {
    extractions.iter().flat_map(|extraction| &extraction.edges)
}

fn labels(extractions: &[Extraction]) -> BTreeSet<String> {
    nodes(extractions).map(|node| node.label.clone()).collect()
}

fn node_id(extractions: &[Extraction], label: &str) -> String {
    nodes(extractions)
        .find(|node| node.label == label)
        .unwrap_or_else(|| panic!("missing node {label:?}"))
        .id
        .clone()
}

fn call_edge<'a>(
    extractions: &'a [Extraction],
    source_label: &str,
    target_label: &str,
) -> Option<&'a Edge> {
    let source_ids = nodes(extractions)
        .filter(|node| node.label.contains(source_label))
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    let target_ids = nodes(extractions)
        .filter(|node| node.label.contains(target_label))
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    edges(extractions).find(|edge| {
        edge.relation == "calls"
            && source_ids.contains(edge.true_source())
            && target_ids.contains(edge.true_target())
    })
}

fn raw_call<'a>(extraction: &'a Extraction, callee: &str) -> Option<&'a Edge> {
    extraction.edges.iter().find(|edge| {
        edge.relation == "calls"
            && edge.extra.get("callee").and_then(serde_json::Value::as_str) == Some(callee)
    })
}

fn method_edges(extractions: &[Extraction]) -> BTreeSet<(String, String)> {
    let label_by_id = nodes(extractions)
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    edges(extractions)
        .filter(|edge| edge.relation == "method")
        .map(|edge| {
            (
                label_by_id
                    .get(edge.true_source())
                    .copied()
                    .unwrap_or("")
                    .to_owned(),
                label_by_id
                    .get(edge.true_target())
                    .copied()
                    .unwrap_or("")
                    .to_owned(),
            )
        })
        .collect()
}

fn mixes_in(extractions: &[Extraction]) -> BTreeSet<(String, String)> {
    let label_by_id = nodes(extractions)
        .map(|node| (node.id.as_str(), node.label.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    edges(extractions)
        .filter(|edge| edge.relation == "mixes_in")
        .map(|edge| {
            (
                label_by_id
                    .get(edge.true_source())
                    .copied()
                    .unwrap_or("")
                    .to_owned(),
                label_by_id
                    .get(edge.true_target())
                    .copied()
                    .unwrap_or("")
                    .to_owned(),
            )
        })
        .collect()
}

const HELPER_RB: &str = r#"def transform(data)
  data.upcase
end

class Processor
  def run(items)
    items.map { |i| transform(i) }
  end
end
"#;

const MAIN_RB: &str = r#"require_relative "helper"

def handle(values)
  transform(values)
end

def process_all(items)
  p = Processor.new
  p.run(items)
end
"#;

const WORKER_RB: &str = r#"class Worker
  def run(jobs)
    jobs.each { |j| j }
  end
end
"#;

#[test]
fn test_require_relative_emits_only_static_literal_imports() {
    let project = Project::new();
    project.write(
        "lib/runner.rb",
        concat!(
            "folder = 'folder'\n",
            "require_relative \"#{folder}/worker\"\n",
            "require_relative 'static_worker'\n",
            "require_relative '.'\n",
            "require_relative 'folder/..'\n",
            "require_relative 'folder/'\n",
            "require_relative 'scheme:worker'\n",
        ),
    );

    let extraction = project.raw("lib/runner.rb");
    let imports = extraction
        .edges
        .iter()
        .filter(|edge| edge.relation == "imports_from")
        .collect::<Vec<_>>();
    assert_eq!(imports.len(), 1, "dynamic Ruby paths must not emit imports");
    assert!(imports[0].true_target().ends_with("lib_static_worker"));
    assert!(!imports[0].true_target().contains("folder_worker"));
}

#[test]
fn test_dynamic_require_relative_cannot_bind_an_admitted_file() {
    let project = Project::new();
    project.write("lib/folder/worker.rb", "class Worker\nend\n");
    project.write(
        "lib/dynamic.rb",
        "folder = 'folder'\nrequire_relative \"#{folder}/worker\"\n",
    );
    project.write("lib/literal.rb", "require_relative 'folder/worker'\n");

    let result = project.extract();
    let dynamic = make_id(&["lib/dynamic"]);
    let literal = make_id(&["lib/literal"]);
    let worker = make_id(&["lib/folder/worker"]);

    assert!(!edges(&result).any(|edge| {
        edge.relation == "imports_from"
            && edge.true_source() == dynamic
            && edge.true_target() == worker
    }));
    assert!(edges(&result).any(|edge| {
        edge.relation == "imports_from"
            && edge.true_source() == literal
            && edge.true_target() == worker
    }));
}

#[test]
fn test_require_relative_normalizes_only_contained_portable_paths() {
    let project = Project::new();
    project.write("worker.rb", "class RootWorker\nend\n");
    project.write("lib/worker.rb", "class NestedWorker\nend\n");
    project.write("lib/contained.rb", "require_relative '../worker'\n");
    project.write("lib/escaping.rb", "require_relative '../../worker'\n");
    project.write(
        "lib/absolute.rb",
        concat!(
            "require_relative '/worker'\n",
            "require_relative 'C:/worker'\n",
            "require_relative 'C:worker'\n",
            "require_relative '//server/share/worker'\n",
            "require_relative '\\\\server\\share\\worker'\n",
        ),
    );

    let result = project.extract();
    let root_worker = make_id(&["worker"]);
    let nested_worker = make_id(&["lib/worker"]);
    let contained = make_id(&["lib/contained"]);
    let escaping = make_id(&["lib/escaping"]);
    let absolute = make_id(&["lib/absolute"]);

    assert!(edges(&result).any(|edge| {
        edge.relation == "imports_from"
            && edge.true_source() == contained
            && edge.true_target() == root_worker
    }));
    assert!(!edges(&result).any(|edge| {
        edge.relation == "imports_from"
            && edge.true_source() == contained
            && edge.true_target() == nested_worker
    }));
    assert!(!edges(&result).any(|edge| {
        edge.relation == "imports_from"
            && matches!(edge.true_source(), source if source == escaping || source == absolute)
            && matches!(edge.true_target(), target if target == root_worker || target == nested_worker)
    }));
}

#[test]
fn test_member_call_captures_receiver() {
    let project = Project::new();
    project.write("main.rb", MAIN_RB);
    let extraction = project.raw("main.rb");
    let call = raw_call(&extraction, "run").expect("p.run raw call");
    assert_eq!(
        call.extra
            .get("member_call")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        call.extra
            .get("receiver")
            .and_then(serde_json::Value::as_str),
        Some("p")
    );
}

#[test]
fn test_local_binding_gives_receiver_a_type() {
    let project = Project::new();
    project.write("main.rb", MAIN_RB);
    let extraction = project.raw("main.rb");
    assert_eq!(
        raw_call(&extraction, "run")
            .and_then(|call| call.extra.get("receiver_type"))
            .and_then(serde_json::Value::as_str),
        Some("Processor")
    );
}

#[test]
fn test_constructor_assignment_after_call_does_not_type_receiver() {
    let project = Project::new();
    project.write(
        "main.rb",
        "class Service\n  def process; end\nend\n\ndef run\n  svc.process; svc = Service.new\nend\n",
    );
    let extraction = project.raw("main.rb");
    assert!(!raw_call(&extraction, "process")
        .expect("svc.process raw call")
        .extra
        .contains_key("receiver_type"));

    let result = project.extract();
    assert!(call_edge(&result, "run", ".process()").is_none());
}

#[test]
fn test_preceding_non_constructor_assignment_keeps_receiver_ambiguous() {
    let project = Project::new();
    project.write(
        "main.rb",
        "def run(source)\n  svc = Service.new\n  svc = source\n  svc.process\nend\n",
    );
    let extraction = project.raw("main.rb");
    assert!(!raw_call(&extraction, "process")
        .expect("svc.process raw call")
        .extra
        .contains_key("receiver_type"));
}

#[test]
fn test_ambiguous_binding_yields_no_type() {
    let project = Project::new();
    project.write(
        "main.rb",
        "def process_all(items)\n  p = Processor.new\n  p = Worker.new\n  p.run(items)\nend\n",
    );
    let extraction = project.raw("main.rb");
    assert!(!raw_call(&extraction, "run")
        .expect("p.run raw call")
        .extra
        .contains_key("receiver_type"));
}

#[test]
fn test_resolves_member_call_by_type() {
    let project = Project::new();
    project.write("helper.rb", HELPER_RB);
    project.write("main.rb", MAIN_RB);
    let result = project.extract();
    let edge = call_edge(&result, "process_all", ".run()").expect("typed member call");
    assert_eq!(edge.confidence, Confidence::Extracted);
    assert!(
        call_edge(&result, "handle", "transform").is_some(),
        "require_relative should make the helper's free function resolvable"
    );
}

#[test]
fn test_resolution_is_type_based_not_name_luck() {
    let project = Project::new();
    project.write("helper.rb", HELPER_RB);
    project.write("worker.rb", WORKER_RB);
    project.write("main.rb", MAIN_RB);
    let result = project.extract();
    let edge = call_edge(&result, "process_all", ".run()").expect("typed member call");
    assert_eq!(edge.confidence, Confidence::Extracted);
    assert!(edge.true_target().contains("processor"));
    assert!(!edge.true_target().contains("worker"));
}

#[test]
fn test_no_false_positive_when_type_unknown() {
    let project = Project::new();
    project.write("helper.rb", HELPER_RB);
    project.write(
        "main.rb",
        "require_relative \"helper\"\n\ndef process_all(thing)\n  thing.run(1)\nend\n",
    );
    let result = project.extract();
    assert!(call_edge(&result, "process_all", ".run()").is_none());
}

#[test]
fn test_class_new_creates_instantiation_edge() {
    let project = Project::new();
    project.write("helper.rb", HELPER_RB);
    project.write("main.rb", MAIN_RB);
    let result = project.extract();
    let edge = call_edge(&result, "process_all", "Processor").expect("Processor.new edge");
    assert_eq!(edge.confidence, Confidence::Extracted);
}

#[test]
fn test_plain_module_gets_a_node_with_methods() {
    let project = Project::new();
    project.write(
        "tax.rb",
        "module TaxCalculator\n  module_function\n  def rate_for(order)\n    0.2\n  end\nend\n",
    );
    let result = project.extract();
    assert!(labels(&result).contains("TaxCalculator"));
    assert!(method_edges(&result).contains(&("TaxCalculator".into(), ".rate_for()".into())));
}

#[test]
fn test_nested_modules_each_get_a_node() {
    let project = Project::new();
    project.write(
        "n.rb",
        "module Billing\n  module Rounding\n    def round(x)\n      x.round(2)\n    end\n  end\nend\n",
    );
    let result = project.extract();
    assert!(labels(&result).contains("Billing"));
    assert!(labels(&result).contains("Billing::Rounding"));
    assert!(method_edges(&result).contains(&("Billing::Rounding".into(), ".round()".into())));
}

#[test]
fn test_nested_superclass_resolves_lexically_despite_an_unrelated_short_name() {
    let project = Project::new();
    project.write(
        "matrix/worker.rb",
        "module MatrixRuntime\n  class Worker\n  end\nend\n",
    );
    project.write(
        "matrix/runner.rb",
        "require_relative 'worker'\nmodule MatrixRuntime\n  class Runner < Worker\n  end\nend\n",
    );
    project.write("other/worker.rb", "class Worker\nend\n");
    let result = project.extract();
    let nested_worker = node_id(&result, "MatrixRuntime::Worker");
    let runner = node_id(&result, "MatrixRuntime::Runner");
    assert!(edges(&result).any(|edge| {
        edge.relation == "inherits"
            && edge.true_source() == runner
            && edge.true_target() == nested_worker
    }));
}

#[test]
fn test_struct_new_constant_creates_class_with_methods() {
    let project = Project::new();
    project.write(
        "invoice.rb",
        "Invoice = Struct.new(:total, :tax) do\n  def grand_total\n    total + tax\n  end\nend\n",
    );
    let result = project.extract();
    assert!(labels(&result).contains("Invoice"));
    assert!(method_edges(&result).contains(&("Invoice".into(), ".grand_total()".into())));
}

#[test]
fn test_class_new_constant_creates_class_and_inherits() {
    let project = Project::new();
    project.write("err.rb", "ApiError = Class.new(StandardError)\n");
    let result = project.extract();
    let source = node_id(&result, "ApiError");
    let target = node_id(&result, "StandardError");
    assert!(edges(&result).any(|edge| {
        edge.relation == "inherits" && edge.true_source() == source && edge.true_target() == target
    }));
}

#[test]
fn test_data_define_constant_creates_class() {
    let project = Project::new();
    project.write("res.rb", "Result = Data.define(:ok, :value)\n");
    assert!(labels(&project.extract()).contains("Result"));
}

#[test]
fn test_constant_receiver_singleton_call_resolves() {
    let project = Project::new();
    project.write(
        "processor.rb",
        "class Processor\n  def self.call; end\nend\n",
    );
    project.write(
        "runner.rb",
        "class Runner\n  def run\n    Processor.call\n  end\nend\n",
    );
    let result = project.extract();
    assert!(call_edge(&result, ".run()", ".call()").is_some());
}

#[test]
fn test_constant_receiver_module_function_call_resolves() {
    let project = Project::new();
    project.write(
        "tax.rb",
        "module TaxCalculator\n  module_function\n  def rate_for(o)\n    0.2\n  end\nend\n",
    );
    project.write(
        "pp.rb",
        "class PaymentProcessor\n  def process(order)\n    TaxCalculator.rate_for(order)\n  end\nend\n",
    );
    let result = project.extract();
    assert!(call_edge(&result, ".process()", ".rate_for()").is_some());
}

#[test]
fn test_constant_receiver_unknown_class_method_falls_back_to_class() {
    let project = Project::new();
    project.write("model.rb", "class Model\n  def self.create; end\nend\n");
    project.write(
        "svc.rb",
        "class Svc\n  def run\n    Model.where(id: 1)\n  end\nend\n",
    );
    let result = project.extract();
    assert!(call_edge(&result, ".run()", "Model").is_some());
}

#[test]
fn test_ambiguous_constant_receiver_emits_no_edge() {
    let project = Project::new();
    project.write(
        "a.rb",
        "module A\n  class Processor\n    def self.call; end\n  end\nend\n",
    );
    project.write(
        "b.rb",
        "module B\n  class Processor\n    def self.call; end\n  end\nend\n",
    );
    project.write(
        "c.rb",
        "class Runner\n  def run\n    Processor.call\n  end\nend\n",
    );
    let result = project.extract();
    assert!(call_edge(&result, ".run()", ".call()").is_none());
}

#[test]
fn test_include_emits_mixes_in_edge() {
    let project = Project::new();
    project.write(
        "concern.rb",
        "module SealedProtection\n  def sealed?; true; end\nend\n",
    );
    project.write(
        "model.rb",
        "class Roster < ApplicationRecord\n  include SealedProtection\nend\n",
    );
    assert!(mixes_in(&project.extract()).contains(&("Roster".into(), "SealedProtection".into())));
}

#[test]
fn test_included_module_instance_method_resolves_bare_call() {
    let project = Project::new();
    project.write(
        "worker.rb",
        concat!(
            "module MatrixRuntime\n",
            "  module Audited\n",
            "    def audit(value); value; end\n",
            "  end\n",
            "  class Worker\n",
            "    include Audited\n",
            "    def process(value); audit(value); end\n",
            "  end\n",
            "end\n",
        ),
    );
    let result = project.extract();
    assert!(mixes_in(&result).contains(&(
        "MatrixRuntime::Worker".into(),
        "MatrixRuntime::Audited".into()
    )));
    let edge = call_edge(&result, ".process()", ".audit()")
        .expect("included module should supply the bare instance method");
    assert_eq!(edge.confidence, Confidence::Extracted);
}

#[test]
fn test_extended_module_does_not_supply_bare_instance_method() {
    let project = Project::new();
    project.write(
        "worker.rb",
        concat!(
            "module Audited\n",
            "  def audit(value); value; end\n",
            "end\n",
            "class Worker\n",
            "  extend Audited\n",
            "  def process(value); audit(value); end\n",
            "end\n",
        ),
    );
    let result = project.extract();
    assert!(mixes_in(&result).contains(&("Worker".into(), "Audited".into())));
    assert!(call_edge(&result, ".process()", ".audit()").is_none());
}

#[test]
fn test_included_module_does_not_capture_unknown_receiver_call() {
    let project = Project::new();
    project.write(
        "worker.rb",
        concat!(
            "module Audited\n",
            "  def audit(value); value; end\n",
            "end\n",
            "class Worker\n",
            "  include Audited\n",
            "  def process(other, value); other.audit(value); end\n",
            "end\n",
        ),
    );
    let result = project.extract();
    assert!(mixes_in(&result).contains(&("Worker".into(), "Audited".into())));
    assert!(call_edge(&result, ".process()", ".audit()").is_none());
}

#[test]
fn test_extend_and_prepend_emit_mixes_in() {
    let project = Project::new();
    project.write("helpers.rb", "module Helpers\n  def h; end\nend\n");
    project.write("audit.rb", "module Audit\n  def a; end\nend\n");
    project.write(
        "svc.rb",
        "class Svc\n  extend Helpers\n  prepend Audit\nend\n",
    );
    let mixins = mixes_in(&project.extract());
    assert!(mixins.contains(&("Svc".into(), "Helpers".into())));
    assert!(mixins.contains(&("Svc".into(), "Audit".into())));
}

#[test]
fn test_extend_self_and_nonconstant_args_emit_no_mixin() {
    let project = Project::new();
    project.write("m.rb", "module M\n  extend self\n  def go; end\nend\n");
    assert!(mixes_in(&project.extract()).is_empty());
}

#[test]
fn test_include_of_undefined_or_ambiguous_module_emits_no_edge() {
    let project = Project::new();
    project.write("x.rb", "class X\n  include NotDefinedAnywhere\nend\n");
    assert!(mixes_in(&project.extract()).is_empty());
}

#[test]
fn test_mixin_is_not_emitted_as_calls_edge() {
    let project = Project::new();
    project.write("concern.rb", "module C\n  def m; end\nend\n");
    project.write("k.rb", "class K\n  include C\nend\n");
    let result = project.extract();
    assert!(call_edge(&result, "K", "C").is_none());
    assert!(mixes_in(&result).contains(&("K".into(), "C".into())));
}

#[test]
fn test_compact_and_nested_module_includes_resolve() {
    let project = Project::new();
    project.write(
        "totals_concern.rb",
        "module Billing::TotalsConcern\n  def total; end\nend\n",
    );
    project.write(
        "archivable_concern.rb",
        "module ArchivableConcern\n  extend ActiveSupport::Concern\n  def archive; end\nend\n",
    );
    project.write(
        "models.rb",
        "module Billing\n  class Invoice\n    include TotalsConcern\n  end\nend\n\nclass Account\n  extend ArchivableConcern\nend\n",
    );
    let mixins = mixes_in(&project.extract());
    assert!(mixins.contains(&("Billing::Invoice".into(), "Billing::TotalsConcern".into())));
    assert!(mixins.contains(&("Account".into(), "ArchivableConcern".into())));
    assert!(!mixins
        .iter()
        .any(|(_, target)| target.rsplit("::").next() == Some("Concern")));
}

#[test]
fn test_qualified_external_mixin_does_not_bind_to_local() {
    let project = Project::new();
    project.write(
        "concern.rb",
        "module Concern\n  def local_thing; end\nend\n",
    );
    project.write(
        "post.rb",
        "class Post\n  extend ActiveSupport::Concern\nend\n",
    );
    assert!(mixes_in(&project.extract()).is_empty());
}

#[test]
fn test_in_corpus_qualified_mixin_resolves() {
    let project = Project::new();
    project.write(
        "foo.rb",
        "module Foo\n  module Concern\n    def helper; end\n  end\nend\n",
    );
    project.write("k.rb", "class K\n  include Foo::Concern\nend\n");
    assert!(mixes_in(&project.extract()).contains(&("K".into(), "Foo::Concern".into())));
}

#[test]
fn test_nested_declared_class_still_resolves_as_receiver() {
    let project = Project::new();
    project.write(
        "billing.rb",
        "module Billing\n  class Processor\n    def run\n      42\n    end\n  end\nend\n",
    );
    project.write(
        "caller.rb",
        "def process_all\n  p = Processor.new\n  p.run\nend\n",
    );
    let result = project.extract();
    assert!(call_edge(&result, "process_all", "Billing::Processor").is_some());
    assert!(call_edge(&result, "process_all", ".run()").is_some());
}

#[test]
fn test_rake_files_extract_and_resolve_like_rb() {
    let project = Project::new();
    project.write(
        "ops.rake",
        "class RakeHelper\n  def self.run\n    Widget.tally\n  end\nend\n",
    );
    project.write(
        "widget.rb",
        "class Widget\n  def self.tally\n    42\n  end\nend\n",
    );
    let result = project.extract();
    let all_labels = labels(&result);
    assert!(all_labels.contains("RakeHelper"));
    assert!(all_labels.contains(".run()"));
    assert!(call_edge(&result, ".run()", ".tally()").is_some());
}
