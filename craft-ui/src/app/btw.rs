use std::sync::Arc;

use flume::Sender;

use craft_providers::provider::Provider;
use craft_providers::{Message, Model, ProviderEvent, RequestOptions, ThinkingConfig};
use serde_json::Value;

use crate::components::btw_modal::BtwEvent;

use super::App;

const BTW_REMINDER: &str = "<system-reminder>This is a one-shot side question. You have no tools available. Do not ask follow-up questions. Answer only from context you already have.</system-reminder>";

const BTW_FALLBACK_SYSTEM: &str = "Answer the user's question concisely. No tools available.";

pub(crate) fn btw_question(question: &str) -> Message {
    Message::user(format!("{BTW_REMINDER}\n\n{question}"))
}

impl App {
    pub(crate) fn start_btw(
        &mut self,
        question: String,
        provider: Arc<dyn Provider>,
        model: Model,
    ) {
        let mut messages = self
            .shared_history
            .as_ref()
            .map(|h| Vec::clone(&h.load()))
            .unwrap_or_default();

        let system: Arc<String> = self
            .btw_system
            .as_ref()
            .map(|s| Arc::clone(&s.load()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| Arc::new(BTW_FALLBACK_SYSTEM.to_owned()));

        let (tx, rx) = flume::bounded(64);
        self.btw_modal.open(&question, rx);
        messages.push(btw_question(&question));

        let session_id = self.state.session.id.clone();
        tokio::spawn(run_btw(provider, model, messages, tx, (*system).clone(), Some(session_id)));
    }
}

async fn run_btw(
    provider: Arc<dyn Provider>,
    model: Model,
    messages: Vec<Message>,
    btw_tx: Sender<BtwEvent>,
    system: String,
    session_id: Option<String>,
) {
    let (event_tx, event_rx) = flume::unbounded();
    let tools = Value::Array(vec![]);

    let stream_fut = provider.stream_message(
        &model,
        &messages,
        &system,
        &tools,
        &event_tx,
        RequestOptions { thinking: ThinkingConfig::Off, fast: false },
        session_id.as_deref(),
    );

    let forward_fut = async {
        while let Ok(event) = event_rx.recv_async().await {
            let delta = match event {
                ProviderEvent::TextDelta { text } | ProviderEvent::ThinkingDelta { text } => text,
                _ => continue,
            };
            if btw_tx.send(BtwEvent::TextDelta(delta)).is_err() {
                return;
            }
        }
    };

    let (result, _) = tokio::join!(stream_fut, forward_fut);

    match result {
        Ok(_) => {
            let _ = btw_tx.send(BtwEvent::Done);
        }
        Err(e) => {
            let _ = btw_tx.send(BtwEvent::Error(e.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_reminder_before_question() {
        let msg = btw_question("what is foo?");
        let text = msg.content.first().and_then(|b| match b {
            craft_providers::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        }).unwrap();
        assert!(text.starts_with("<system-reminder>"), "must start with reminder");
        assert!(text.ends_with("what is foo?"), "must end with question");
    }
}
