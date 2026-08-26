//! The side-channel queue state — the data half of RealtimeSession's
//! proactive-speech machinery. The engine owns the flush decisions (they need
//! the live speaker / in-flight-response state); this module owns the queue
//! discipline: coalescing, hold mode, and the spoken-facts ledger.

/// Milestones are ambient chatter (droppable, coalesced newest-wins);
/// completions are news the user is owed (never coalesced away).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideChannelKind {
    Milestone,
    Completion,
}

/// `facts` — completed-step texts this item announces. They're logged into
/// `spoken_facts` ONLY when the item is actually sent, so a purged or
/// coalesced-away milestone never counts as "already told the user".
#[derive(Debug, Clone)]
pub struct SideChannelItem {
    pub kind: SideChannelKind,
    pub text: String,
    pub instructions: Option<String>,
    pub facts: Vec<String>,
}

#[derive(Default)]
pub struct SideChannel {
    queue: Vec<SideChannelItem>,
    /// True between asking for a side-channel response and that response
    /// ending — a synchronous guard so we never fire two `response.create`s.
    pub busy: bool,
    /// True while the user asked to hear held updates and the queue drains —
    /// lets the flush run despite hold mode until empty.
    pub releasing_held: bool,
    /// Completed-step texts ACTUALLY spoken since the current agent turn
    /// began. The end-of-turn announcement is told not to repeat them.
    pub spoken_facts: Vec<String>,
}

impl SideChannel {
    /// Stage an item. Returns false when hold mode dropped it (milestones
    /// would be stale by the time the user asks; completions queue up).
    pub fn stage(&mut self, item: SideChannelItem, hold_mode: bool) -> bool {
        if hold_mode && item.kind == SideChannelKind::Milestone {
            return false;
        }
        // A milestone still queued when a completion arrives is history: the
        // completion covers it, and speaking both is the back-to-back
        // "done… and done again". A newer milestone replaces an older one for
        // the same reason. Dropped-unspoken is safe for the wrap-up because
        // facts are only logged at send time.
        self.queue.retain(|i| i.kind != SideChannelKind::Milestone);
        self.queue.push(item);
        true
    }

    /// Pop the next item to speak; the caller logs its facts.
    pub fn pop(&mut self) -> Option<SideChannelItem> {
        if self.queue.is_empty() {
            None
        } else {
            Some(self.queue.remove(0))
        }
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// The pending updates were relayed by other means (the model answered a
    /// status question) — drop them so they don't later replay stale news.
    pub fn clear_queue(&mut self) {
        self.queue.clear();
    }

    /// Full reset for a new transport leg / call.
    pub fn reset(&mut self) {
        self.queue.clear();
        self.busy = false;
        self.releasing_held = false;
        self.spoken_facts.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: SideChannelKind, text: &str) -> SideChannelItem {
        SideChannelItem {
            kind,
            text: text.into(),
            instructions: None,
            facts: Vec::new(),
        }
    }

    #[test]
    fn newer_milestone_replaces_older() {
        let mut sc = SideChannel::default();
        assert!(sc.stage(item(SideChannelKind::Milestone, "step 1"), false));
        assert!(sc.stage(item(SideChannelKind::Milestone, "step 2"), false));
        assert_eq!(sc.len(), 1);
        assert_eq!(sc.pop().unwrap().text, "step 2");
    }

    #[test]
    fn completion_purges_stale_milestones_but_not_other_completions() {
        let mut sc = SideChannel::default();
        sc.stage(item(SideChannelKind::Completion, "task A done"), false);
        sc.stage(item(SideChannelKind::Milestone, "step"), false);
        sc.stage(item(SideChannelKind::Completion, "task B done"), false);
        assert_eq!(sc.len(), 2);
        assert_eq!(sc.pop().unwrap().text, "task A done");
        assert_eq!(sc.pop().unwrap().text, "task B done");
    }

    #[test]
    fn hold_mode_drops_milestones_and_queues_completions() {
        let mut sc = SideChannel::default();
        assert!(!sc.stage(item(SideChannelKind::Milestone, "chatter"), true));
        assert!(sc.stage(item(SideChannelKind::Completion, "news"), true));
        assert_eq!(sc.len(), 1);
    }
}
