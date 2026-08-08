use std::sync::Arc;

use craft_providers::provider::Provider;
use craft_providers::{Model, RequestOptions};

use crate::cancel::CancelToken;
use crate::{AgentError, AgentEvent, EventSender, InterruptSource, SessionMailbox};
use craft_storage::id::SessionRef;

use tracing::{error, warn};

use super::TurnOutcome;

const MAX_REAUTH_ATTEMPTS: u32 = 2;

pub(super) struct AgentIo {
    pub(super) provider: Arc<dyn Provider>,
    pub(super) model: Arc<Model>,
    pub(super) opts: RequestOptions,
    pub(super) timeouts: craft_providers::Timeouts,
    pub(super) fallback_chain: Vec<craft_providers::roles::ChainHop>,
    pub(super) event_tx: EventSender,
    pub(super) cancel: CancelToken,
    pub(super) mailbox: Option<SessionMailbox>,
    pub(super) interrupt_source: Option<Arc<dyn InterruptSource>>,
    pub(super) user_response_rx: Option<Arc<tokio::sync::Mutex<flume::Receiver<String>>>>,
    pub(super) session_id: Option<SessionRef>,
    pub(super) reauth_attempts: u32,
}

impl AgentIo {
    pub(super) async fn wait_for_reauth(
        &mut self,
        err: AgentError,
        num_turns: u32,
    ) -> Result<TurnOutcome, AgentError> {
        if self.reauth_attempts >= MAX_REAUTH_ATTEMPTS {
            error!(error = %err, attempts = self.reauth_attempts, "max re-auth attempts reached");
            return Err(err);
        }
        let Some(rx) = &self.user_response_rx else {
            error!(error = %err, model = %self.model.id, num_turns, "stream_message failed");
            return Err(err);
        };
        self.reauth_attempts += 1;
        warn!(error = %err, attempt = self.reauth_attempts, "auth error, waiting for re-authentication");
        self.event_tx.send(AgentEvent::AuthRequired)?;
        let rx = rx.lock().await;
        match tokio::select! {
            r = rx.recv_async() => r.map_err(|_| flume::RecvError::Disconnected),
            _ = self.cancel.cancelled() => Err(flume::RecvError::Disconnected),
        } {
            Ok(_) => {
                self.provider.refresh_auth().await?;
                Ok(TurnOutcome::Continue)
            }
            Err(_) => Err(AgentError::Cancelled),
        }
    }
}
