//! Sound design (GitHub issue #226): a real, disableable sound module rather than a fixed
//! file-per-event mapping. Off by default end to end - `crate::settings::store::SoundSettings`'s
//! own `Default` sets the master switch `enabled = false`, and nothing in this module ever opens
//! an audio device until [`crate::sound::player::SoundPlayer`] is asked to play something for
//! real.
//!
//! ## How the pieces fit
//!
//! - [`library`] - the sound library itself: built-in sounds embedded at compile time
//!   ([`library::builtin_sounds`]), plus whatever the user has imported into
//!   `~/.config/jerry/sounds/` ([`library::load_user_sounds_from_dir`]). Every
//!   [`SoundEventKind`] resolves to one [`library::LibrarySound`] by id
//!   ([`library::resolve`]).
//! - [`player`] - the real playback side: a lazily-spawned dedicated audio thread
//!   (`crate::sound::player::SoundPlayer`), never started at all if the module stays off.
//! - `flow` (`pub(crate)`) - the `AdeApp`-side wiring: detecting real agent-status transitions on
//!   the existing status-poll tick and deciding, given the live settings and window-focus state,
//!   whether a sound should actually play right now.
//!
//! Three events, chosen because they're the three moments GitHub issue #226 asks for and each has
//! a real, already-derived signal to trigger from - see [`SoundEventKind`].

pub(crate) mod flow;
pub mod library;
pub mod player;

pub use library::{LibrarySound, SoundFileError, SoundSource};

use std::sync::atomic::{AtomicBool, Ordering};

/// True the *first* time this is called in the process, `false` every time after -
/// `crate::root::state::AdeApp::new_with_settings`'s "the app-start sound plays once per
/// process, not once per window" gate, so opening a second window (`File > New Window`,
/// `crate::title_bar::menu`) never replays it. A plain process-global static, not a per-`AdeApp`
/// field: the whole point is that it survives across separate `AdeApp` constructions in the same
/// process, which a struct field by definition cannot.
pub fn claim_app_start_sound() -> bool {
    claim_once(&APP_START_SOUND_PLAYED)
}

static APP_START_SOUND_PLAYED: AtomicBool = AtomicBool::new(false);

/// The real compare-and-swap [`claim_app_start_sound`] wraps - split out so it has a fresh,
/// non-global [`AtomicBool`] to test against; the process-global static itself can't be reset
/// between test runs sharing the same test binary.
fn claim_once(flag: &AtomicBool) -> bool {
    flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// The three lifecycle moments this app can attach a sound to. `Hash`/`Eq` so this can key a
/// dropdown-open-state map; `Copy` because every use of it is a cheap tag, never owned data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SoundEventKind {
    /// Jerry has just launched - see `crate::root::state`'s startup wiring.
    AppStart,
    /// An agent's turn ended with real changes ready to review (`rail::status::Status::Review`) -
    /// see [`flow`]'s own docs for why this, and not every path back to `Status::Idle`, is what
    /// "finished" means here.
    AgentFinished,
    /// An agent moved into `rail::status::Status::Ask`.
    AgentNeedsInput,
}

impl SoundEventKind {
    pub const ALL: [SoundEventKind; 3] = [
        SoundEventKind::AppStart,
        SoundEventKind::AgentFinished,
        SoundEventKind::AgentNeedsInput,
    ];

    /// The `settings.toml` table name this event's toggle/sound-choice live under
    /// (`crate::settings::store::SoundSettings`) - `[sound.app_start]` etc.
    pub fn settings_key(self) -> &'static str {
        match self {
            SoundEventKind::AppStart => "app_start",
            SoundEventKind::AgentFinished => "agent_finished",
            SoundEventKind::AgentNeedsInput => "agent_needs_input",
        }
    }

    /// The settings-page row label.
    pub fn label(self) -> &'static str {
        match self {
            SoundEventKind::AppStart => "App start",
            SoundEventKind::AgentFinished => "Agent finished",
            SoundEventKind::AgentNeedsInput => "Agent needs input",
        }
    }

    /// The one-line description under this event's settings row.
    pub fn description(self) -> &'static str {
        match self {
            SoundEventKind::AppStart => "Play a sound when Jerry launches.",
            SoundEventKind::AgentFinished => {
                "Play a sound when an agent finishes a turn with changes ready to review."
            }
            SoundEventKind::AgentNeedsInput => {
                "Play a sound when an agent is waiting for your response."
            }
        }
    }

    /// The built-in [`library::LibrarySound`] id this event points to before the user ever
    /// touches the settings page - each event gets a distinct built-in sound, matching
    /// `crate::settings::store::SoundSettings`'s own hand-written `Default` impl.
    pub fn default_sound_id(self) -> &'static str {
        match self {
            SoundEventKind::AppStart => "soft-chime",
            SoundEventKind::AgentFinished => "marimba-pop",
            SoundEventKind::AgentNeedsInput => "gentle-ping",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_events_default_sound_id_resolves_in_the_builtin_library() {
        let builtins = library::builtin_sounds();
        for event in SoundEventKind::ALL {
            assert!(
                library::resolve(event.default_sound_id(), &builtins).is_some(),
                "{:?}'s default_sound_id {:?} must be a real built-in sound",
                event,
                event.default_sound_id()
            );
        }
    }

    #[test]
    fn claim_once_is_true_exactly_the_first_time() {
        let flag = AtomicBool::new(false);
        assert!(claim_once(&flag));
        assert!(!claim_once(&flag));
        assert!(!claim_once(&flag));
    }

    #[test]
    fn every_event_has_a_distinct_default_sound() {
        let ids: std::collections::HashSet<&str> = SoundEventKind::ALL
            .iter()
            .map(|event| event.default_sound_id())
            .collect();
        assert_eq!(ids.len(), SoundEventKind::ALL.len());
    }
}
