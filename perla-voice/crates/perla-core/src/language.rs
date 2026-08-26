//! Pins a call to the language the user actually speaks — port of
//! `VoiceLanguageLock.swift`.
//!
//! Why this exists: side-channel announcements go out as `response.create`
//! with their own `instructions`, which REPLACE the session instructions for
//! that response — so the language rule must ride along on every one, or the
//! model answers an Arabic speaker in Arabic and then announces agent
//! progress in English (the internal system notes are always English).
//!
//! v1 keeps only the explicit user pin plus the neutral clause; automatic
//! detection (the Swift version used NLLanguageRecognizer and was dormant
//! anyway because input transcription is off) can plug into `observe`.

pub struct LanguageLock {
    /// Explicit user setting: an English language name ("Arabic", "English").
    /// Strongest signal — survives reset, produces a never-switch clause.
    user_pinned: Option<String>,
    /// Detected from transcripts, when a detector is wired in.
    detected: Option<String>,
}

impl LanguageLock {
    pub fn new() -> Self {
        Self {
            user_pinned: None,
            detected: None,
        }
    }

    /// Apply the user's "voice language" setting. Accepts a plain English
    /// language name or a BCP-47-ish code we can map. None = auto.
    pub fn pin_user(&mut self, language: Option<&str>) {
        self.user_pinned = language.map(display_name);
    }

    /// Clears detection state for a new call. The user pin survives.
    pub fn reset(&mut self) {
        self.detected = None;
    }

    /// Feed a spoken user utterance when a detector is available. Returns
    /// true when the lock changed and the session instructions should be
    /// re-pinned. (No detector in v1 — always false unless a host sets
    /// `detected` through `set_detected`.)
    pub fn observe(&mut self, _text: &str) -> bool {
        false
    }

    /// Host-provided detection result (e.g. from an embedder's own STT).
    pub fn set_detected(&mut self, language: Option<String>) -> bool {
        if self.user_pinned.is_some() || self.detected == language {
            return false;
        }
        self.detected = language;
        true
    }

    /// The clause appended to the session instructions AND to every
    /// side-channel `response.create`.
    pub fn clause(&self) -> String {
        let tail = "Judge the language ONLY by the words the user says — never by their accent, their name, or their location. Perla's internal system notes and tool results are always written in English; that is never a reason to switch languages.";
        if let Some(name) = &self.user_pinned {
            return format!(
                "LANGUAGE (explicit user setting): Speak {name} and ONLY {name}, for every spoken reply in this entire call — answers, progress updates, announcements, questions. This is a hard setting the user chose. Never switch languages for any reason: not their accent, not the language they appear to speak, not the language of system notes or tool results. All of Perla's internal system notes and tool results are written in English; that is an internal detail, never a cue to switch."
            );
        }
        if let Some(name) = &self.detected {
            return format!(
                "LANGUAGE LOCK: The user speaks {name}. Every spoken reply for the rest of this call — answers, progress updates, announcements, questions — must be in {name}. {tail} Only switch if the user clearly switches and stays there."
            );
        }
        format!("LANGUAGE: Speak the same language the user has been speaking in this conversation. {tail}")
    }
}

impl Default for LanguageLock {
    fn default() -> Self {
        Self::new()
    }
}

/// Map common codes to English names; pass through anything else verbatim.
fn display_name(code: &str) -> String {
    match code.to_ascii_lowercase().as_str() {
        "en" => "English".into(),
        "ar" => "Arabic".into(),
        "es" => "Spanish".into(),
        "fr" => "French".into(),
        "de" => "German".into(),
        "it" => "Italian".into(),
        "pt" => "Portuguese".into(),
        "ja" => "Japanese".into(),
        "ko" => "Korean".into(),
        "zh" => "Chinese".into(),
        "hi" => "Hindi".into(),
        "ru" => "Russian".into(),
        "tr" => "Turkish".into(),
        "ur" => "Urdu".into(),
        other => {
            // Already a name ("Arabic") or an unknown code — capitalize best-effort.
            let mut c = other.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        }
    }
}
