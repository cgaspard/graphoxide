//! Reusable CLI services.

pub mod build_guard;
pub mod build_progress;
pub mod build_telemetry;
pub mod coverage;
pub mod enrich;
pub mod extract_cli;
pub(crate) mod fs_lock;
pub mod google_workspace;
pub mod hook_guard;
pub mod hooks;
pub mod index;
pub mod install;
pub mod ollama_transport;
pub mod transcribe;
pub mod watch;
pub mod wiki;
pub mod wiki_draft;
pub mod wiki_materialize;
pub mod wiki_openapi;
pub mod wiki_provider;
