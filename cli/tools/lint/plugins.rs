// Copyright 2018-2024 the Deno authors. All rights reserved. MIT license.

use deno_ast::swc::common as swc_common;
use deno_ast::swc::common::BytePos;
use deno_ast::ModuleSpecifier;
use deno_ast::ParsedSource;
use deno_core::anyhow::Context;
use deno_core::error::custom_error;
use deno_core::error::AnyError;
use deno_core::futures::FutureExt;
use deno_core::op2;
use deno_core::serde_json;
use deno_core::serde_v8;
use deno_core::url::Url;
use deno_core::v8;
use deno_core::JsRuntime;
use deno_core::OpState;
use deno_core::PollEventLoopOptions;
use deno_core::RuntimeOptions;
use deno_lint::diagnostic::LintDiagnostic;
use deno_lint::diagnostic::LintDiagnosticDetails;
use deno_runtime::tokio_util;
use indexmap::IndexMap;
use serde::Deserialize;
use std::rc::Rc;
use std::sync::Arc;
use swc_estree_compat::babelify;
use swc_estree_compat::babelify::Babelify;
use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;

#[derive(Debug)]
pub enum PluginRunnerRequest {
  LoadPlugins(Vec<ModuleSpecifier>),
  Run(String),
}

pub enum PluginRunnerResponse {
  LoadPlugin(Result<(), AnyError>),
  Run(Result<Vec<LintDiagnostic>, AnyError>),
}

impl std::fmt::Debug for PluginRunnerResponse {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::LoadPlugin(_arg0) => f.debug_tuple("LoadPlugin").finish(),
      Self::Run(_arg0) => f.debug_tuple("Run").finish(),
    }
  }
}

#[derive(Debug)]
pub struct PluginRunnerProxy {
  tx: Sender<PluginRunnerRequest>,
  rx: Arc<tokio::sync::Mutex<Receiver<PluginRunnerResponse>>>,
  join_handle: std::thread::JoinHandle<Result<(), AnyError>>,
}

pub struct PluginRunner {
  runtime: JsRuntime,
  run_plugin_rule_fn: v8::Global<v8::Function>,
  tx: Sender<PluginRunnerResponse>,
  rx: Receiver<PluginRunnerRequest>,
}

