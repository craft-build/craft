//! The question seam: the `question` tool parks on the user through
//! this asker, mirroring the approval gate's parking pattern — event out,
//! oneshot back, cancellation and a 30-minute ask timeout.

use tokio::sync::{mpsc, oneshot};

use crate::tools::{ASK_TIMEOUT, AskQuestions, QuestionAnswer, QuestionSpec};

use super::SharedPark;
use crate::tui::provider::AgentEvent;

pub(super) struct QuestionAsker {
    park: SharedPark,
    tx: mpsc::UnboundedSender<AgentEvent>,
    cancel: std::sync::Arc<arc_swap::ArcSwap<animist::agent::Cancellation>>,
}

impl QuestionAsker {
    pub(super) fn new(
        park: SharedPark,
        tx: mpsc::UnboundedSender<AgentEvent>,
        cancel: std::sync::Arc<arc_swap::ArcSwap<animist::agent::Cancellation>>,
    ) -> Self {
        Self { park, tx, cancel }
    }
}

fn dismissed() -> QuestionAnswer {
    QuestionAnswer {
        dismissed: true,
        answers: vec![],
    }
}

impl AskQuestions for QuestionAsker {
    fn ask(&self, questions: Vec<QuestionSpec>) -> crate::harness::BoxFuture<QuestionAnswer> {
        let QuestionAsker { park, tx, cancel } = QuestionAsker {
            park: self.park.clone(),
            tx: self.tx.clone(),
            cancel: self.cancel.clone(),
        };
        Box::pin(async move {
            let id = crate::id::CraftId::generate().to_string();
            // Register before emitting: an answer racing in on event
            // receipt must find the oneshot already parked.
            let (answer_tx, mut answer_rx) = oneshot::channel();
            park.lock().await.pending_question = Some((id.clone(), answer_tx));
            let _ = tx.send(AgentEvent::QuestionRequest {
                id: id.clone(),
                questions: questions.clone(),
            });
            let turn_cancel = (*cancel.load_full()).clone();
            let answer = tokio::select! {
                biased;
                () = turn_cancel.cancelled() => {
                    dismissed()
                }
                answer = tokio::time::timeout(ASK_TIMEOUT, &mut answer_rx) => {
                    answer.unwrap_or_else(|_| Ok(dismissed()))
                        .unwrap_or_else(|_| dismissed())
                }
            };
            park.lock().await.pending_question = None;
            let _ = tx.send(AgentEvent::QuestionResolved { id });
            answer
        })
    }
}

/// Deliver the user's answer to the parked `question` call, if the id
/// matches the currently pending one.
pub(super) async fn answer_question(park: &SharedPark, id: String, answer: QuestionAnswer) {
    let mut slots = park.lock().await;
    if matches!(&slots.pending_question, Some((pid, _)) if *pid == id)
        && let Some((_, sender)) = slots.pending_question.take()
    {
        let _ = sender.send(answer);
    }
}

#[cfg(test)]
mod tests {
    use super::super::ParkSlots;
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    type TestCell = std::sync::Arc<arc_swap::ArcSwap<animist::agent::Cancellation>>;

    fn test_cell() -> TestCell {
        std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
            animist::agent::Cancellation::default(),
        ))
    }

    fn spec() -> QuestionSpec {
        QuestionSpec {
            question: "Which?".into(),
            header: None,
            options: vec![],
            multi_select: false,
        }
    }

    #[tokio::test]
    async fn parks_on_the_form_and_returns_the_answer() {
        let state = Arc::new(Mutex::new(ParkSlots::default()));
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let cell = test_cell();
        let asker = QuestionAsker::new(state.clone(), tx, cell.clone());

        let pending = tokio::spawn(async move { asker.ask(vec![spec()]).await });
        let request = loop {
            match rx.recv().await.expect("channel open") {
                event @ AgentEvent::QuestionRequest { .. } => break event,
                _ => continue,
            }
        };
        let AgentEvent::QuestionRequest { id, questions } = request else {
            unreachable!();
        };
        assert_eq!(questions.len(), 1);
        while state.lock().await.pending_question.is_none() {
            tokio::task::yield_now().await;
        }

        answer_question(
            &state,
            id,
            QuestionAnswer {
                dismissed: false,
                answers: vec![vec!["X".into()]],
            },
        )
        .await;
        let answer = pending.await.unwrap();
        assert!(!answer.dismissed);
        assert_eq!(answer.answers, vec![vec!["X".to_string()]]);
        assert!(state.lock().await.pending_question.is_none());
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::QuestionResolved { .. })
        ));
    }

    /// The oneshot is registered before the event is emitted, so an
    /// answer delivered the instant the request arrives still lands.
    #[tokio::test]
    async fn immediate_answer_on_event_receipt_lands() {
        let state = Arc::new(Mutex::new(ParkSlots::default()));
        let (tx, mut rx) = mpsc::unbounded_channel::<AgentEvent>();
        let cell = test_cell();
        let asker = QuestionAsker::new(state.clone(), tx, cell.clone());

        let pending = tokio::spawn(async move { asker.ask(vec![spec()]).await });
        let request = loop {
            match rx.recv().await.expect("channel open") {
                event @ AgentEvent::QuestionRequest { .. } => break event,
                _ => continue,
            }
        };
        let AgentEvent::QuestionRequest { id, .. } = request else {
            unreachable!();
        };
        // No yield loop: answer right away, as the UI would on receipt.
        answer_question(
            &state,
            id,
            QuestionAnswer {
                dismissed: false,
                answers: vec![vec!["Y".into()]],
            },
        )
        .await;
        let answer = pending.await.unwrap();
        assert!(!answer.dismissed);
        assert_eq!(answer.answers, vec![vec!["Y".to_string()]]);
    }

    #[tokio::test]
    async fn cancel_dismisses_the_parked_question() {
        let state = Arc::new(Mutex::new(ParkSlots::default()));
        let (tx, _rx) = mpsc::unbounded_channel::<AgentEvent>();
        let cell = test_cell();
        let asker = QuestionAsker::new(state.clone(), tx, cell.clone());

        let pending = tokio::spawn(async move { asker.ask(vec![spec()]).await });
        while state.lock().await.pending_question.is_none() {
            tokio::task::yield_now().await;
        }
        cell.load().cancel();
        assert!(pending.await.unwrap().dismissed);
        assert!(state.lock().await.pending_question.is_none());
    }

    #[tokio::test]
    async fn stale_ids_do_not_consume_the_pending_slot() {
        let state = Arc::new(Mutex::new(ParkSlots::default()));
        let (tx, _rx) = oneshot::channel();
        state.lock().await.pending_question = Some(("current".into(), tx));

        answer_question(
            &state,
            "stale".into(),
            QuestionAnswer {
                dismissed: true,
                answers: vec![],
            },
        )
        .await;
        assert!(matches!(
            &state.lock().await.pending_question,
            Some((id, _)) if id == "current"
        ));
    }
}
