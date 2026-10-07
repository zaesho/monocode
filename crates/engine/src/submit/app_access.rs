//! App CLI access for threads without `/operator`: the "Let agents open
//! sessions" actions and the actions between linked sessions. The
//! `<monocode_app>` block here is the short version of the `/operator` one.
//! `orchestration::agent_app` enforces the same limits on each call.

use monocode_core::Session;

use crate::runtime::session_links::LINK_MESSAGE_BUDGET;

/// What a turn's agent may do with the app CLI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnAppAccess {
    /// A `/operator` turn enabled every action in this thread.
    pub operator: bool,
    /// "Let agents open sessions" is on and applies to this thread.
    pub open_sessions: bool,
    /// "Review agent-opened sessions before they run" is on.
    pub review_opened: bool,
    /// Linked sessions as (id, title).
    pub peers: Vec<(String, String)>,
}

impl TurnAppAccess {
    /// The access a turn in `session` gets.
    pub fn for_session(
        session: &Session,
        operator: bool,
        open_sessions_setting: bool,
        review_setting: bool,
        peers: Vec<(String, String)>,
    ) -> Self {
        // Orchestration leads, their workers, and inbox asks never reach the
        // app CLI; the executor refuses them too.
        let eligible = session.orchestration_lead_id.is_none() && session.inbox_ask.is_none();
        Self {
            operator,
            open_sessions: eligible && open_sessions_setting,
            review_opened: review_setting,
            peers: if eligible { peers } else { Vec::new() },
        }
    }

    /// Whether the turn may call the app CLI at all.
    pub fn any(&self) -> bool {
        self.operator || self.open_sessions || !self.peers.is_empty()
    }

    /// The `<monocode_app>` block for the actions `/operator` does not
    /// already explain, with `{cli}` where the CLI path goes. `None` when
    /// there is nothing to say.
    pub fn note(&self) -> Option<String> {
        let open_sessions = self.open_sessions && !self.operator;
        if !open_sessions && self.peers.is_empty() {
            return None;
        }
        let mut lines = vec![
            "<monocode_app>".to_string(),
            "This thread can use some MonoCode app actions through a local CLI without /operator. Run `{cli} --help` for the exact JSON fields. The CLI uses a session credential already in your environment; never print it.".to_string(),
        ];
        if open_sessions {
            let start = if self.review_opened {
                "Each new session waits as an unsent draft until the user reviews and sends it."
            } else {
                "The new session runs its prompt at once. Pass draft:true to leave the prompt unsent for the user."
            };
            lines.push(format!(
                "sessions.list lists this project's sessions. sessions.start opens a new session with a prompt you write. {start} Open a session only when the user asks for one or the work clearly needs a separate agent."
            ));
        }
        if !self.peers.is_empty() {
            let peers = self
                .peers
                .iter()
                .map(|(id, title)| {
                    let title = title.trim();
                    if title.is_empty() {
                        id.clone()
                    } else {
                        format!("\"{}\" ({id})", title.replace('"', "'"))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!(
                "Linked sessions: {peers}. links.read reads a linked session's recent user and assistant messages. links.send sends it a message; a busy session gets the message when its turn ends. Each link carries at most {LINK_MESSAGE_BUDGET} agent messages until the user writes in either session, so send one only when the other agent needs it."
            ));
        }
        lines.push("</monocode_app>".to_string());
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use monocode_core::HarnessId;

    fn session() -> Session {
        Session::blank("s", HarnessId::Codex, "codex:test", "/repo")
    }

    #[test]
    fn explains_only_what_the_thread_may_do() {
        let open = TurnAppAccess::for_session(&session(), false, true, false, Vec::new());
        assert!(open.any());
        let note = open.note().unwrap();
        assert!(note.contains("sessions.start"));
        assert!(note.contains("runs its prompt at once"));
        assert!(!note.contains("links.send"));

        let review = TurnAppAccess::for_session(&session(), false, true, true, Vec::new());
        assert!(review.note().unwrap().contains("unsent draft"));

        let linked = TurnAppAccess::for_session(
            &session(),
            true,
            true,
            false,
            vec![("p1".into(), "API \"work\"".into())],
        );
        let note = linked.note().unwrap();
        assert!(note.contains("Linked sessions: \"API 'work'\" (p1)."));
        assert!(
            !note.contains("sessions.start"),
            "operator already covers it"
        );

        let none = TurnAppAccess::for_session(&session(), false, false, false, Vec::new());
        assert!(!none.any());
        assert_eq!(none.note(), None);
        assert_eq!(
            TurnAppAccess::for_session(&session(), true, false, false, Vec::new()).note(),
            None
        );
    }

    #[test]
    fn orchestration_workers_get_no_access() {
        let mut ask = session();
        ask.orchestration_lead_id = Some("lead".into());
        let access =
            TurnAppAccess::for_session(&ask, false, true, false, vec![("p".into(), "P".into())]);
        assert!(!access.any());
    }
}
