//! Detecting real agent-status transitions to trigger a sound from (GitHub issue #226) - the
//! `AdeApp`-side wiring lives in `crate::root::AdeApp::play_agent_status_sounds` (rides the
//! existing status-poll tick, `crate::rail::render::AdeApp::start_status_polling`), but every
//! decision that doesn't need a live `AdeApp`/`Context` lives here as plain functions so it's
//! testable without GPUI or an audio device.
//!
//! ## Why not `crate::hooks::flow::AdeApp::record_agent_statuses`
//!
//! That function already computes a "did this agent's status really change" bool per agent on
//! this same poll tick, which looks like a ready-made transition detector. It isn't a usable one
//! here: it returns early whenever `agent_status_path` is `None`, and even when it runs, it only
//! considers agents with a *fresh Claude Code hook fact* (`hooks/flow.rs`'s own `recordable`
//! filter) - a Codex agent, or a Claude agent whose hooks haven't fired yet, is invisible to it by
//! construction. A sound module that only ever notices half the agents would be worse than
//! useless, so this reads every agent's live [`crate::rail::status::Status`] independently
//! instead.
//!
//! ## What counts as a transition
//!
//! Three edges matter:
//!
//! - anything -> [`Status::Ask`] triggers [`SoundEventKind::AgentNeedsInput`]
//! - the agent's own [`HookFact::TurnEnded`] fact newly becoming fresh triggers
//!   [`SoundEventKind::AgentFinished`] - see "Why `AgentFinished` needs its own signal" below
//! - anything -> [`Status::Review`] (reached some other way than the fact above, e.g. a plain
//!   process exiting 0 with a real diff) also triggers [`SoundEventKind::AgentFinished`]
//!
//! Deliberately not anything -> [`Status::Idle`]: `Idle` is reached both by a real "turn ended,
//! nothing to review" *and* by the pty-silence heuristic that also produces every other quiescent
//! moment - `crate::hooks::flow`'s own docs already call a status derived from pty silence "a
//! guess". A sound module playing on a guess is worse than one that stays quiet on a real (but
//! diff-less) turn ending - see the issue's own "assumed trade-offs" for this call.
//!
//! An agent seen for the first time (present in the "current" snapshot but absent from
//! "previous") never sounds - see [`crate::root::AdeApp::play_agent_status_sounds`]'s own
//! "seeded" flag for why a *whole app launch* with agents already open must not replay every one
//! of their statuses as a burst of transitions.
//!
//! ## Why `AgentFinished` needs its own signal, not just `-> Status::Review`
//!
//! `Status::Review` (`crate::rail::status::derive_status`) requires three things at once: a real
//! `HookFact::TurnEnded` (or a clean process exit), a non-empty *unreviewed* diff against the
//! agent's own baseline, and - via `crate::review::flow::AdeApp::agent_has_unreviewed_changes` -
//! that this agent is currently the *only* one open in its worktree
//! (`crate::work_surface::agents::Agents::is_sole_agent_in_worktree`). Every window starts with a
//! shell already open in the repo, so spawning a Claude agent into that same worktree makes two
//! agents there - the single-agent gate then never opens for it, and `AgentFinished` could never
//! fire at all. `Status::Ask`, by contrast, is reachable by the plain quiescence heuristic alone,
//! with none of those three conditions - which is why "needs input" sounded correctly while
//! "finished" silently never did.
//!
//! The fix reads a stronger, earlier signal instead: `HookFact::TurnEnded` itself, the moment
//! Claude Code's own `Stop` hook reports it, on its **rising edge** - a fact that stays fresh
//! across several ticks (`crate::hooks::event::HOOK_SIGNAL_TTL`) never re-sounds. This is the
//! agent stating outright "my turn just ended", independent of whether there happens to be a
//! reviewable diff or a second agent sharing its worktree. It is also gated on
//! [`crate::work_surface::agents::ProcessKind::is_agent_session`], the same guard
//! `crate::rail::status::derive_status` applies to every hook-derived fact, so a plain shell
//! (which never receives hooks) can never trigger it. A Codex agent has no hook channel at all,
//! so for it `AgentFinished` still only ever arrives via the older `-> Status::Review` path.
//!
//! A falling edge (the fact expiring after 30 minutes with nothing else happening) is explicitly
//! *not* a transition worth sounding for - see [`sound_for_transition`]'s own doc comment.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::Context;