impl PluginRunner {
  async fn create() -> Result<PluginRunnerProxy, AnyError> {
    let (tx_req, rx_req) = channel(10);
    let (tx_res, rx_res) = channel(10);

    eprintln!("spawning thread");
    let join_handle = std::thread::spawn(move || {
      eprintln!("thread spawned");
      let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![deno_lint_ext::init_ops()],
        module_loader: Some(Rc::new(deno_core::FsModuleLoader)),
        ..Default::default()
      });

      eprintln!("before loaded");

      let obj_result = runtime.lazy_load_es_module_with_code(
        "ext:cli/lint.js",
        deno_core::ascii_str_include!(concat!("lint.js")),
      );

      eprintln!("after loaded {}", obj_result.is_err());
      let obj = obj_result?;

      let run_plugin_rule_fn = {
        let scope = &mut runtime.handle_scope();
        let fn_name = v8::String::new(scope, "runPluginRule").unwrap();
        let obj_local: v8::Local<v8::Object> =
          v8::Local::new(scope, obj).try_into().unwrap();
        let run_fn_val = obj_local.get(scope, fn_name.into()).unwrap();
        let run_fn: v8::Local<v8::Function> = run_fn_val.try_into().unwrap();
        v8::Global::new(scope, run_fn)
      };

      let mut runner = Self {
        runtime,
        run_plugin_rule_fn,
        tx: tx_res,
        rx: rx_req,
      };
      // TODO(bartlomieju): send "host ready" message to the proxy
      eprintln!("running host loop");
      runner.run_loop()?;
      Ok(())
    });

    eprintln!("is thread finished {}", join_handle.is_finished());
    let proxy = PluginRunnerProxy {
      tx: tx_req,
      rx: Arc::new(tokio::sync::Mutex::new(rx_res)),
      join_handle,
    };

    Ok(proxy)
  }

  fn run_loop(mut self) -> Result<(), AnyError> {
    let fut = async move {
      eprintln!("waiting for message");
      while let Some(req) = self.rx.recv().await {
        eprintln!("received message");
        match req {
          PluginRunnerRequest::LoadPlugins(specifiers) => {
            let r = self.load_plugins(specifiers).await;
            let _ = self.tx.send(PluginRunnerResponse::LoadPlugin(r)).await;
          }
          PluginRunnerRequest::Run(serialized_ast) => {
            let rules_to_run = self.get_rules_to_run();

            eprintln!("Loaded plugins:");
            for (plugin_name, rules) in rules_to_run.iter() {
              eprintln!(" - {}", plugin_name);
              for rule in rules {
                eprintln!("   - {}", rule);
              }
            }

            let r = match self.run_rules(rules_to_run, serialized_ast).await {
              Ok(()) => Ok(self.take_diagnostics()),
              Err(err) => Err(err),
            };
            let _ = self.tx.send(PluginRunnerResponse::Run(r)).await;
          }
        }
      }
      eprintln!("breaking loop");
      Ok(())
    }
    .boxed_local();
    tokio_util::create_and_run_current_thread(fut)
  }

  fn take_diagnostics(&mut self) -> Vec<LintDiagnostic> {
    let op_state = self.runtime.op_state();
    let mut state = op_state.borrow_mut();
    let mut container = state.borrow_mut::<LintPluginContainer>();
    std::mem::take(&mut container.diagnostics)
  }

  fn get_rules_to_run(&mut self) -> IndexMap<String, Vec<String>> {
    let op_state = self.runtime.op_state();
    let state = op_state.borrow();
    let container = state.borrow::<LintPluginContainer>();

    let mut to_run = IndexMap::with_capacity(container.plugins.len());
    for (plugin_name, plugin) in container.plugins.iter() {
      let rules = plugin
        .rules
        .keys()
        .map(String::from)
        .collect::<Vec<String>>();
      to_run.insert(plugin_name.to_string(), rules);
    }

    to_run
  }

  async fn run_rules(
    &mut self,
    rules_to_run: IndexMap<String, Vec<String>>,
    ast_string: String,
  ) -> Result<(), AnyError> {
    for (plugin_name, rules) in rules_to_run {
      for rule_name in rules {
        // TODO(bartlomieju): filename and ast_string can be made into global only once, not on every iteration
        let (file_name, plugin_name_v8, rule_name_v8, ast_string_v8) = {
          let scope = &mut self.runtime.handle_scope();
          let file_name: v8::Local<v8::Value> =
            v8::String::new(scope, "foo.js").unwrap().into();
          let plugin_name_v8: v8::Local<v8::Value> =
            v8::String::new(scope, &plugin_name).unwrap().into();
          let rule_name_v8: v8::Local<v8::Value> =
            v8::String::new(scope, &rule_name).unwrap().into();
          let ast_string_v8: v8::Local<v8::Value> =
            v8::String::new(scope, &ast_string).unwrap().into();
          (
            v8::Global::new(scope, file_name),
            v8::Global::new(scope, plugin_name_v8),
            v8::Global::new(scope, rule_name_v8),
            v8::Global::new(scope, ast_string_v8),
          )
        };
        let call = self.runtime.call_with_args(
          &self.run_plugin_rule_fn,
          &[file_name, plugin_name_v8, rule_name_v8, ast_string_v8],
        );
        let result = self
          .runtime
          .with_event_loop_promise(call, PollEventLoopOptions::default())
          .await;
        match result {
          Ok(r) => {
            eprintln!("plugin finished")
          }
          Err(error) => {
            eprintln!("error running plugin {}", error);
          }
        }
      }
    }

    Ok(())
  }

  async fn load_plugins(
    &mut self,
    plugin_specifiers: Vec<ModuleSpecifier>,
  ) -> Result<(), AnyError> {
    let mut load_futures = Vec::with_capacity(plugin_specifiers.len());
    for specifier in plugin_specifiers {
      let mod_id = self.runtime.load_side_es_module(&specifier).await?;
      let mod_future = self.runtime.mod_evaluate(mod_id).boxed_local();
      load_futures.push((mod_future, mod_id));
    }

    self
      .runtime
      .run_event_loop(PollEventLoopOptions::default())
      .await?;

    let state = self.runtime.op_state();

    for (fut, mod_id) in load_futures {
      let _ = fut.await?;
      let module = self.runtime.get_module_namespace(mod_id).unwrap();
      let scope = &mut self.runtime.handle_scope();
      let module_local = v8::Local::new(scope, module);
      let default_export_str = v8::String::new(scope, "default").unwrap();
      eprintln!(
        "has default export {:?}",
        module_local.has_own_property(scope, default_export_str.into())
      );
      let default_export =
        module_local.get(scope, default_export_str.into()).unwrap();
      let name_str = v8::String::new(scope, "name").unwrap();
      let name_val: v8::Local<v8::Object> = default_export.try_into().unwrap();

      let name_val = name_val.get(scope, name_str.into()).unwrap();

      eprintln!(
        "default export name {:?}",
        name_val.to_rust_string_lossy(scope)
      );
      eprintln!("deserializing plugin");

      let def: PluginDefinition = serde_v8::from_v8(scope, default_export)
        .context("Failed to deserialize plugin")?;

      eprintln!("deserialized plugin {} {:?}", def.name, def.rules.keys());

      let mut state = state.borrow_mut();
      let mut container = state.borrow_mut::<LintPluginContainer>();
      let mut plugin_desc = LintPluginDesc {
        rules: IndexMap::with_capacity(def.rules.len()),
      };
      for (name, rule_def) in def.rules.into_iter() {
        let local_val = v8::Local::new(scope, rule_def.create.v8_value);
        let local_fn: v8::Local<v8::Function> = local_val.try_into().unwrap();
        let global_fn = v8::Global::new(scope, local_fn);
        plugin_desc.rules.insert(name, global_fn);
      }
      let _ = container.register(def.name, plugin_desc);

      // let name_str = v8::String::new(scope, "name").unwrap();
      // let plugin_name = module_local.get(scope, name.into()).unwrap();
      // if plugin_name.is_undefined() {
      //   bail!("Missing 'name' for a plugin");
      // }
      // let plugin_name: v8::Local<v8::String> = plugin_name.try_into().unwrap();
      // let plugin_name = plugin_name.to_rust_string_lossy(scope);

      // let rules_str =
    }

    Ok(())
  }
}

