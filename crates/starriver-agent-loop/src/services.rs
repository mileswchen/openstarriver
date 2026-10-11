//! Object-safe interface views over the kernel seam traits.
//!
//! The kernel seams extend `Service`, whose associated const `NAME` makes
//! them dyn-incompatible. These thin views drop the `Service` supertrait and
//! blanket-delegate to the real seams, so the loop can inject
//! `Arc<dyn …View>` while depending only on `starriver-kernel`.

use std::sync::Arc;

use cordis_rslite::{BoxFuture, EffectHandle};
use futures::stream::BoxStream;
use starriver_kernel::llm::{
    GenerateOptions, LlmCallConfig, LlmError, LlmRuntime, PreparedLlmCall, StreamChunk, ToolSchema,
};
use starriver_kernel::persistence::{PersistenceError, SessionInspection};
use starriver_kernel::session::SessionHeader;
use starriver_kernel::session::{
    EnterHandle, PrepareOptions, Session, SessionError, SessionId, SessionPreparation, SessionStore,
};
use starriver_kernel::tools::{
    ScheduledToolDispatch, ScheduledToolPreparation, ToolError, ToolExecutionInput,
    ToolExecutionMode, ToolExecutionResult, ToolRunContext, ToolRuntime,
};
use tokio_util::sync::CancellationToken;