use crate::hooks::event::HookFact;
use crate::rail::status::Status;
use crate::root::AdeApp;
use crate::sound::SoundEventKind;
use crate::work_surface::agents::AgentId;

/// The minimum real time between two sounds this module plays on its own (agent-status
/// transitions) - not applied to a settings-page preview button, which is an explicit user
/// action. Several agents finishing within the same status-poll tick (or in quick succession)
/// produce one sound, not a burst - see [`pick_one_event`].
pub(crate) const SOUND_COOLDOWN: Duration = Duration::from_secs(3);

/// One agent's sound-relevant state as of a single status-poll tick - [`Status`] plus whether a
/// fresh `HookFact::TurnEnded` is currently reported for it. Two fields rather than folding
/// `turn_ended` into a sixth [`Status`] variant: the two are genuinely independent (an agent can
/// be freshly turn-ended *and* sit in [`Status::Idle`] with no reviewable diff, or *and* sit in
/// [`Status::Review`] with one), and [`Status`] is a wider, non-sound-specific vocabulary that
/// this module shouldn't be the one to extend - see the module docs' "Why `AgentFinished` needs
/// its own signal" section for why both are read at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentSoundState {
    pub status: Status,
    pub turn_ended: bool,
}

/// Which [`SoundEventKind`] (if any) a single agent's state change should trigger. `None` when
/// `prev == next` (nothing changed) or the new state isn't one of the cases the module docs list.
pub(crate) fn sound_for_transition(
    prev: AgentSoundState,
    next: AgentSoundState,
) -> Option<SoundEventKind> {
    // The turn-ended fact's rising edge outranks everything else below: it is checked first, and
    // returns unconditionally, whether or not `status` also changed on this same tick.
    if !prev.turn_ended && next.turn_ended {
        return Some(SoundEventKind::AgentFinished);
    }
    // The fact merely expiring (falling edge, no new fact arrived) is not itself an event - see
    // the module docs. Any `status` change riding along on the very same tick this expires is
    // just the quiescence heuristic resuming, not a transition worth sounding for, so this
    // returns rather than falling through to the `status` match below.
    if prev.turn_ended && !next.turn_ended {
        return None;
    }
    if prev.status == next.status {
        return None;
    }
    match next.status {
        Status::Ask => Some(SoundEventKind::AgentNeedsInput),
        Status::Review => Some(SoundEventKind::AgentFinished),
        _ => None,
    }
}

/// Higher priority wins when more than one real transition happens on the same tick -
/// `AgentNeedsInput` over `AgentFinished`, matching `Status::urgency_rank`'s own "needs input is
/// more urgent than review-ready" ordering.
fn priority(event: SoundEventKind) -> u8 {
    match event {
        SoundEventKind::AgentNeedsInput => 0,
        SoundEventKind::AgentFinished => 1,
        SoundEventKind::AppStart => 2,
    }
}

/// Scans every agent in `current` against its remembered status in `prev` and returns the single
/// highest-priority [`SoundEventKind`] any real transition produced, or `None` if nothing did.
/// Deterministic regardless of `HashMap` iteration order (picks by [`priority`], not by "first
/// seen"). An agent present only in `current` (just appeared) or only in `prev` (closed since the
/// last tick) never contributes - only agents present in both, whose status actually changed,
/// count as a real transition.
pub(crate) fn agents_sound_event(
    current: &HashMap<AgentId, AgentSoundState>,
    prev: &HashMap<AgentId, AgentSoundState>,
) -> Option<SoundEventKind> {
    current
        .iter()
        .filter_map(|(agent_id, &next)| {
            let &prev_state = prev.get(agent_id)?;
            sound_for_transition(prev_state, next)
        })
        .min_by_key(|event| priority(*event))
}