#[derive(Deserialize)]
struct RuleDefinition {
  create: serde_v8::GlobalValue,
}

#[derive(Deserialize)]
struct PluginDefinition {
  name: String,
  rules: IndexMap<String, RuleDefinition>,
}

impl PluginRunnerProxy {
  pub async fn load_plugins(
    &self,
    plugin_specifiers: Vec<ModuleSpecifier>,
  ) -> Result<(), AnyError> {
    self
      .tx
      .send(PluginRunnerRequest::LoadPlugins(plugin_specifiers))
      .await?;
    let mut rx = self.rx.lock().await;
    eprintln!("receiving load plugins");
    if let Some(val) = rx.recv().await {
      let PluginRunnerResponse::LoadPlugin(result) = val else {
        unreachable!()
      };
      eprintln!("load plugins response {:#?}", result);
      return Ok(());
    }
    Err(custom_error("AlreadyClosed", "Plugin host has closed"))
  }

  pub async fn run_rules(
    &self,
    serialized_ast: String,
  ) -> Result<Vec<LintDiagnostic>, AnyError> {
    self
      .tx
      .send(PluginRunnerRequest::Run(serialized_ast))
      .await?;
    let mut rx = self.rx.lock().await;
    eprintln!("receiving diagnostics");
    if let Some(PluginRunnerResponse::Run(diagnostics_result)) = rx.recv().await
    {
      return diagnostics_result;
    }
    Err(custom_error("AlreadyClosed", "Plugin host has closed"))
  }
}

