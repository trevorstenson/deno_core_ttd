// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.
use super::bindings;
use crate::error::exception_to_err_result;
use crate::error::generic_error;
use crate::error::to_v8_type_error;
use crate::module_specifier::ModuleSpecifier;
use crate::modules::ModuleCode;
use crate::modules::ModuleError;
use crate::modules::ModuleId;
use crate::modules::ModuleLoadId;
use crate::modules::ModuleMap;
use crate::ops::OpCtx;
use crate::ops::PendingOp;
use crate::runtime::JsRuntimeState;
use crate::JsRuntime;
use anyhow::Error;
use deno_unsync::JoinSet;
use futures::channel::oneshot;
use futures::stream::StreamExt;
use std::cell::RefCell;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::hash::BuildHasherDefault;
use std::hash::Hasher;
use std::option::Option;
use std::rc::Rc;
use std::task::Context;
use std::task::Poll;
use v8::HandleScope;
use v8::Local;

// Hasher used for `unrefed_ops`. Since these are rolling i32, there's no
// need to actually hash them.
#[derive(Default)]
pub(crate) struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
  fn write_i32(&mut self, i: i32) {
    self.0 = i as u64;
  }

  fn finish(&self) -> u64 {
    self.0
  }

  fn write(&mut self, _bytes: &[u8]) {
    unreachable!()
  }
}

struct DynImportModEvaluate {
  load_id: ModuleLoadId,
  module_id: ModuleId,
  promise: v8::Global<v8::Promise>,
  module: v8::Global<v8::Module>,
}

pub(crate) struct ModEvaluate {
  promise: Option<v8::Global<v8::Promise>>,
  pub(crate) has_evaluated: bool,
  pub(crate) handled_promise_rejections: Vec<v8::Global<v8::Promise>>,
  sender: oneshot::Sender<Result<(), Error>>,
}

#[derive(Default)]
pub(crate) struct ContextState {
  pub(crate) js_event_loop_tick_cb: Option<Rc<v8::Global<v8::Function>>>,
  pub(crate) js_build_custom_error_cb: Option<Rc<v8::Global<v8::Function>>>,
  pub(crate) js_promise_reject_cb: Option<Rc<v8::Global<v8::Function>>>,
  pub(crate) js_format_exception_cb: Option<Rc<v8::Global<v8::Function>>>,
  pub(crate) js_wasm_streaming_cb: Option<Rc<v8::Global<v8::Function>>>,
  pub(crate) pending_promise_rejections:
    VecDeque<(v8::Global<v8::Promise>, v8::Global<v8::Value>)>,
  pending_dyn_mod_evaluate: Vec<DynImportModEvaluate>,
  pub(crate) pending_mod_evaluate: Option<ModEvaluate>,
  pub(crate) unrefed_ops: HashSet<i32, BuildHasherDefault<IdentityHasher>>,
  pub(crate) pending_ops: JoinSet<PendingOp>,
  // We don't explicitly re-read this prop but need the slice to live alongside
  // the context
  pub(crate) op_ctxs: Box<[OpCtx]>,
  pub(crate) isolate: Option<*mut v8::OwnedIsolate>,
}

/// A representation of a JavaScript realm tied to a [`JsRuntime`], that allows
/// execution in the realm's context.
///
/// A [`JsRealm`] instance is a reference to an already existing realm, which
/// does not hold ownership of it, so instances can be created and dropped as
/// needed. As such, calling [`JsRealm::new`] doesn't create a new realm, and
/// cloning a [`JsRealm`] only creates a new reference. See
/// [`JsRuntime::create_realm`] to create new realms instead.
///
/// Despite [`JsRealm`] instances being references, multiple instances that
/// point to the same realm won't overlap because every operation requires
/// passing a mutable reference to the [`v8::Isolate`]. Therefore, no operation
/// on two [`JsRealm`] instances tied to the same isolate can be run at the same
/// time, regardless of whether they point to the same realm.
///
/// # Panics
///
/// Every method of [`JsRealm`] will panic if you call it with a reference to a
/// [`v8::Isolate`] other than the one that corresponds to the current context.
///
/// In other words, the [`v8::Isolate`] parameter for all the related [`JsRealm`] methods
/// must be extracted from the pre-existing [`JsRuntime`].
///
/// Example usage with the [`JsRealm::execute_script`] method:
/// ```
/// use deno_core::JsRuntime;
/// use deno_core::RuntimeOptions;
/// use deno_core::CreateRealmOptions;
///
/// let mut runtime = JsRuntime::new(RuntimeOptions::default());
/// let new_realm = runtime
///         .create_realm(CreateRealmOptions::default())
///         .expect("Handle the error properly");
/// let source_code = "var a = 0; a + 1";
/// let result = new_realm
///         .execute_script_static(runtime.v8_isolate(), "<anon>", source_code)
///         .expect("Handle the error properly");
/// # drop(result);
/// ```
///
/// # Lifetime of the realm
///
/// As long as the corresponding isolate is alive, a [`JsRealm`] instance will
/// keep the underlying V8 context alive even if it would have otherwise been
/// garbage collected.
#[derive(Clone)]
#[repr(transparent)]
pub struct JsRealm(pub(crate) JsRealmInner);

