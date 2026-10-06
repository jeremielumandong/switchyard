//! Questions the runtime asks the user (unknown host keys, passwords, MFA codes). Each is
//! an event with a request id; the UI answers with [`Command::AnswerPrompt`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use secrecy::SecretString;
use switchyard_remote::ssh::{HostKeyDecision, HostKeyRequest, InteractiveRequest, SshPrompter};
use tokio::sync::oneshot;

use crate::bus::{Event, PromptAnswer, RequestId};
use crate::runtime::EventSender;

/// Prompt ids start high so they never collide with UI request ids in logs.
const FIRST_ID: u64 = 1 << 48;

/// Asks over the event bus and waits for the matching answer.
pub struct BusPrompter {
    events: EventSender,
    next: AtomicU64,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<PromptAnswer>>>,
}

impl BusPrompter {
    /// A prompter emitting on `events`.
    pub fn new(events: EventSender) -> Arc<Self> {
        Arc::new(Self {
            events,
            next: AtomicU64::new(FIRST_ID),
            pending: Mutex::default(),
        })
    }

    fn ask(&self, make: impl FnOnce(RequestId) -> Event) -> oneshot::Receiver<PromptAnswer> {
        self.ask_with_id(make).1
    }

    /// Raise a prompt; the id is needed to close it with [`BusPrompter::close`].
    pub(crate) fn ask_with_id(
        &self,
        make: impl FnOnce(RequestId) -> Event,
    ) -> (RequestId, oneshot::Receiver<PromptAnswer>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        self.events.emit(make(id));
        (id, rx)
    }

    /// Withdraw a prompt the user no longer needs to answer.
    pub(crate) fn close(&self, request: RequestId) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&request);
        self.events.emit(Event::PromptClosed { request });
    }

    /// Deliver the user's answer.
    pub fn answer(&self, request: RequestId, answer: PromptAnswer) {
        let tx = self
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&request);
        if let Some(tx) = tx {
            let _ = tx.send(answer);
        }
    }
}

impl SshPrompter for BusPrompter {
    fn host_key(&self, req: HostKeyRequest) -> BoxFuture<'static, HostKeyDecision> {
        let rx = self.ask(|request| Event::HostKeyPrompt { request, key: req });
        Box::pin(async move {
            match rx.await {
                Ok(PromptAnswer::HostKey(d)) => d,
                _ => HostKeyDecision::Reject,
            }
        })
    }

    fn secret(&self, host: String, prompt: String) -> BoxFuture<'static, Option<SecretString>> {
        let rx = self.ask(|request| Event::SecretPrompt {
            request,
            host,
            prompt,
        });
        Box::pin(async move {
            match rx.await {
                Ok(PromptAnswer::Secret(s)) => s,
                _ => None,
            }
        })
    }

    fn interactive(
        &self,
        req: InteractiveRequest,
    ) -> BoxFuture<'static, Option<Vec<SecretString>>> {
        let rx = self.ask(|request| Event::InteractivePrompt { request, req });
        Box::pin(async move {
            match rx.await {
                Ok(PromptAnswer::Interactive(a)) => a,
                _ => None,
            }
        })
    }
}