/// Object-safe view over the persistence default crate's prepare extension.
/// `prepare` stays outside the kernel seam; this view lets the loop depend
/// only on a trait while tests mock or inject the real SQLite service.
pub trait PersistenceView: Send + Sync {
    fn prepare(
        &self,
        id: SessionId,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<
        Result<
            starriver_session_persistence::prepare::PersistedPreparation,
            starriver_session_persistence::prepare::PrepareError,
        >,
    >;
    fn load(
        &self,
        id: SessionId,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<SessionInspection, PersistenceError>>;
    fn list(
        &self,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<Vec<SessionHeader>, PersistenceError>>;
    fn remove(
        &self,
        id: SessionId,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<(), PersistenceError>>;
}

impl PersistenceView for starriver_session_persistence::SqliteSessionPersistence {
    fn prepare(
        &self,
        id: SessionId,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<
        Result<
            starriver_session_persistence::prepare::PersistedPreparation,
            starriver_session_persistence::prepare::PrepareError,
        >,
    > {
        let coordinator = Arc::clone(self.coordinator());
        Box::pin(async move { coordinator.prepare(id, cancel).await })
    }
    fn load(
        &self,
        id: SessionId,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<SessionInspection, PersistenceError>> {
        Box::pin(starriver_kernel::persistence::SessionPersistence::load(
            self, id, cancel,
        ))
    }
    fn list(
        &self,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<Vec<SessionHeader>, PersistenceError>> {
        Box::pin(starriver_kernel::persistence::SessionPersistence::list(
            self, cancel,
        ))
    }
    fn remove(
        &self,
        id: SessionId,
        cancel: Option<CancellationToken>,
    ) -> BoxFuture<Result<(), PersistenceError>> {
        Box::pin(starriver_kernel::persistence::SessionPersistence::remove(
            self, id, cancel,
        ))
    }
}

pub trait SessionStoreView: Send + Sync {
    fn prepare(
        &self,
        id: SessionId,
        options: PrepareOptions,
    ) -> Result<SessionPreparation, SessionError>;
    fn enter(&self, session: Session) -> Result<EnterHandle, SessionError>;
    fn announce(&self, session: &Session) -> Result<(), SessionError>;
    fn get(&self, id: &SessionId) -> Option<Session>;
    fn remove(&self, id: &SessionId) -> Option<Session>;
}

impl<T: SessionStore> SessionStoreView for T {
    fn prepare(
        &self,
        id: SessionId,
        options: PrepareOptions,
    ) -> Result<SessionPreparation, SessionError> {
        SessionStore::prepare(self, id, options)
    }
    fn enter(&self, session: Session) -> Result<EnterHandle, SessionError> {
        SessionStore::enter(self, session)
    }
    fn announce(&self, session: &Session) -> Result<(), SessionError> {
        SessionStore::announce(self, session)
    }
    fn get(&self, id: &SessionId) -> Option<Session> {
        SessionStore::get(self, id)
    }
    fn remove(&self, id: &SessionId) -> Option<Session> {
        SessionStore::remove(self, id)
    }
}

pub trait ToolRuntimeView: Send + Sync {
    fn schemas(&self) -> Vec<ToolSchema>;
    fn schemas_for(&self, agent_id: Option<&str>) -> Vec<ToolSchema>;
    fn sdk_schemas(&self) -> Vec<ToolSchema>;
    fn is_ptc_active(&self) -> bool;
    fn restrict_for(&self, agent_id: &str, allow: Vec<String>) -> Result<EffectHandle, ToolError>;
    fn execution_mode(&self, input: &ToolExecutionInput) -> ToolExecutionMode;
    fn prepare(
        &self,
        input: ToolExecutionInput,
    ) -> BoxFuture<Result<ScheduledToolPreparation, ToolError>>;
    fn dispatch(&self, exec: ToolRunContext)
        -> BoxFuture<Result<ScheduledToolDispatch, ToolError>>;
    fn finalize(
        &self,
        exec: ToolRunContext,
        result: ToolExecutionResult,
    ) -> BoxFuture<Result<ToolExecutionResult, ToolError>>;
    fn finish(
        &self,
        exec: ToolRunContext,
        result: ToolExecutionResult,
    ) -> Result<ToolExecutionResult, ToolError>;
    fn presentation_meta(
        &self,
        input: &ToolExecutionInput,
        value: &cordis_rslite::Value,
    ) -> Option<serde_json::Value>;
}

impl<T: ToolRuntime> ToolRuntimeView for T {
    fn schemas(&self) -> Vec<ToolSchema> {
        ToolRuntime::schemas(self)
    }
    fn schemas_for(&self, agent_id: Option<&str>) -> Vec<ToolSchema> {
        ToolRuntime::schemas_for(self, agent_id)
    }
    fn sdk_schemas(&self) -> Vec<ToolSchema> {
        ToolRuntime::sdk_schemas(self)
    }
    fn is_ptc_active(&self) -> bool {
        ToolRuntime::is_ptc_active(self)
    }
    fn restrict_for(&self, agent_id: &str, allow: Vec<String>) -> Result<EffectHandle, ToolError> {
        ToolRuntime::restrict_for(self, agent_id, allow)
    }
    fn execution_mode(&self, input: &ToolExecutionInput) -> ToolExecutionMode {
        ToolRuntime::execution_mode(self, input)
    }
    fn prepare(
        &self,
        input: ToolExecutionInput,
    ) -> BoxFuture<Result<ScheduledToolPreparation, ToolError>> {
        ToolRuntime::prepare(self, input)
    }
    fn dispatch(
        &self,
        exec: ToolRunContext,
    ) -> BoxFuture<Result<ScheduledToolDispatch, ToolError>> {
        ToolRuntime::dispatch(self, exec)
    }
    fn finalize(
        &self,
        exec: ToolRunContext,
        result: ToolExecutionResult,
    ) -> BoxFuture<Result<ToolExecutionResult, ToolError>> {
        ToolRuntime::finalize(self, exec, result)
    }
    fn finish(
        &self,
        exec: ToolRunContext,
        result: ToolExecutionResult,
    ) -> Result<ToolExecutionResult, ToolError> {
        ToolRuntime::finish(self, exec, result)
    }
    fn presentation_meta(
        &self,
        input: &ToolExecutionInput,
        value: &cordis_rslite::Value,
    ) -> Option<serde_json::Value> {
        ToolRuntime::presentation_meta(self, input, value)
    }
}

pub trait LlmRuntimeView: Send + Sync {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
        cancellation: Option<CancellationToken>,
    ) -> BoxFuture<Result<PreparedLlmCall, LlmError>>;
    fn stream(&self, options: GenerateOptions)
        -> BoxStream<'static, Result<StreamChunk, LlmError>>;
}

impl<T: LlmRuntime> LlmRuntimeView for T {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
        cancellation: Option<CancellationToken>,
    ) -> BoxFuture<Result<PreparedLlmCall, LlmError>> {
        LlmRuntime::prepare_call(self, config, cancellation)
    }
    fn stream(
        &self,
        options: GenerateOptions,
    ) -> BoxStream<'static, Result<StreamChunk, LlmError>> {
        LlmRuntime::stream(self, options)
    }
}
