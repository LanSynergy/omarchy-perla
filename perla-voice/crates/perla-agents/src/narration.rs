//! Detail-mode narration controller — direct port of `Narration.swift`.
//!
//! Fed LIVE digests of the agent's transcript while a turn is in flight, it
//! decides when the agent has hit a moment worth saying out loud and stages
//! ONE short side-channel utterance. The engine queues it so it's spoken the
//! instant Perla is idle.
//!
//! Two signals:
//! - Milestone — forward progress on the to-do list: a step just completed,
//!   or a new step became active. Diffed against a running baseline so we
//!   only ever speak on NEW progress and never re-announce.
//! - Heartbeat — for tiny tasks with no to-do list, a rare "still working…"
//!   so detail mode never feels dead. Time-gated.
//!
//! The end-of-turn "done" line is handled by the turn-finished path, so this
//! stays silent once the turn completes.

use std::collections::HashSet;

use crate::digest::AgentDigest;

/// A staged utterance. `facts` are the completed-step texts it announces —
/// they ride along so the engine can log them as "already told the user"
/// ONLY when the milestone is actually spoken (a staged milestone can still
/// be purged unspoken, and logging at stage time made the end-of-turn summary
/// suppress news the user never heard).
#[derive(Debug, Clone)]
pub struct Utterance {
    pub text: String,
    pub instructions: String,
    pub facts: Vec<String>,
}

#[derive(Default)]
pub struct Narration {
    pending: Option<Utterance>,
    announced_completed: HashSet<String>,
    last_in_progress: Option<String>,
    last_heartbeat_elapsed: f64,
}

impl Narration {
    pub fn new() -> Self {
        Self {
            last_heartbeat_elapsed: -1000.0,
            ..Default::default()
        }
    }

    /// Call at the START of every turn. The baselines are keyed by to-do
    /// TEXT, so carrying them across turns silently swallows a later turn's
    /// "run the tests" as already-announced.
    pub fn reset(&mut self) {
        self.pending = None;
        self.announced_completed.clear();
        self.last_in_progress = None;
        self.last_heartbeat_elapsed = -1000.0;
    }

    /// Feed a fresh snapshot + seconds since the turn began. Returns true if
    /// a new utterance was staged.
    pub fn ingest(
        &mut self,
        digest: &AgentDigest,
        elapsed_secs: f64,
        enabled: bool,
        big_moments_only: bool,
    ) -> bool {
        if !enabled || digest.turn_complete {
            return false;
        }
        if digest.todos.is_empty() {
            self.heartbeat(digest, elapsed_secs, big_moments_only)
        } else {
            self.milestone(digest, big_moments_only)
        }
    }

    fn milestone(&mut self, digest: &AgentDigest, big_moments_only: bool) -> bool {
        let completed_now: HashSet<String> = digest
            .todos
            .iter()
            .filter(|t| t.status == "completed")
            .map(|t| t.text.clone())
            .collect();
        let in_progress_now = digest
            .todos
            .iter()
            .find(|t| t.status == "in_progress")
            .map(|t| t.text.clone());

        let newly_completed: Vec<String> = {
            let mut v: Vec<String> = completed_now
                .difference(&self.announced_completed)
                .cloned()
                .collect();
            v.sort();
            v
        };
        let in_progress_changed =
            in_progress_now.is_some() && in_progress_now != self.last_in_progress;

        // Always advance the baseline so we never re-announce, even for
        // changes a calmer verbosity chooses to skip.
        let advance = |s: &mut Self| {
            s.announced_completed.extend(completed_now.iter().cloned());
            s.last_in_progress = in_progress_now.clone();
        };

        // Every box ticked: the agent is seconds from ending its turn and the
        // completion announcement will state the outcome — a milestone here
        // is the "it's done… and now it's done again" double-announcement.
        if !digest.todos.is_empty() && completed_now.len() == digest.todos.len() {
            advance(self);
            return false;
        }

        let speak_next = in_progress_changed && !big_moments_only;
        if newly_completed.is_empty() && !speak_next {
            advance(self);
            return false;
        }

        let mut facts_text: Vec<String> = Vec::new();
        if !newly_completed.is_empty() {
            facts_text.push(format!("just finished — {}", newly_completed.join("; ")));
        }
        if speak_next {
            if let Some(ip) = &in_progress_now {
                facts_text.push(format!("now working on — {ip}"));
            }
        }
        advance(self);
        if facts_text.is_empty() {
            return false;
        }

        let done = completed_now.len();
        let total = digest.todos.len();
        self.pending = Some(Utterance {
            text: format!(
                "[live agent status] {} ({done} of {total} steps done)",
                facts_text.join("; ")
            ),
            instructions: "Give the user this live progress update in ONE short, casual spoken sentence. Don't read it verbatim or list every step — just the gist of what it finished and what's next.".into(),
            // Completed steps only — an "in progress" line isn't a fact the
            // completion could wrongly repeat.
            facts: newly_completed,
        });
        true
    }

    fn heartbeat(&mut self, digest: &AgentDigest, elapsed: f64, big_moments_only: bool) -> bool {
        // Only once the task has clearly settled into work, and only if it's
        // actually doing something. Tiny tasks that finish fast never trip this.
        if digest.recent_actions.is_empty() {
            return false;
        }
        let gap = if big_moments_only { 45.0 } else { 25.0 };
        if elapsed < 12.0 || (elapsed - self.last_heartbeat_elapsed) < gap {
            return false;
        }
        self.last_heartbeat_elapsed = elapsed;

        let last = digest.recent_actions.last().cloned().unwrap_or_default();
        self.pending = Some(Utterance {
            text: format!("[live agent status] still working — last action: {last}"),
            instructions: "In ONE short, casual spoken sentence, reassure the user it's still working. Mention the last thing it did only if it sounds natural.".into(),
            facts: Vec::new(),
        });
        true
    }

    pub fn drain(&mut self) -> Option<Utterance> {
        self.pending.take()
    }
}
