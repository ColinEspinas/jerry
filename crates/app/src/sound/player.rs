//! Real audio playback for the sound module (GitHub issue #226) - a dedicated audio thread,
//! spawned lazily the first time [`SoundPlayer::play`] is actually called, never before. A user
//! who leaves `settings.sound.enabled` off (the default) never causes this app to open an audio
//! device at all: [`crate::sound::flow`]'s gating happens before `play` is ever reached.
//!
//! ## Why a dedicated thread
//!
//! `rodio`'s output stream (`rodio::MixerDeviceSink`, from `DeviceSinkBuilder::open_default_sink`)
//! has to stay alive for as long as anything might play through it, and opening it is real,
//! possibly slow device I/O that has no business running on the GPUI foreground thread. One
//! thread, opened once, outliving every individual sound - not one thread per play.
//!
//! ## Failure is silent by design
//!
//! No audio device, a corrupt/missing file, a thread that failed to spawn at all - every one of
//! these is a `log::warn!` and nothing else, never a panic and never surfaced back to the caller.
//! [`SoundPlayer::play`] is a fire-and-forget queue push specifically so a caller (the status-poll
//! tick, a settings-page preview button) never has anything to handle. A device-open failure logs
//! exactly once: the audio thread returns immediately after that warning, so every later `play`
//! call just pushes onto a channel with no receiver left and is silently dropped - not a
//! re-attempted, repeatedly-logged failure on every subsequent transition.

use std::io::Cursor;
use std::sync::mpsc::{self, Sender};
use std::sync::OnceLock;

use super::library::SoundSource;

/// See the module docs. `Send + Sync`: shared as a single field on `AdeApp`, called from GPUI
/// callback closures that themselves run on the foreground thread.
pub struct SoundPlayer {
    /// `None` once initialised means the audio thread failed to spawn at all (an OS-level
    /// failure, not a device failure - a missing device is instead handled *inside* the thread,
    /// see [`audio_thread_main`]) - every `play` after that is a silent no-op.
    sender: OnceLock<Option<Sender<SoundSource>>>,
}

impl SoundPlayer {
    pub fn new() -> Self {
        Self {
            sender: OnceLock::new(),
        }
    }

    /// Queues `source` to play. Never blocks on real audio I/O - decoding and device output both
    /// happen on the dedicated audio thread, spawned here on first use if it doesn't exist yet.
    pub fn play(&self, source: SoundSource) {
        let sender = self.sender.get_or_init(|| spawn_audio_thread().ok());
        if let Some(sender) = sender {
            // A disconnected receiver (the audio thread already exited after a device-open
            // failure) makes this a normal, silent `Err` - not logged again here, since the
            // thread already logged the real reason once before exiting.
            let _ = sender.send(source);
        }
    }
}

impl Default for SoundPlayer {
    fn default() -> Self {
        Self::new()
    }
}

fn spawn_audio_thread() -> std::io::Result<Sender<SoundSource>> {
    let (tx, rx) = mpsc::channel::<SoundSource>();
    std::thread::Builder::new()
        .name("jerry-sound".to_string())
        .spawn(move || audio_thread_main(rx))
        .map(|_join_handle| tx)
}

/// The audio thread's whole body: open the default output device once, then play whatever comes
/// in over the channel until the sender side (the last live [`SoundPlayer`]) is dropped.
fn audio_thread_main(rx: mpsc::Receiver<SoundSource>) {
    let stream_handle = match rodio::DeviceSinkBuilder::open_default_sink() {
        Ok(handle) => handle,
        Err(err) => {
            log::warn!("sound: couldn't open an audio output device, sounds will not play: {err}");
            return;
        }
    };
    let mixer = stream_handle.mixer();

    for source in rx {
        let bytes: Vec<u8> = match &source {
            SoundSource::Builtin(bytes) => bytes.to_vec(),
            SoundSource::File(path) => match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(err) => {
                    log::warn!("sound: couldn't read {}: {err}", path.display());
                    continue;
                }
            },
        };
        match rodio::play(mixer, Cursor::new(bytes)) {
            // `detach()`, not left to drop at the end of this loop iteration: dropping a
            // non-detached `Player` stops it immediately (see `rodio::player::Player`'s own
            // `Drop` impl) - this app wants the sound to finish playing, not to be cut off the
            // instant the next message is (or isn't) received.
            Ok(player) => player.detach(),
            Err(err) => log::warn!("sound: couldn't play a sound: {err}"),
        }
    }
}