impl AdeApp {
    /// The `AdeApp`-side half of GitHub issue #226's agent sounds - called once per status-poll
    /// tick, right alongside `crate::hooks::flow::AdeApp::record_agent_statuses`
    /// (`crate::rail::render::AdeApp::start_status_polling`). See the module docs for why this
    /// reads every agent's live status itself rather than reusing that function's own "changed"
    /// signal, and for exactly which transitions count.
    pub(crate) fn play_agent_status_sounds(&mut self, cx: &mut Context<Self>) {
        let current_statuses: HashMap<AgentId, AgentSoundState> = self
            .agents
            .iter()
            .map(|agent| {
                let status = self.agent_status(agent, cx);
                // Mirrors `crate::work_surface::render::AdeApp::agent_status`'s own
                // `hook_runtime`/`fresh()` read - a shell never has a hook fact to begin with,
                // but `is_agent_session()` is checked anyway, matching
                // `crate::rail::status::derive_status`'s identical guard on every other
                // hook-derived fact.
                let turn_ended = agent.kind.is_agent_session()
                    && self.hook_runtime.as_ref().is_some_and(|runtime| {
                        runtime.signal_for(agent.id).fresh() == Some(HookFact::TurnEnded)
                    });
                (agent.id, AgentSoundState { status, turn_ended })
            })
            .collect();

        // First tick since this window opened: every already-open agent's state is real, but
        // none of it is a *transition* - there is nothing to compare against yet. Seed
        // `prev_agent_sound_states` and return without ever consulting `agents_sound_event`, so a
        // window that opens onto three agents already sitting in `Ask` (or already carrying a
        // fresh `TurnEnded` fact) never plays a burst of sounds it had no part in causing.
        if !self.agent_sound_seeded {
            self.agent_sound_seeded = true;
            self.prev_agent_sound_states = current_statuses;
            return;
        }

        let event = agents_sound_event(&current_statuses, &self.prev_agent_sound_states);
        self.prev_agent_sound_states = current_statuses;

        let Some(event) = event else {
            return;
        };
        if !self.agent_sound_gate_open(event) {
            return;
        }

        let now = Instant::now();
        if let Some(last) = self.last_sound_at {
            if now.duration_since(last) < SOUND_COOLDOWN {
                return;
            }
        }
        self.last_sound_at = Some(now);
        self.play_event_sound(event);
    }

    /// Every real condition an agent-transition sound must clear, besides the transition itself
    /// having happened at all and the cooldown (checked separately in
    /// [`Self::play_agent_status_sounds`], since it needs a real clock read `cx`-free unit tests
    /// don't have to care about). Never applied to [`Self::preview_sound`] (an explicit user
    /// click) or to the app-start sound's own narrower master+event-only check
    /// (`Self::maybe_play_app_start_sound`) - window focus and "is this specifically an *agent*
    /// sound" only make sense for this path.
    fn agent_sound_gate_open(&self, event: SoundEventKind) -> bool {
        if !self.settings.sound.enabled || self.window_active {
            return false;
        }
        self.sound_event_toggle(event)
    }

    /// Just the per-[`SoundEventKind`] toggle
    /// (`crate::settings::store::SoundEventSettings::enabled`) - the master switch is checked
    /// separately by each caller, since [`Self::maybe_play_app_start_sound`] and
    /// [`Self::agent_sound_gate_open`] both need it but nothing else in this shared helper does.
    fn sound_event_toggle(&self, event: SoundEventKind) -> bool {
        match event {
            SoundEventKind::AppStart => self.settings.sound.app_start.enabled,
            SoundEventKind::AgentFinished => self.settings.sound.agent_finished.enabled,
            SoundEventKind::AgentNeedsInput => self.settings.sound.agent_needs_input.enabled,
        }
    }

    /// The app-start sound's own gate: master switch + its own toggle, nothing else - there is no
    /// "window already focused" case at startup worth suppressing it for (the window is always
    /// focused the instant it opens) and no transition/cooldown bookkeeping either. Callers are
    /// still responsible for the *per-process* "only once, ever" gate
    /// (`crate::sound::claim_app_start_sound`) - this only decides whether the user actually
    /// wants this sound at all.
    pub(crate) fn maybe_play_app_start_sound(&self) {
        if !self.settings.sound.enabled || !self.sound_event_toggle(SoundEventKind::AppStart) {
            return;
        }
        self.play_event_sound(SoundEventKind::AppStart);
    }

