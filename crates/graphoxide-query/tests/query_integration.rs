// One explicit integration-test binary avoids a linker invocation per source
// file while retaining each upstream test file and its fixture-relative paths.
#[path = "upstream_file_label_disambiguation.rs"]
mod upstream_file_label_disambiguation;
#[path = "upstream_prs.rs"]
mod upstream_prs;
#[path = "upstream_serve.rs"]
mod upstream_serve;
