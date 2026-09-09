#![recursion_limit = "256"]

//! Reusable CLI services.

pub mod build_guard;
pub mod build_progress;
pub mod build_telemetry;
pub mod coverage;
pub mod enrich;
pub mod extract_cli;
pub mod google_workspace;
pub mod hook_guard;
pub mod hooks;
pub mod index;
pub mod install;
pub mod ollama_transport;
pub mod transcribe;
pub mod watch;
pub mod wiki_direct;
pub mod wiki_hugo;
mod wiki_lock;
pub mod wiki_openapi;
pub mod wiki_provider;
pub mod wiki_source;

pub mod wiki_materialize;