    /// Resolves `event`'s *configured* sound id against the live library and queues it - the raw,
    /// ungated primitive both [`Self::play_agent_status_sounds`] and
    /// [`Self::maybe_play_app_start_sound`] call once their own gating already passed. A
    /// since-deleted/unresolvable id falls back to the event's own built-in default rather than
    /// silently playing nothing - see `crate::sound::library::resolve`'s own docs; nothing here
    /// ever rewrites `settings.toml` to "heal" a missing id.
    fn play_event_sound(&self, event: SoundEventKind) {
        let configured_id = match event {
            SoundEventKind::AppStart => self.settings.sound.app_start.sound.as_str(),
            SoundEventKind::AgentFinished => self.settings.sound.agent_finished.sound.as_str(),
            SoundEventKind::AgentNeedsInput => self.settings.sound.agent_needs_input.sound.as_str(),
        };
        self.preview_sound(configured_id, Some(event));
    }

    /// The one real place anything in this app queues a sound to actually play - every other
    /// method in this `impl` block funnels into this. Explicit-user-action previews (a settings
    /// page's ▶ button) call this directly with `event: None`; `event: Some(_)` callers get a
    /// fallback to that event's own built-in default when `sound_id` no longer resolves.
    pub(crate) fn preview_sound(&self, sound_id: &str, event: Option<SoundEventKind>) {
        let resolved =
            crate::sound::library::resolve(sound_id, &self.sound_library).or_else(|| {
                event.and_then(|event| {
                    crate::sound::library::resolve(event.default_sound_id(), &self.sound_library)
                })
            });
        let Some(sound) = resolved else {
            return;
        };
        self.sound_player.play(sound.source.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an [`AgentSoundState`] with `turn_ended: false` - what every pre-hook-signal test
    /// below means, and the state most agents are in most of the time.
    fn state(status: Status) -> AgentSoundState {
        AgentSoundState {
            status,
            turn_ended: false,
        }
    }

    /// Builds an [`AgentSoundState`] with a fresh `turn_ended` fact, for the tests exercising the
    /// module docs' "Why `AgentFinished` needs its own signal" edge.
    fn turn_ended_state(status: Status) -> AgentSoundState {
        AgentSoundState {
            status,
            turn_ended: true,
        }
    }

    #[test]
    fn same_status_is_never_a_transition() {
        for status in Status::ORDER {
            assert_eq!(sound_for_transition(state(status), state(status)), None);
        }
    }

    #[test]
    fn transitioning_into_ask_from_anything_else_triggers_needs_input() {
        for prev in Status::ORDER {
            if prev == Status::Ask {
                continue;
            }
            assert_eq!(
                sound_for_transition(state(prev), state(Status::Ask)),
                Some(SoundEventKind::AgentNeedsInput),
                "prev = {prev:?}"
            );
        }
    }

    #[test]
    fn transitioning_into_review_from_anything_else_triggers_agent_finished() {
        for prev in Status::ORDER {
            if prev == Status::Review {
                continue;
            }
            assert_eq!(
                sound_for_transition(state(prev), state(Status::Review)),
                Some(SoundEventKind::AgentFinished),
                "prev = {prev:?}"
            );
        }
    }

    #[test]
    fn transitioning_into_run_fail_or_idle_never_triggers_a_sound() {
        let silent_targets = [Status::Run, Status::Fail, Status::Idle];
        for prev in Status::ORDER {
            for &next in &silent_targets {
                if prev == next {
                    continue;
                }
                assert_eq!(
                    sound_for_transition(state(prev), state(next)),
                    None,
                    "prev = {prev:?}, next = {next:?}"
                );
            }
        }
    }

    /// The module docs' central fix: a fresh `TurnEnded` fact triggers `AgentFinished` on its own
    /// rising edge, even while `status` itself stays put at `Idle` - the exact real-world case
    /// (a Claude agent sharing its worktree with a shell, so `Status::Review`'s single-agent gate
    /// never opens) that used to leave this event permanently silent.
    #[test]
    fn a_fresh_turn_ended_fact_triggers_agent_finished_even_with_no_status_change() {
        assert_eq!(
            sound_for_transition(state(Status::Idle), turn_ended_state(Status::Idle)),
            Some(SoundEventKind::AgentFinished)
        );
    }

    /// The same rising edge also outranks whatever `status` did on the same tick - even a
    /// coincident move into `Ask` doesn't shadow it, since the fact is checked first and returns
    /// unconditionally.
    #[test]
    fn a_fresh_turn_ended_fact_wins_even_alongside_a_move_into_ask() {
        assert_eq!(
            sound_for_transition(state(Status::Run), turn_ended_state(Status::Ask)),
            Some(SoundEventKind::AgentFinished)
        );
    }

    #[test]
    fn turn_ended_staying_true_across_ticks_does_not_re_sound() {
        assert_eq!(
            sound_for_transition(turn_ended_state(Status::Idle), turn_ended_state(Status::Idle)),
            None
        );
    }

    /// The fact's falling edge (it merely expired, `HOOK_SIGNAL_TTL` elapsed with no new fact) is
    /// not itself an event, even if `status` also moved into `Ask` on the same tick as the
    /// quiescence heuristic resumes - see the module docs' "A falling edge ... is explicitly not
    /// a transition" note.
    #[test]
    fn turn_ended_expiring_never_sounds_even_alongside_a_move_into_ask() {
        assert_eq!(
            sound_for_transition(turn_ended_state(Status::Idle), state(Status::Ask)),
            None
        );
    }

    fn agent(id: u64) -> AgentId {
        id
    }

    #[test]
    fn no_agents_produces_no_sound() {
        let current = HashMap::new();
        let prev = HashMap::new();
        assert_eq!(agents_sound_event(&current, &prev), None);
    }

    #[test]
    fn a_brand_new_agent_never_sounds_even_if_it_starts_in_ask() {
        let mut current = HashMap::new();
        current.insert(agent(1), state(Status::Ask));
        let prev = HashMap::new();
        assert_eq!(agents_sound_event(&current, &prev), None);
    }

    #[test]
    fn a_real_transition_to_ask_sounds() {
        let mut current = HashMap::new();
        current.insert(agent(1), state(Status::Ask));
        let mut prev = HashMap::new();
        prev.insert(agent(1), state(Status::Run));
        assert_eq!(
            agents_sound_event(&current, &prev),
            Some(SoundEventKind::AgentNeedsInput)
        );
    }

    #[test]
    fn several_agents_finishing_at_once_produce_a_single_sound() {
        let mut current = HashMap::new();
        let mut prev = HashMap::new();
        for id in 1..=5u64 {
            current.insert(agent(id), state(Status::Review));
            prev.insert(agent(id), state(Status::Run));
        }
        // A single `Option`, never a collection - the caller can only ever play one sound per
        // tick regardless of how many agents transitioned.
        assert_eq!(
            agents_sound_event(&current, &prev),
            Some(SoundEventKind::AgentFinished)
        );
    }

    #[test]
    fn needs_input_wins_over_finished_when_both_happen_on_the_same_tick() {
        let mut current = HashMap::new();
        let mut prev = HashMap::new();
        current.insert(agent(1), state(Status::Review));
        prev.insert(agent(1), state(Status::Run));
        current.insert(agent(2), state(Status::Ask));
        prev.insert(agent(2), state(Status::Run));
        assert_eq!(
            agents_sound_event(&current, &prev),
            Some(SoundEventKind::AgentNeedsInput)
        );
    }

    #[test]
    fn an_agent_that_closed_since_the_last_tick_is_ignored() {
        let current = HashMap::new();
        let mut prev = HashMap::new();
        prev.insert(agent(1), state(Status::Run));
        assert_eq!(agents_sound_event(&current, &prev), None);
    }

    #[test]
    fn staying_in_ask_across_ticks_does_not_re_sound() {
        let mut current = HashMap::new();
        current.insert(agent(1), state(Status::Ask));
        let mut prev = HashMap::new();
        prev.insert(agent(1), state(Status::Ask));
        assert_eq!(agents_sound_event(&current, &prev), None);
    }
}

/// Real, `AdeApp`-integrated coverage - a real window, a real spawned shell agent, real settings.
/// [`agents_sound_event`]/[`sound_for_transition`] above are already exhaustively covered as pure
/// functions with no GPUI involved; this module instead covers the two things that genuinely need
/// a live `AdeApp` to mean anything: the gating predicate reading real `self.settings`/
/// `self.window_active`, and [`AdeApp::play_agent_status_sounds`]'s own seeding/no-false-positive
/// behaviour against a real (not fabricated) agent. None of these ever open a real audio device:
/// every scenario here either never reaches [`AdeApp::preview_sound`] at all, or does so with
/// `settings.sound.enabled` on top of `crate::sound::player::SoundPlayer`'s own documented
/// "device-open failure is a single silent `log::warn!`, never a panic" contract - see that
/// module's docs.
#[cfg(test)]
mod adeapp_tests {
    use super::*;
    use crate::root::focus::palette_focus_tests::open_test_app;
    use gpui::TestAppContext;

    #[gpui::test]
    fn a_focused_window_gates_every_agent_sound_closed(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.settings.sound.enabled = true;
            app.window_active = true;
            for event in SoundEventKind::ALL {
                assert!(
                    !app.agent_sound_gate_open(event),
                    "{event:?} must be gated closed while the window has focus"
                );
            }
        });
    }

