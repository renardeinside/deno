#![allow(unused_imports)]

pub mod args;
pub mod cache;
pub mod emit;
pub mod factory;
pub mod file_fetcher;
pub mod graph_container;
pub mod graph_util;
pub mod http_util;
pub mod module_loader;
pub mod node;
pub mod npm;
pub mod resolver;
pub mod standalone;
pub mod task_runner;
pub mod tools;
pub mod tsc;
pub mod util;
pub mod worker;

pub mod sys {
  #[allow(clippy::disallowed_types)] // ok, definition
  pub type CliSys = sys_traits::impls::RealSys;
}

pub mod cdp;
pub mod js;
pub mod jsr;
pub mod lsp;
pub mod ops;

use std::env;
use std::future::Future;
use std::io::IsTerminal;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;

use args::TaskFlags;
use deno_core::anyhow::Context;
use deno_core::error::AnyError;
use deno_core::error::CoreError;
use deno_core::futures::FutureExt;
use deno_core::unsync::JoinHandle;
use deno_lib::util::result::any_and_jserrorbox_downcast_ref;
use deno_resolver::npm::ByonmResolvePkgFolderFromDenoReqError;
use deno_resolver::npm::ResolvePkgFolderFromDenoReqError;
use deno_runtime::fmt_errors::format_js_error;
use deno_runtime::tokio_util::create_and_run_current_thread_with_maybe_metrics;
use deno_runtime::WorkerExecutionMode;
pub use deno_runtime::UNSTABLE_GRANULAR_FLAGS;
use deno_telemetry::OtelConfig;
use deno_terminal::colors;
use factory::CliFactory;

use self::npm::ResolveSnapshotError;
use self::util::draw_thread::DrawThread;
use crate::args::DenoSubcommand;
use crate::args::Flags;
use crate::args::{flags_from_vec, AddFlags};
use crate::util::display;
use crate::util::v8::get_v8_flags_from_env;
use crate::util::v8::init_v8_flags;

pub fn unstable_exit_cb(feature: &str, api_name: &str) {
  log::error!(
    "Unstable API '{api_name}'. The `--unstable-{}` flag must be provided.",
    feature
  );
  deno_runtime::exit(70);
}

pub async fn add(package: Vec<String>, dev: bool) -> Result<(), AnyError> {
  let default_flags = Flags::default();

  let add_flags = AddFlags {
    packages: package,
    dev,
  };

  tools::registry::add(
    Arc::new(default_flags),
    add_flags,
    tools::registry::AddCommandName::Add,
  )
  .await
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::env;
  use tempfile::tempdir;

  #[tokio::test]
  async fn test_add_dependency() {
    let temp_dir = tempdir().expect("Failed to create temporary directory");
    env::set_current_dir(&temp_dir).expect("Failed to change directory");
    println!("Current directory: {:?}", env::current_dir().unwrap());

    let result = add(
      vec!["npm:svelte".to_string(), "npm:typescript".to_string()],
      false,
    )
    .await;

    assert!(
      result.is_ok(),
      "Failed to add dependency: {:?}",
      result.err()
    );
    // check that deno.lock exists
    assert!(temp_dir.path().join("deno.lock").exists());
  }
}