#[derive(Clone)]
pub(crate) struct JsRealmInner {
  context_state: Rc<RefCell<ContextState>>,
  context: Rc<v8::Global<v8::Context>>,
  module_map: Rc<RefCell<ModuleMap>>,
  runtime_state: Rc<RefCell<JsRuntimeState>>,
  is_main_realm: bool,
}

impl JsRealmInner {
  pub(crate) fn new(
    context_state: Rc<RefCell<ContextState>>,
    context: v8::Global<v8::Context>,
    module_map: Rc<RefCell<ModuleMap>>,
    runtime_state: Rc<RefCell<JsRuntimeState>>,
    is_main_realm: bool,
  ) -> Self {
    Self {
      context_state,
      context: context.into(),
      module_map,
      runtime_state,
      is_main_realm,
    }
  }

  pub fn num_pending_ops(&self) -> usize {
    self.context_state.borrow().pending_ops.len()
  }

  pub fn num_unrefed_ops(&self) -> usize {
    self.context_state.borrow().unrefed_ops.len()
  }

  pub fn has_pending_dyn_imports(&self) -> bool {
    self.module_map.borrow().has_pending_dynamic_imports()
  }

  pub fn has_pending_dyn_module_evaluation(&self) -> bool {
    !self
      .context_state
      .borrow()
      .pending_dyn_mod_evaluate
      .is_empty()
  }

  pub fn has_pending_module_evaluation(&self) -> bool {
    self.context_state.borrow().pending_mod_evaluate.is_some()
  }

  #[inline(always)]
  pub fn context(&self) -> &v8::Global<v8::Context> {
    &self.context
  }

  #[inline(always)]
  pub(crate) fn state(&self) -> Rc<RefCell<ContextState>> {
    self.context_state.clone()
  }

  #[inline(always)]
  pub(crate) fn module_map(&self) -> Rc<RefCell<ModuleMap>> {
    self.module_map.clone()
  }

  /// For info on the [`v8::Isolate`] parameter, check [`JsRealm#panics`].
  #[inline(always)]
  pub fn handle_scope<'s>(
    &self,
    isolate: &'s mut v8::Isolate,
  ) -> v8::HandleScope<'s> {
    println!("handle_scope {:?}", self.context);
    v8::HandleScope::with_context(isolate, &*self.context)
  }

  pub(crate) fn check_promise_rejections(
    &self,
    isolate: &mut v8::Isolate,
  ) -> Result<(), Error> {
    let Some((_, handle)) = self.context_state.borrow_mut().pending_promise_rejections.pop_front() else {
      return Ok(());
    };

    let scope = &mut self.handle_scope(isolate);
    let exception = v8::Local::new(scope, handle);
    let state_rc = JsRuntime::state_from(scope);
    let state = state_rc.borrow();
    if let Some(inspector) = &state.inspector {
      let inspector = inspector.borrow();
      inspector.exception_thrown(scope, exception, true);
      if inspector.has_blocking_sessions() {
        return Ok(());
      }
    }
    exception_to_err_result(scope, exception, true)
  }

  pub(crate) fn is_same(&self, other: &Rc<v8::Global<v8::Context>>) -> bool {
    Rc::ptr_eq(&self.context, other)
  }

  pub fn destroy(self) {
    let state = self.state();
    let raw_ptr = self.state().borrow().isolate.unwrap();
    // SAFETY: We know the isolate outlives the realm
    let isolate = unsafe { raw_ptr.as_mut().unwrap() };
    let mut realm_state = state.borrow_mut();
    // These globals will prevent snapshots from completing, take them
    std::mem::take(&mut realm_state.js_event_loop_tick_cb);
    std::mem::take(&mut realm_state.js_build_custom_error_cb);
    std::mem::take(&mut realm_state.js_promise_reject_cb);
    std::mem::take(&mut realm_state.js_format_exception_cb);
    std::mem::take(&mut realm_state.js_wasm_streaming_cb);
    // The OpCtx slice may contain a circular reference
    std::mem::take(&mut realm_state.op_ctxs);

    self.context().open(isolate).clear_all_slots(isolate);

    // Expect that this context is dead (we only check this in debug mode)
    // TODO(mmastrac): This check fails for some tests, will need to fix this
    // debug_assert_eq!(Rc::strong_count(&self.context), 1, "Realm was still alive when we wanted to destroy it. Not dropped?");
  }
}

