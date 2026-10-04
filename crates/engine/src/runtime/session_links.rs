//! Linked sessions. Two linked sessions can read and message each other
//! through the app CLI without `/operator`. Links persist in the store's
//! `session_links` table; the message budget that stops two agents from
//! messaging each other forever lives only in memory.

use std::collections::HashMap;

use gpui::{App, AppContext as _, Context};

use super::engine::Engine;

/// Agent messages one link allows before a user message in either session
/// resets the count.
pub const LINK_MESSAGE_BUDGET: u32 = 5;

/// The header a message sent over a link starts with. A submitted message
/// with this header came from an agent, so it does not reset the budget.
const LINK_MESSAGE_PREFIX: &str = "From linked session \"";

/// The links the app knows about, and the agent messages sent over each.
#[derive(Default)]
pub struct SessionLinks {
    /// Each pair once, smaller id first.
    links: Vec<(String, String)>,
    /// Agent messages sent since the last user message, per pair.
    sent: HashMap<(String, String), u32>,
    loaded: bool,
}

fn pair(a: &str, b: &str) -> (String, String) {
    let (first, second) = monocode_store::session_links::ordered(a, b);
    (first.to_string(), second.to_string())
}

/// The text a linked peer receives: who sent it, then the message.
pub fn link_message_text(from_id: &str, from_title: &str, prompt: &str) -> String {
    let title = from_title.replace('"', "'");
    format!("{LINK_MESSAGE_PREFIX}{title}\" ({from_id}):\n\n{prompt}")
}

/// Whether a submitted message is one an agent sent over a link.
pub fn is_link_message(text: &str) -> bool {
    text.starts_with(LINK_MESSAGE_PREFIX)
}

impl SessionLinks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Merge the stored links read at startup with any made since.
    pub fn set_loaded(&mut self, stored: Vec<(String, String)>, cx: &mut Context<Self>) {
        for (a, b) in stored {
            let key = pair(&a, &b);
            if a != b && !self.links.contains(&key) {
                self.links.push(key);
            }
        }
        self.loaded = true;
        cx.notify();
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    /// The sessions linked to `id`, oldest link first. Sessions deleted in
    /// this process are left out.
    pub fn peers(&self, id: &str, cx: &App) -> Vec<String> {
        let writer = Engine::try_global(cx).map(|engine| engine.writer.clone());
        self.links
            .iter()
            .filter_map(|(a, b)| {
                if a == id {
                    Some(b.clone())
                } else if b == id {
                    Some(a.clone())
                } else {
                    None
                }
            })
            .filter(|peer| !writer.as_ref().is_some_and(|w| w.is_deleted(peer)))
            .collect()
    }

    pub fn is_linked(&self, a: &str, b: &str) -> bool {
        self.links.contains(&pair(a, b))
    }

    /// Link two sessions and save the link.
    pub fn link(&mut self, a: &str, b: &str, cx: &mut Context<Self>) -> Result<(), String> {
        monocode_store::session_store::validate_id(a, "session")?;
        monocode_store::session_store::validate_id(b, "session")?;
        if a == b {
            return Err("A session cannot be linked to itself".into());
        }
        let key = pair(a, b);
        if self.links.contains(&key) {
            return Ok(());
        }
        self.links.push(key);
        self.persist(a, b, true, cx);
        cx.notify();
        Ok(())
    }

    /// Remove the link between two sessions and save the change.
    pub fn unlink(&mut self, a: &str, b: &str, cx: &mut Context<Self>) {
        let key = pair(a, b);
        let before = self.links.len();
        self.links.retain(|entry| entry != &key);
        self.sent.remove(&key);
        if self.links.len() != before {
            self.persist(a, b, false, cx);
            cx.notify();
        }
    }

    fn persist(&self, a: &str, b: &str, linked: bool, cx: &mut Context<Self>) {
        let Some(engine) = Engine::try_global(cx) else {
            return;
        };
        let save = engine
            .writer
            .backend()
            .set_session_link(a.to_string(), b.to_string(), linked);
        cx.background_spawn(async move {
            if let Err(error) = save.await {
                log::error!("[session-links] could not save a link: {error}");
            }
        })
        .detach();
    }

    /// Agent messages sent over the link since the last user message.
    pub fn sent(&self, a: &str, b: &str) -> u32 {
        self.sent.get(&pair(a, b)).copied().unwrap_or(0)
    }

    /// Count one agent message from `from` to `to`. Fails when the sessions
    /// are not linked or the link used its budget.
    pub fn spend(&mut self, from: &str, to: &str) -> Result<u32, String> {
        let key = pair(from, to);
        if !self.links.contains(&key) {
            return Err("That session is not linked to this one".into());
        }
        let sent = self.sent.entry(key).or_insert(0);
        if *sent >= LINK_MESSAGE_BUDGET {
            return Err(format!(
                "This link already carried {LINK_MESSAGE_BUDGET} agent messages. Wait for the user to send a message in either session before messaging again."
            ));
        }
        *sent += 1;
        Ok(LINK_MESSAGE_BUDGET - *sent)
    }

    /// Give back a message counted by `spend` that was never delivered.
    pub fn refund(&mut self, from: &str, to: &str) {
        if let Some(sent) = self.sent.get_mut(&pair(from, to)) {
            *sent = sent.saturating_sub(1);
        }
    }

    /// A user message in `id` resets the budget of every link it has.
    pub fn reset_budget(&mut self, id: &str) {
        self.sent.retain(|(a, b), _| a != id && b != id);
    }
}