    #[gpui::test]
    fn the_master_switch_off_gates_every_agent_sound_closed(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.settings.sound.enabled = false;
            app.window_active = false;
            for event in SoundEventKind::ALL {
                assert!(
                    !app.agent_sound_gate_open(event),
                    "{event:?} must be gated closed while the master switch is off"
                );
            }
        });
    }

    #[gpui::test]
    fn a_single_disabled_event_stays_closed_without_affecting_its_siblings(
        cx: &mut TestAppContext,
    ) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.settings.sound.enabled = true;
            app.window_active = false;
            app.settings.sound.agent_finished.enabled = false;

            assert!(!app.agent_sound_gate_open(SoundEventKind::AgentFinished));
            assert!(app.agent_sound_gate_open(SoundEventKind::AgentNeedsInput));
        });
    }

    #[gpui::test]
    fn every_gate_open_when_enabled_and_unfocused(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, _cx| {
            app.settings.sound.enabled = true;
            app.window_active = false;
            for event in SoundEventKind::ALL {
                assert!(
                    app.agent_sound_gate_open(event),
                    "{event:?} must be open: master on, its own toggle on (the real default), \
                     window unfocused"
                );
            }
        });
    }

    /// The very first status-poll tick after a window opens must never play a sound, no matter
    /// what state its one already-open agent (the real startup shell,
    /// `AdeApp::new_with_settings`'s own "a fresh window starts with one shell" - see that
    /// method's docs) happens to be in - see the module docs' "An agent seen for the first time"
    /// section.
    #[gpui::test]
    fn the_first_tick_seeds_without_ever_setting_last_sound_at(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, cx| {
            app.settings.sound.enabled = true;
            app.window_active = false;
            assert!(
                !app.agent_sound_seeded,
                "premise: nothing has seeded prev_agent_sound_states yet"
            );

            app.play_agent_status_sounds(cx);

            assert!(app.agent_sound_seeded);
            assert!(
                app.last_sound_at.is_none(),
                "the seeding tick must never itself count as a transition"
            );
            assert!(
                !app.prev_agent_sound_states.is_empty(),
                "the real startup shell agent must have been recorded"
            );
        });
    }

    /// Two ticks in a row, with nothing about the real agent having changed in between, must
    /// never produce a sound - there is no real transition to detect, only the same status
    /// re-observed.
    #[gpui::test]
    fn two_ticks_with_no_real_change_never_set_last_sound_at(cx: &mut TestAppContext) {
        let repo = tempfile::tempdir().expect("tempdir");
        let (app, cx) = open_test_app(cx, repo.path().to_path_buf());
        app.update(cx, |app, cx| {
            app.settings.sound.enabled = true;
            app.window_active = false;
            app.play_agent_status_sounds(cx); // seeds
            app.play_agent_status_sounds(cx); // real second observation
            assert!(app.last_sound_at.is_none());
        });
    }
}
