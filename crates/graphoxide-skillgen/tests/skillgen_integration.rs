// One explicit integration-test binary avoids a linker invocation per source
// file while retaining each upstream test file and its fixture-relative paths.
#[path = "upstream_skillgen.rs"]
mod upstream_skillgen;
