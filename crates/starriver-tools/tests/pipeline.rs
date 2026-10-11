//! End-to-end pipeline integration tests over the public plugin API.

use std::sync::{Arc, Mutex};

use cordis_rslite::{Context, EventBus, PluginRegistry, Value};
use starriver_kernel::llm::{CallId, ContentBlock};
use starriver_kernel::tools::{
    PostToolDecision, PreToolDecision, ToolDefinition, ToolError, ToolExecutionInput,
    ToolExecutionResult, ToolExecutor, ToolRunContext, ToolRuntime, ToolsExecute, ToolsPostExecute,
    ToolsPreExecute, ToolsResult,
};
use starriver_tools::{in_memory_tools_plugin, InMemoryToolRuntime};

struct Echo;

impl ToolExecutor for Echo {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        Box::pin(async { Ok(arguments) })
    }
}

fn echo_definition() -> ToolDefinition {
    ToolDefinition {
        requires_approval: false,
        deny_in_readonly: false,
        name: "echo".into(),
        description: "echo".into(),
        parameters: Value::new(()),
        output: Value::new(()),
        render: Arc::new(|_, value| vec![ContentBlock::Text(format!("{value:?}"))]),
        executor: Arc::new(Echo),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
    }
}

fn execution(name: &str) -> ToolExecutionInput {
    ToolExecutionInput {
        cwd: None,
        sandbox_mode: None,
        call_id: CallId("c1".into()),
        root_call_id: None,
        name: name.into(),
        arguments: Value::new(1u64),
        agent_id: None,
        parent: None,
        cancellation: None,
    }
}

async fn booted_runtime() -> (Context, EventBus, Arc<InMemoryToolRuntime>) {
    let ctx = Context::new();
    let bus = EventBus::new();
    let registry = PluginRegistry::new();
    registry.register(in_memory_tools_plugin());
    let _effects = registry.boot(&ctx, &bus).await.unwrap();
    let runtime = ctx.require::<InMemoryToolRuntime>().unwrap();
    (ctx, bus, runtime)
}

#[tokio::test]
async fn booted_plugin_runs_full_pipeline_in_order_and_notifies() {
    let (_ctx, bus, runtime) = booted_runtime().await;
    let _handle = runtime.register(echo_definition()).unwrap();

    let order = Arc::new(Mutex::new(Vec::new()));
    let _pre = bus.on_waterfall::<ToolsPreExecute, _>({
        let order = Arc::clone(&order);
        move |_, next| {
            let order = Arc::clone(&order);
            Box::pin(async move {
                order.lock().unwrap().push("pre");
                next().await
            })
        }
    });
    let _execute = bus.on_waterfall::<ToolsExecute, _>({
        let order = Arc::clone(&order);
        move |_, next| {
            let order = Arc::clone(&order);
            Box::pin(async move {
                order.lock().unwrap().push("execute:before");
                let result = next().await;
                order.lock().unwrap().push("execute:after");
                result
            })
        }
    });
    let _post = bus.on_waterfall::<ToolsPostExecute, _>({
        let order = Arc::clone(&order);
        move |_, next| {
            let order = Arc::clone(&order);
            Box::pin(async move {
                order.lock().unwrap().push("post");
                next().await
            })
        }
    });
    let notified = Arc::new(Mutex::new(None));
    let _result = bus.on::<ToolsResult, _>({
        let notified = Arc::clone(&notified);
        move |event| {
            let notified = Arc::clone(&notified);
            Box::pin(async move {
                *notified.lock().unwrap() = Some(event.result.clone());
                Ok(Value::new(()))
            })
        }
    });

    let result = runtime.execute(execution("echo")).await.unwrap();
    assert!(matches!(result, ToolExecutionResult::Success { .. }));
    assert_eq!(
        *order.lock().unwrap(),
        vec!["pre", "execute:before", "execute:after", "post"]
    );

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(matches!(
        notified.lock().unwrap().clone(),
        Some(ToolExecutionResult::Success { .. })
    ));
}

#[tokio::test]
async fn booted_plugin_denies_via_policy_and_notifies_failure() {
    let (_ctx, bus, runtime) = booted_runtime().await;
    let _handle = runtime.register(echo_definition()).unwrap();
    let _policy = bus.on_waterfall::<ToolsPreExecute, _>(move |_, _next| {
        Box::pin(async {
            Ok(PreToolDecision::Deny {
                reason: "integration policy".into(),
            })
        })
    });
    let notified = Arc::new(Mutex::new(None));
    let _result = bus.on::<ToolsResult, _>({
        let notified = Arc::clone(&notified);
        move |event| {
            let notified = Arc::clone(&notified);
            Box::pin(async move {
                *notified.lock().unwrap() = Some(event.result.clone());
                Ok(Value::new(()))
            })
        }
    });

    let result = runtime.execute(execution("echo")).await.unwrap();
    let ToolExecutionResult::Failure { error, .. } = result else {
        panic!("expected failure");
    };
    assert_eq!(error.code, "DENIED");
    assert!(error.message.contains("integration policy"));

    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert!(matches!(
        notified.lock().unwrap().clone(),
        Some(ToolExecutionResult::Failure { .. })
    ));
}

#[tokio::test]
async fn booted_plugin_blocks_result_via_post_execute() {
    let (_ctx, bus, runtime) = booted_runtime().await;
    let _handle = runtime.register(echo_definition()).unwrap();
    let _policy = bus.on_waterfall::<ToolsPostExecute, _>(move |_, _next| {
        Box::pin(async {
            Ok(PostToolDecision::Block {
                feedback: vec![ContentBlock::Text("integration feedback".into())],
                additional_contexts: Vec::new(),
            })
        })
    });

    let result = runtime.execute(execution("echo")).await.unwrap();
    let ToolExecutionResult::Failure { error, content, .. } = result else {
        panic!("expected failure");
    };
    assert_eq!(error.code, "BLOCKED");
    assert!(
        matches!(content.as_slice(), [ContentBlock::Text(text)] if text == "integration feedback")
    );
}