impl JsRealm {
  pub(crate) fn new(inner: JsRealmInner) -> Self {
    Self(inner)
  }

  #[inline(always)]
  pub(crate) fn state_from_scope(
    scope: &mut v8::HandleScope,
  ) -> Rc<RefCell<ContextState>> {
    let context = scope.get_current_context();
    context
      .get_slot::<Rc<RefCell<ContextState>>>(scope)
      .unwrap()
      .clone()
  }

  #[inline(always)]
  pub(crate) fn module_map_from(
    scope: &mut v8::HandleScope,
  ) -> Rc<RefCell<ModuleMap>> {
    let context = scope.get_current_context();
    context
      .get_slot::<Rc<RefCell<ModuleMap>>>(scope)
      .unwrap()
      .clone()
  }

  #[inline(always)]
  pub fn num_pending_ops(&self) -> usize {
    self.0.num_pending_ops()
  }

  #[inline(always)]
  pub fn num_unrefed_ops(&self) -> usize {
    self.0.num_unrefed_ops()
  }

  /// For info on the [`v8::Isolate`] parameter, check [`JsRealm#panics`].
  #[inline(always)]
  pub fn handle_scope<'s>(
    &self,
    isolate: &'s mut v8::Isolate,
  ) -> v8::HandleScope<'s> {
    self.0.handle_scope(isolate)
  }

  #[inline(always)]
  pub fn context(&self) -> &v8::Global<v8::Context> {
    self.0.context()
  }

  /// For info on the [`v8::Isolate`] parameter, check [`JsRealm#panics`].
  pub fn global_object<'s>(
    &self,
    isolate: &'s mut v8::Isolate,
  ) -> v8::Local<'s, v8::Object> {
    let scope = &mut self.0.handle_scope(isolate);
    self.0.context.open(scope).global(scope)
  }

  fn string_from_code<'a>(
    scope: &mut HandleScope<'a>,
    code: &ModuleCode,
  ) -> Option<Local<'a, v8::String>> {
    if let Some(code) = code.try_static_ascii() {
      v8::String::new_external_onebyte_static(scope, code)
    } else {
      v8::String::new_from_utf8(
        scope,
        code.as_bytes(),
        v8::NewStringType::Normal,
      )
    }
  }

  /// Executes traditional JavaScript code (traditional = not ES modules) in the
  /// realm's context.
  ///
  /// For info on the [`v8::Isolate`] parameter, check [`JsRealm#panics`].
  ///
  /// The `name` parameter can be a filepath or any other string. E.g.:
  ///
  ///   - "/some/file/path.js"
  ///   - "<anon>"
  ///   - "[native code]"
  ///
  /// The same `name` value can be used for multiple executions.
  ///
  /// `Error` can usually be downcast to `JsError`.
  pub fn execute_script_static(
    &self,
    isolate: &mut v8::Isolate,
    name: &'static str,
    source_code: &'static str,
  ) -> Result<v8::Global<v8::Value>, Error> {
    self.execute_script(isolate, name, ModuleCode::from_static(source_code))
  }

  /// Executes traditional JavaScript code (traditional = not ES modules) in the
  /// realm's context.
  ///
  /// For info on the [`v8::Isolate`] parameter, check [`JsRealm#panics`].
  ///
  /// The `name` parameter can be a filepath or any other string. E.g.:
  ///
  ///   - "/some/file/path.js"
  ///   - "<anon>"
  ///   - "[native code]"
  ///
  /// The same `name` value can be used for multiple executions.
  ///
  /// `Error` can usually be downcast to `JsError`.
  pub fn execute_script(
    &self,
    isolate: &mut v8::Isolate,
    name: &'static str,
    source_code: ModuleCode,
  ) -> Result<v8::Global<v8::Value>, Error> {
    let scope = &mut self.0.handle_scope(isolate);

    let source = Self::string_from_code(scope, &source_code).unwrap();
    println!("source: {:?}", source);
    debug_assert!(name.is_ascii());
    let name =
      v8::String::new_external_onebyte_static(scope, name.as_bytes()).unwrap();
    let origin = bindings::script_origin(scope, name);

    let tc_scope = &mut v8::TryCatch::new(scope);

    let script = match v8::Script::compile(tc_scope, source, Some(&origin)) {
      Some(script) => script,
      None => {
        let exception = tc_scope.exception().unwrap();
        return exception_to_err_result(tc_scope, exception, false);
      }
    };

    match script.run(tc_scope) {
      Some(value) => {
        let value_handle = v8::Global::new(tc_scope, value);
        Ok(value_handle)
      }
      None => {
        assert!(tc_scope.has_caught());
        let exception = tc_scope.exception().unwrap();
        exception_to_err_result(tc_scope, exception, false)
      }
    }
  }

  /// Clears all loaded modules
  /// May not clear all associated memory and should not
  /// be used in production
  pub fn clear_modules(&mut self) {
    self.0.module_map().borrow_mut().clear();
  }

  /// Returns the namespace object of a module.
  ///
  /// This is only available after module evaluation has completed.
  /// This function panics if module has not been instantiated.
  pub fn get_module_namespace(
    &self,
    isolate: &mut v8::Isolate,
    module_id: ModuleId,
  ) -> Result<v8::Global<v8::Object>, Error> {
    self
      .0
      .module_map()
      .borrow()
      .get_module_namespace(&mut self.handle_scope(isolate), module_id)
  }

  pub(crate) fn instantiate_module(
    &self,
    isolate: &mut v8::Isolate,
    id: ModuleId,
  ) -> Result<(), v8::Global<v8::Value>> {
    self
      .0
      .module_map()
      .borrow_mut()
      .instantiate_module(&mut self.handle_scope(isolate), id)
  }

  fn dynamic_import_module_evaluate(
    &self,
    isolate: &mut v8::Isolate,
    load_id: ModuleLoadId,
    id: ModuleId,
  ) -> Result<(), Error> {
    let module_handle = self
      .0
      .module_map()
      .borrow()
      .get_handle(id)
      .expect("ModuleInfo not found");

    let status = {
      let scope = &mut self.handle_scope(isolate);
      let module = module_handle.open(scope);
      module.get_status()
    };

    match status {
      v8::ModuleStatus::Instantiated | v8::ModuleStatus::Evaluated => {}
      _ => return Ok(()),
    }

    // IMPORTANT: Top-level-await is enabled, which means that return value
    // of module evaluation is a promise.
    //
    // This promise is internal, and not the same one that gets returned to
    // the user. We add an empty `.catch()` handler so that it does not result
    // in an exception if it rejects. That will instead happen for the other
    // promise if not handled by the user.
    //
    // For more details see:
    // https://github.com/denoland/deno/issues/4908
    // https://v8.dev/features/top-level-await#module-execution-order
    let scope = &mut self.handle_scope(isolate);
    let tc_scope = &mut v8::TryCatch::new(scope);
    let module = v8::Local::new(tc_scope, &module_handle);
    let maybe_value = module.evaluate(tc_scope);

    // Update status after evaluating.
    let status = module.get_status();

    if let Some(value) = maybe_value {
      assert!(
        status == v8::ModuleStatus::Evaluated
          || status == v8::ModuleStatus::Errored
      );
      let promise = v8::Local::<v8::Promise>::try_from(value)
        .expect("Expected to get promise as module evaluation result");
      let empty_fn = bindings::create_empty_fn(tc_scope).unwrap();
      promise.catch(tc_scope, empty_fn);
      let promise_global = v8::Global::new(tc_scope, promise);
      let module_global = v8::Global::new(tc_scope, module);

      let dyn_import_mod_evaluate = DynImportModEvaluate {
        load_id,
        module_id: id,
        promise: promise_global,
        module: module_global,
      };

      self
        .0
        .context_state
        .borrow_mut()
        .pending_dyn_mod_evaluate
        .push(dyn_import_mod_evaluate);
    } else if tc_scope.has_terminated() || tc_scope.is_execution_terminating() {
      return Err(
        generic_error("Cannot evaluate dynamically imported module, because JavaScript execution has been terminated.")
      );
    } else {
      assert!(status == v8::ModuleStatus::Errored);
    }

    Ok(())
  }

  // TODO(bartlomieju): make it return `ModuleEvaluationFuture`?
  /// Evaluates an already instantiated ES module.
  ///
  /// Returns a receiver handle that resolves when module promise resolves.
  /// Implementors must manually call [`JsRuntime::run_event_loop`] to drive
  /// module evaluation future.
  ///
  /// `Error` can usually be downcast to `JsError` and should be awaited and
  /// checked after [`JsRuntime::run_event_loop`] completion.
  ///
  /// This function panics if module has not been instantiated.
  pub fn mod_evaluate(
    &self,
    isolate: &mut v8::Isolate,
    id: ModuleId,
  ) -> oneshot::Receiver<Result<(), Error>> {
    let state_rc = self.0.state();
    let module_map_rc = self.0.module_map();
    let scope = &mut self.handle_scope(isolate);
    let tc_scope = &mut v8::TryCatch::new(scope);

    let module = module_map_rc
      .borrow()
      .get_handle(id)
      .map(|handle| v8::Local::new(tc_scope, handle))
      .expect("ModuleInfo not found");
    let mut status = module.get_status();
    assert_eq!(
      status,
      v8::ModuleStatus::Instantiated,
      "{} {} ({})",
      if status == v8::ModuleStatus::Evaluated {
        "Module already evaluated. Perhaps you've re-provided a module or extension that was already included in the snapshot?"
      } else {
        "Module not instantiated"
      },
      module_map_rc
        .borrow()
        .get_info_by_id(id)
        .unwrap()
        .name
        .as_str(),
      id,
    );

    let (sender, receiver) = oneshot::channel();

    // IMPORTANT: Top-level-await is enabled, which means that return value
    // of module evaluation is a promise.
    //
    // Because that promise is created internally by V8, when error occurs during
    // module evaluation the promise is rejected, and since the promise has no rejection
    // handler it will result in call to `bindings::promise_reject_callback` adding
    // the promise to pending promise rejection table - meaning JsRuntime will return
    // error on next poll().
    //
    // This situation is not desirable as we want to manually return error at the
    // end of this function to handle it further. It means we need to manually
    // remove this promise from pending promise rejection table.
    //
    // For more details see:
    // https://github.com/denoland/deno/issues/4908
    // https://v8.dev/features/top-level-await#module-execution-order
    {
      let mut state = state_rc.borrow_mut();
      assert!(
        state.pending_mod_evaluate.is_none(),
        "There is already pending top level module evaluation"
      );
      state.pending_mod_evaluate = Some(ModEvaluate {
        promise: None,
        has_evaluated: false,
        handled_promise_rejections: vec![],
        sender,
      });
    }

    let maybe_value = module.evaluate(tc_scope);
    {
      let mut state = state_rc.borrow_mut();
      let pending_mod_evaluate = state.pending_mod_evaluate.as_mut().unwrap();
      pending_mod_evaluate.has_evaluated = true;
    }

    // Update status after evaluating.
    status = module.get_status();

    let has_dispatched_exception = self
      .0
      .runtime_state
      .borrow_mut()
      .dispatched_exception
      .is_some();
    if has_dispatched_exception {
      // This will be overridden in `exception_to_err_result()`.
      let exception = v8::undefined(tc_scope).into();
      let pending_mod_evaluate = {
        let mut state = state_rc.borrow_mut();
        state.pending_mod_evaluate.take().unwrap()
      };
      pending_mod_evaluate
        .sender
        .send(exception_to_err_result(tc_scope, exception, false))
        .expect("Failed to send module evaluation error.");
    } else if let Some(value) = maybe_value {
      assert!(
        status == v8::ModuleStatus::Evaluated
          || status == v8::ModuleStatus::Errored
      );
      let promise = v8::Local::<v8::Promise>::try_from(value)
        .expect("Expected to get promise as module evaluation result");
      let promise_global = v8::Global::new(tc_scope, promise);
      let mut state = state_rc.borrow_mut();
      {
        let pending_mod_evaluate = state.pending_mod_evaluate.as_ref().unwrap();
        let pending_rejection_was_already_handled = pending_mod_evaluate
          .handled_promise_rejections
          .contains(&promise_global);
        if !pending_rejection_was_already_handled {
          state
            .pending_promise_rejections
            .retain(|(key, _)| key != &promise_global);
        }
      }
      let promise_global = v8::Global::new(tc_scope, promise);
      state.pending_mod_evaluate.as_mut().unwrap().promise =
        Some(promise_global);
      tc_scope.perform_microtask_checkpoint();
    } else if tc_scope.has_terminated() || tc_scope.is_execution_terminating() {
      let pending_mod_evaluate = {
        let mut state = state_rc.borrow_mut();
        state.pending_mod_evaluate.take().unwrap()
      };
      pending_mod_evaluate.sender.send(Err(
        generic_error("Cannot evaluate module, because JavaScript execution has been terminated.")
      )).expect("Failed to send module evaluation error.");
    } else {
      assert!(status == v8::ModuleStatus::Errored);
    }

    receiver
  }

  fn dynamic_import_reject(
    &self,
    isolate: &mut v8::Isolate,
    id: ModuleLoadId,
    exception: v8::Global<v8::Value>,
  ) {
    let module_map_rc = self.0.module_map();
    let scope = &mut self.handle_scope(isolate);

    let resolver_handle = module_map_rc
      .borrow_mut()
      .dynamic_import_map
      .remove(&id)
      .expect("Invalid dynamic import id");
    let resolver = resolver_handle.open(scope);

    // IMPORTANT: No borrows to `ModuleMap` can be held at this point because
    // rejecting the promise might initiate another `import()` which will
    // in turn call `bindings::host_import_module_dynamically_callback` which
    // will reach into `ModuleMap` from within the isolate.
    let exception = v8::Local::new(scope, exception);
    resolver.reject(scope, exception).unwrap();
    scope.perform_microtask_checkpoint();
  }

  fn dynamic_import_resolve(
    &self,
    isolate: &mut v8::Isolate,
    id: ModuleLoadId,
    mod_id: ModuleId,
  ) {
    let state_rc = self.0.runtime_state.clone();
    let module_map_rc = self.0.module_map();
    let scope = &mut self.handle_scope(isolate);

    let resolver_handle = module_map_rc
      .borrow_mut()
      .dynamic_import_map
      .remove(&id)
      .expect("Invalid dynamic import id");
    let resolver = resolver_handle.open(scope);

    let module = {
      module_map_rc
        .borrow()
        .get_handle(mod_id)
        .map(|handle| v8::Local::new(scope, handle))
        .expect("Dyn import module info not found")
    };
    // Resolution success
    assert_eq!(module.get_status(), v8::ModuleStatus::Evaluated);

    // IMPORTANT: No borrows to `ModuleMap` can be held at this point because
    // resolving the promise might initiate another `import()` which will
    // in turn call `bindings::host_import_module_dynamically_callback` which
    // will reach into `ModuleMap` from within the isolate.
    let module_namespace = module.get_module_namespace();
    resolver.resolve(scope, module_namespace).unwrap();
    state_rc.borrow_mut().dyn_module_evaluate_idle_counter = 0;
    scope.perform_microtask_checkpoint();
  }

  pub(in crate::runtime) fn prepare_dyn_imports(
    &self,
    isolate: &mut v8::Isolate,
    cx: &mut Context,
  ) -> Poll<Result<(), Error>> {
    if self
      .0
      .module_map()
      .borrow()
      .preparing_dynamic_imports
      .is_empty()
    {
      return Poll::Ready(Ok(()));
    }

    loop {
      let poll_result = self
        .0
        .module_map()
        .borrow_mut()
        .preparing_dynamic_imports
        .poll_next_unpin(cx);

      if let Poll::Ready(Some(prepare_poll)) = poll_result {
        let dyn_import_id = prepare_poll.0;
        let prepare_result = prepare_poll.1;

        match prepare_result {
          Ok(load) => {
            self
              .0
              .module_map()
              .borrow_mut()
              .pending_dynamic_imports
              .push(load.into_future());
          }
          Err(err) => {
            let exception =
              to_v8_type_error(&mut self.handle_scope(isolate), err);
            self.dynamic_import_reject(isolate, dyn_import_id, exception);
          }
        }
        // Continue polling for more prepared dynamic imports.
        continue;
      }

      // There are no active dynamic import loads, or none are ready.
      return Poll::Ready(Ok(()));
    }
  }

  pub(in crate::runtime) fn poll_dyn_imports(
    &self,
    isolate: &mut v8::Isolate,
    cx: &mut Context,
  ) -> Poll<Result<(), Error>> {
    if self
      .0
      .module_map()
      .borrow()
      .pending_dynamic_imports
      .is_empty()
    {
      return Poll::Ready(Ok(()));
    }

    loop {
      let poll_result = self
        .0
        .module_map()
        .borrow_mut()
        .pending_dynamic_imports
        .poll_next_unpin(cx);

      if let Poll::Ready(Some(load_stream_poll)) = poll_result {
        let maybe_result = load_stream_poll.0;
        let mut load = load_stream_poll.1;
        let dyn_import_id = load.id;

        if let Some(load_stream_result) = maybe_result {
          match load_stream_result {
            Ok((request, info)) => {
              // A module (not necessarily the one dynamically imported) has been
              // fetched. Create and register it, and if successful, poll for the
              // next recursive-load event related to this dynamic import.
              let register_result = load.register_and_recurse(
                &mut self.handle_scope(isolate),
                &request,
                info,
              );

              match register_result {
                Ok(()) => {
                  // Keep importing until it's fully drained
                  self
                    .0
                    .module_map()
                    .borrow_mut()
                    .pending_dynamic_imports
                    .push(load.into_future());
                }
                Err(err) => {
                  let exception = match err {
                    ModuleError::Exception(e) => e,
                    ModuleError::Other(e) => {
                      to_v8_type_error(&mut self.handle_scope(isolate), e)
                    }
                  };
                  self.dynamic_import_reject(isolate, dyn_import_id, exception)
                }
              }
            }
            Err(err) => {
              // A non-javascript error occurred; this could be due to a an invalid
              // module specifier, or a problem with the source map, or a failure
              // to fetch the module source code.
              let exception =
                to_v8_type_error(&mut self.handle_scope(isolate), err);
              self.dynamic_import_reject(isolate, dyn_import_id, exception);
            }
          }
        } else {
          // The top-level module from a dynamic import has been instantiated.
          // Load is done.
          let module_id =
            load.root_module_id.expect("Root module should be loaded");
          let result = self.instantiate_module(isolate, module_id);
          if let Err(exception) = result {
            self.dynamic_import_reject(isolate, dyn_import_id, exception);
          }
          self.dynamic_import_module_evaluate(
            isolate,
            dyn_import_id,
            module_id,
          )?;
        }

        // Continue polling for more ready dynamic imports.
        continue;
      }

      // There are no active dynamic import loads, or none are ready.
      return Poll::Ready(Ok(()));
    }
  }

  /// "deno_core" runs V8 with Top Level Await enabled. It means that each
  /// module evaluation returns a promise from V8.
  /// Feature docs: https://v8.dev/features/top-level-await
  ///
  /// This promise resolves after all dependent modules have also
  /// resolved. Each dependent module may perform calls to "import()" and APIs
  /// using async ops will add futures to the runtime's event loop.
  /// It means that the promise returned from module evaluation will
  /// resolve only after all futures in the event loop are done.
  ///
  /// Thus during turn of event loop we need to check if V8 has
  /// resolved or rejected the promise. If the promise is still pending
  /// then another turn of event loop must be performed.
  pub(in crate::runtime) fn evaluate_pending_module(
    &self,
    isolate: &mut v8::Isolate,
  ) {
    let maybe_module_evaluation = self
      .0
      .context_state
      .borrow_mut()
      .pending_mod_evaluate
      .take();
    if maybe_module_evaluation.is_none() {
      return;
    }

    let mut module_evaluation = maybe_module_evaluation.unwrap();
    let state_rc = self.0.state();
    let scope = &mut self.handle_scope(isolate);

    let promise_global = module_evaluation.promise.clone().unwrap();
    let promise = promise_global.open(scope);
    let promise_state = promise.state();
    match promise_state {
      v8::PromiseState::Pending => {
        // NOTE: `poll_event_loop` will decide if
        // runtime would be woken soon
        state_rc.borrow_mut().pending_mod_evaluate = Some(module_evaluation);
      }
      v8::PromiseState::Fulfilled => {
        scope.perform_microtask_checkpoint();
        // Receiver end might have been already dropped, ignore the result
        let _ = module_evaluation.sender.send(Ok(()));
        module_evaluation.handled_promise_rejections.clear();
      }
      v8::PromiseState::Rejected => {
        let exception = promise.result(scope);
        scope.perform_microtask_checkpoint();

        // Receiver end might have been already dropped, ignore the result
        if module_evaluation
          .handled_promise_rejections
          .contains(&promise_global)
        {
          let _ = module_evaluation.sender.send(Ok(()));
          module_evaluation.handled_promise_rejections.clear();
        } else {
          let _ = module_evaluation
            .sender
            .send(exception_to_err_result(scope, exception, false));
        }
      }
    }
  }

  // Returns true if some dynamic import was resolved.
  pub(in crate::runtime) fn evaluate_dyn_imports(
    &self,
    isolate: &mut v8::Isolate,
  ) -> bool {
    let pending = std::mem::take(
      &mut self.0.context_state.borrow_mut().pending_dyn_mod_evaluate,
    );
    if pending.is_empty() {
      return false;
    }
    let mut resolved_any = false;
    let mut still_pending = vec![];
    for pending_dyn_evaluate in pending {
      let maybe_result = {
        let scope = &mut self.handle_scope(isolate);

        let module_id = pending_dyn_evaluate.module_id;
        let promise = pending_dyn_evaluate.promise.open(scope);
        let _module = pending_dyn_evaluate.module.open(scope);
        let promise_state = promise.state();

        match promise_state {
          v8::PromiseState::Pending => {
            still_pending.push(pending_dyn_evaluate);
            None
          }
          v8::PromiseState::Fulfilled => {
            Some(Ok((pending_dyn_evaluate.load_id, module_id)))
          }
          v8::PromiseState::Rejected => {
            let exception = promise.result(scope);
            let exception = v8::Global::new(scope, exception);
            Some(Err((pending_dyn_evaluate.load_id, exception)))
          }
        }
      };

      if let Some(result) = maybe_result {
        resolved_any = true;
        match result {
          Ok((dyn_import_id, module_id)) => {
            self.dynamic_import_resolve(isolate, dyn_import_id, module_id);
          }
          Err((dyn_import_id, exception)) => {
            self.dynamic_import_reject(isolate, dyn_import_id, exception);
          }
        }
      }
    }
    self.0.context_state.borrow_mut().pending_dyn_mod_evaluate = still_pending;
    resolved_any
  }

  /// Asynchronously load specified module and all of its dependencies.
  ///
  /// The module will be marked as "main", and because of that
  /// "import.meta.main" will return true when checked inside that module.
  ///
  /// User must call [`JsRealm::mod_evaluate`] with returned `ModuleId`
  /// manually after load is finished.
  pub async fn load_main_module(
    &self,
    isolate: &mut v8::Isolate,
    specifier: &ModuleSpecifier,
    code: Option<ModuleCode>,
  ) -> Result<ModuleId, Error> {
    let module_map_rc = self.0.module_map();
    if let Some(code) = code {
      let specifier = specifier.as_str().to_owned().into();
      let scope = &mut self.handle_scope(isolate);
      // true for main module
      module_map_rc
        .borrow_mut()
        .new_es_module(scope, true, specifier, code, false)
        .map_err(|e| match e {
          ModuleError::Exception(exception) => {
            let exception = v8::Local::new(scope, exception);
            exception_to_err_result::<()>(scope, exception, false).unwrap_err()
          }
          ModuleError::Other(error) => error,
        })?;
    }

    let mut load =
      ModuleMap::load_main(module_map_rc.clone(), &specifier).await?;

    while let Some(load_result) = load.next().await {
      let (request, info) = load_result?;
      let scope = &mut self.handle_scope(isolate);
      load.register_and_recurse(scope, &request, info).map_err(
        |e| match e {
          ModuleError::Exception(exception) => {
            let exception = v8::Local::new(scope, exception);
            exception_to_err_result::<()>(scope, exception, false).unwrap_err()
          }
          ModuleError::Other(error) => error,
        },
      )?;
    }

    let root_id = load.root_module_id.expect("Root module should be loaded");
    self.instantiate_module(isolate, root_id).map_err(|e| {
      let scope = &mut self.handle_scope(isolate);
      let exception = v8::Local::new(scope, e);
      exception_to_err_result::<()>(scope, exception, false).unwrap_err()
    })?;
    Ok(root_id)
  }

  /// Asynchronously load specified ES module and all of its dependencies.
  ///
  /// This method is meant to be used when loading some utility code that
  /// might be later imported by the main module (ie. an entry point module).
  ///
  /// User must call [`JsRealm::mod_evaluate`] with returned `ModuleId`
  /// manually after load is finished.
  pub async fn load_side_module(
    &self,
    isolate: &mut v8::Isolate,
    specifier: &ModuleSpecifier,
    code: Option<ModuleCode>,
  ) -> Result<ModuleId, Error> {
    let module_map_rc = self.0.module_map();
    if let Some(code) = code {
      let specifier = specifier.as_str().to_owned().into();
      let scope = &mut self.handle_scope(isolate);
      // false for side module (not main module)
      module_map_rc
        .borrow_mut()
        .new_es_module(scope, false, specifier, code, false)
        .map_err(|e| match e {
          ModuleError::Exception(exception) => {
            let exception = v8::Local::new(scope, exception);
            exception_to_err_result::<()>(scope, exception, false).unwrap_err()
          }
          ModuleError::Other(error) => error,
        })?;
    }

    let mut load =
      ModuleMap::load_side(module_map_rc.clone(), &specifier).await?;

    while let Some(load_result) = load.next().await {
      let (request, info) = load_result?;
      let scope = &mut self.handle_scope(isolate);
      load.register_and_recurse(scope, &request, info).map_err(
        |e| match e {
          ModuleError::Exception(exception) => {
            let exception = v8::Local::new(scope, exception);
            exception_to_err_result::<()>(scope, exception, false).unwrap_err()
          }
          ModuleError::Other(error) => error,
        },
      )?;
    }

    let root_id = load.root_module_id.expect("Root module should be loaded");
    self.instantiate_module(isolate, root_id).map_err(|e| {
      let scope = &mut self.handle_scope(isolate);
      let exception = v8::Local::new(scope, e);
      exception_to_err_result::<()>(scope, exception, false).unwrap_err()
    })?;
    Ok(root_id)
  }
}

impl Drop for JsRealm {
  fn drop(&mut self) {
    // Don't do anything special with the main realm
    if self.0.is_main_realm {
      return;
    }

    // There's us and there's the runtime
    if Rc::strong_count(&self.0.context) == 2 {
      self
        .0
        .runtime_state
        .borrow_mut()
        .remove_realm(&self.0.context);
      assert_eq!(Rc::strong_count(&self.0.context), 1);
      self.0.clone().destroy();
      assert_eq!(Rc::strong_count(&self.0.context_state), 1);
      assert_eq!(Rc::strong_count(&self.0.module_map), 1);
    }
  }
}