/// Read the stored links into the entity once the store answers.
pub(crate) fn load_links(cx: &mut App) {
    let Some(engine) = Engine::try_global(cx) else {
        return;
    };
    let links = engine.links.clone();
    let read = engine.writer.backend().list_session_links();
    cx.spawn(async move |cx| match read.await {
        Ok(stored) => links.update(cx, |links, cx| links.set_loaded(stored, cx)),
        Err(error) => log::error!("[session-links] could not read links: {error}"),
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::testing::FakeBackend;
    use gpui::TestAppContext;

    fn init(cx: &mut TestAppContext) -> std::sync::Arc<FakeBackend> {
        let backend = FakeBackend::new();
        cx.update(|cx| {
            Engine::init(
                crate::runtime::engine::EngineConfig::with_backend(backend.clone()),
                cx,
            )
        });
        cx.run_until_parked();
        backend
    }

    #[gpui::test]
    fn links_two_sessions_once_and_saves_the_pair(cx: &mut TestAppContext) {
        let backend = init(cx);
        let links = cx.update(|cx| Engine::global(cx).links.clone());
        links.update(cx, |links, cx| {
            links.link("b", "a", cx).unwrap();
            links.link("a", "b", cx).unwrap();
            assert!(links.link("a", "a", cx).is_err());
        });
        cx.run_until_parked();
        assert_eq!(backend.session_links(), vec![("a".into(), "b".into())]);
        cx.update(|cx| {
            assert_eq!(links.read(cx).peers("a", cx), vec!["b".to_string()]);
            assert_eq!(links.read(cx).peers("b", cx), vec!["a".to_string()]);
        });
        links.update(cx, |links, cx| links.unlink("a", "b", cx));
        cx.run_until_parked();
        assert!(backend.session_links().is_empty());
        cx.update(|cx| assert!(links.read(cx).peers("a", cx).is_empty()));
    }

    #[gpui::test]
    fn stops_agent_messages_after_the_budget_until_a_user_message(cx: &mut TestAppContext) {
        init(cx);
        let links = cx.update(|cx| Engine::global(cx).links.clone());
        links.update(cx, |links, cx| {
            links.link("a", "b", cx).unwrap();
            for _ in 0..LINK_MESSAGE_BUDGET {
                links.spend("a", "b").unwrap();
            }
            let error = links.spend("b", "a").unwrap_err();
            assert!(error.contains("5 agent messages"), "{error}");
            links.reset_budget("b");
            assert_eq!(links.spend("b", "a"), Ok(LINK_MESSAGE_BUDGET - 1));
            links.refund("b", "a");
            assert_eq!(links.sent("a", "b"), 0);
            assert!(links.spend("a", "c").is_err());
        });
    }

    #[test]
    fn recognizes_messages_sent_over_a_link() {
        let text = link_message_text("abc", "Fix \"auth\"", "Done with the API.");
        assert_eq!(
            text,
            "From linked session \"Fix 'auth'\" (abc):\n\nDone with the API."
        );
        assert!(is_link_message(&text));
        assert!(!is_link_message("Please check the linked session"));
    }
}
