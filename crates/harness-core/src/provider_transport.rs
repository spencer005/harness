//! Provider transport actor.
//!
//! The [`ProviderActor`] is the harness actor's single point of contact for any
//! provider transport. It currently wraps the Responses WebSocket/HTTPS actor
//! (`ResponsesWsActor`) and exposes provider-neutral commands. In the future it
//! can host dedicated drivers per provider kind.

use crate::{
    actors::{
        ActorHandle, ActorReceiver, ActorSender, DEFAULT_ACTOR_MAILBOX_CAPACITY,
        channel as actor_channel,
    },
    provider_runtime::ProviderRuntime,
    responses::{
        ResponsesCommand, ResponsesEvent, ResponsesHttpsTransport, ResponsesWsActor,
        ResponsesWsPool, WsPoolConfig,
    },
};

/// Commands accepted by the provider transport actor.
#[derive(Debug, Clone)]
pub enum ProviderCommand {
    /// Forward a Responses request to the active transport.
    Submit {
        /// Responses request to send.
        request: crate::responses::ResponsesRequest,
        /// Event sink for responses events.
        events: ActorSender<ResponsesEvent>,
    },
    /// Close idle transport connections.
    CloseIdle,
    /// Shut down the actor.
    Shutdown,
}

/// Events emitted by the provider transport actor.
pub type ProviderEvent = ResponsesEvent;

/// Actor that owns the active transport for the current provider runtime.
#[derive(Debug)]
pub struct ProviderActor {
    transport: ActorHandle<ResponsesCommand>,
    rx: ActorReceiver<ProviderCommand>,
}

impl ProviderActor {
    /// Spawn a provider actor from a validated runtime and return its handle.
    pub fn spawn(runtime: &ProviderRuntime) -> ActorHandle<ProviderCommand> {
        let transport = spawn_responses_transport(runtime);
        Self::spawn_with_transport(transport)
    }

    /// Spawn a provider actor with an explicit inner transport handle.
    pub fn spawn_with_transport(
        transport: ActorHandle<ResponsesCommand>,
    ) -> ActorHandle<ProviderCommand> {
        let (handle, rx) = actor_channel(DEFAULT_ACTOR_MAILBOX_CAPACITY);
        let actor = Self { transport, rx };
        tokio::spawn(actor.run());
        handle
    }

    /// Return a provider actor handle that wraps an existing Responses actor.
    ///
    /// This is a migration helper while the harness transitions from directly
    /// holding `ResponsesWsActor` to holding `ProviderActor`.
    pub fn wrap(responses: ActorHandle<ResponsesCommand>) -> ActorHandle<ProviderCommand> {
        Self::spawn_with_transport(responses)
    }

    async fn run(self) {
        let rx = self.rx;
        while let Ok(command) = rx.recv().await {
            match command {
                ProviderCommand::Submit { request, events } => {
                    let _ = self
                        .transport
                        .send(ResponsesCommand::Submit { request, events })
                        .await;
                }
                ProviderCommand::CloseIdle => {
                    let _ = self.transport.send(ResponsesCommand::CloseIdle).await;
                }
                ProviderCommand::Shutdown => {
                    let _ = self.transport.send(ResponsesCommand::Shutdown).await;
                    break;
                }
            }
        }
    }
}

fn spawn_responses_transport(runtime: &ProviderRuntime) -> ActorHandle<ResponsesCommand> {
    let api = runtime.api.clone();
    let auth = runtime.auth.clone().into_responses_auth();
    let default_headers = runtime.default_headers.clone();
    match &runtime.profile.driver {
        crate::providers::ProviderDriverConfig::CodexWsResponses { .. } => {
            let pool = ResponsesWsPool::new(api, auth, default_headers, WsPoolConfig::default());
            ResponsesWsActor::spawn(pool)
        }
        crate::providers::ProviderDriverConfig::HttpsResponses { .. } => {
            let request_timeout = runtime
                .profile
                .driver
                .request_timeout()
                .expect("HTTPS driver has request timeout");
            ResponsesWsActor::spawn_https(ResponsesHttpsTransport::new(
                api,
                auth,
                default_headers,
                request_timeout,
            ))
        }
    }
}

/// Build a provider transport actor from a runtime.
pub fn provider_transport_for_runtime(runtime: &ProviderRuntime) -> ActorHandle<ProviderCommand> {
    ProviderActor::spawn(runtime)
}