pub async fn create_runner_and_load_plugins(
  plugin_specifiers: Vec<ModuleSpecifier>,
) -> Result<PluginRunnerProxy, AnyError> {
  let mut runner_proxy = PluginRunner::create().await?;
  runner_proxy.load_plugins(plugin_specifiers).await?;
  Ok(runner_proxy)
}

pub async fn run_rules_for_ast(
  runner_proxy: &mut PluginRunnerProxy,
  serialized_ast: String,
) -> Result<Vec<LintDiagnostic>, AnyError> {
  let d = runner_proxy.run_rules(serialized_ast).await?;
  Ok(d)
}

pub fn get_estree_from_parsed_source(
  parsed_source: ParsedSource,
) -> Result<String, AnyError> {
  let cm = Rc::new(swc_common::SourceMap::new(
    swc_common::FilePathMapping::empty(),
  ));
  let fm = Rc::new(swc_common::SourceFile::new(
    Rc::new(swc_common::FileName::Anon),
    false,
    Rc::new(swc_common::FileName::Anon),
    parsed_source.text().to_string(),
    BytePos(1),
  ));
  let babelify_ctx = babelify::Context {
    fm,
    cm,
    comments: swc_node_comments::SwcComments::default(),
  };
  let program = parsed_source.program();
  let start = std::time::Instant::now();
  let program = &*program;
  let r = serde_json::to_string(&program.clone().babelify(&babelify_ctx))?;
  let end = std::time::Instant::now();
  eprintln!("serialize using serde_json took {:?}", end - start);
  Ok(r)
}

struct LintPluginDesc {
  rules: IndexMap<String, v8::Global<v8::Function>>,
}

#[derive(Default)]
struct LintPluginContainer {
  plugins: IndexMap<String, LintPluginDesc>,
  diagnostics: Vec<LintDiagnostic>,
}

impl LintPluginContainer {
  fn register(
    &mut self,
    name: String,
    desc: LintPluginDesc,
  ) -> Result<(), AnyError> {
    if self.plugins.contains_key(&name) {
      return Err(custom_error(
        "AlreadyExists",
        format!("{} plugin already exists", name),
      ));
    }

    self.plugins.insert(name, desc);
    Ok(())
  }

  fn report(&mut self, id: String, specifier: String, message: String) {
    let lint_diagnostic = LintDiagnostic {
      // TODO: fix
      specifier: ModuleSpecifier::parse(&format!("file:///{}", specifier))
        .unwrap(),
      range: None,
      details: LintDiagnosticDetails {
        message,
        code: id,
        hint: None,
        fixes: vec![],
        custom_docs_url: None,
        info: vec![],
      },
    };
    self.diagnostics.push(lint_diagnostic);
  }
}

deno_core::extension!(
  deno_lint_ext,
  ops = [op_lint_get_rule, op_lint_report,],
  state = |state| {
    state.put(LintPluginContainer::default());
  },
);

#[op2]
#[global]
fn op_lint_get_rule(
  state: &mut OpState,
  #[string] plugin_name: &str,
  #[string] rule_name: &str,
) -> Result<v8::Global<v8::Function>, AnyError> {
  let container = state.borrow::<LintPluginContainer>();
  let Some(plugin) = container.plugins.get(plugin_name) else {
    return Err(custom_error(
      "NotFound",
      format!("{} plugin is not registered", plugin_name),
    ));
  };
  let Some(rule) = plugin.rules.get(rule_name) else {
    return Err(custom_error(
      "NotFound",
      format!("Plugin {}, does not have {} rule", plugin_name, rule_name),
    ));
  };
  Ok(rule.clone())
}

#[op2(fast)]
fn op_lint_report(
  state: &mut OpState,
  #[string] id: String,
  #[string] specifier: String,
  #[string] message: String,
) {
  let container = state.borrow_mut::<LintPluginContainer>();
  container.report(id, specifier, message);
}