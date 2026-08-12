//! The sound library (GitHub issue #226) - built-in sounds plus anything the user has imported
//! into `~/.config/jerry/sounds/`, together forming the one list every sound-event dropdown on
//! the Notifications settings page picks from. Deliberately the same shape
//! `crate::settings::custom_theme` already uses for custom themes: a directory derived from the
//! settings file's own location, files loaded from disk with a size cap and per-file errors
//! reported rather than aborting the whole load, and an importer that validates before it ever
//! writes anything.
//!
//! ## What's different from themes
//!
//! A theme file is validated by parsing it as TOML and checking every key against
//! `crate::theme::TOKEN_GROUPS`. A sound file has no such schema - the only real validation
//! available is decoding it, so [`import_sound_file`] does that for real (`rodio::Decoder::new`)
//! before ever copying the file into the library. A file that looks like a `.wav` by extension
//! but isn't real audio is rejected at import time, never added to fail silently the first time
//! something tries to play it.
//!
//! A sound also has no internal `name` field the way a theme file does (`name = "Midnight
//! Coral"` inside the TOML) - its identity is its filename stem. That's what [`LibrarySound::id`]
//! is, for both built-in and imported sounds alike, so a user importing a file also named
//! `soft-chime.mp3` collides with the built-in `soft-chime` sound exactly the way importing a
//! theme named `"Jerry Dark"` collides with that built-in theme - reported, not silently
//! shadowed or duplicated.

use std::path::{Path, PathBuf};

/// A defensive upper bound on a single sound file's size - generous for what this library is for
/// (a short UI cue, not a song): even an uncompressed 44.1kHz 16-bit stereo WAV several seconds
/// long is a fraction of this. Big enough that a real short sound effect never trips it, small
/// enough that a user accidentally pointing "Import sound..." at a multi-minute audio file gets a
/// real, immediate error instead of a multi-megabyte file quietly joining the library.
pub const MAX_SOUND_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// The file extensions the sound library accepts, both for files already sitting in
/// `~/.config/jerry/sounds/` ([`load_user_sounds_from_dir`]) and for [`import_sound_file`]'s own
/// source file - matched case-insensitively, the same discipline
/// `custom_theme::load_custom_themes_from_dir` already applies to `.toml`/`.TOML` (a `Foo.WAV`
/// saved by a Windows/macOS export is a real, common spelling, not an edge case to ignore).
const ACCEPTED_EXTENSIONS: [&str; 3] = ["wav", "mp3", "ogg"];

/// `(id, display name, embedded bytes)` for every sound this app ships with - see the module docs'
/// table for where each id is assigned by default. `include_bytes!` embeds the files at compile
/// time, so there's no runtime dependency on `assets/sounds/` existing on disk once built (same
/// contract `crate::fonts::FONT_FILES` documents for the bundled fonts). Generated, not sourced -
/// see `assets/sounds/LICENSE.txt` for how, and why that sidesteps the "can this be redistributed"
/// question a real recording would raise.
const BUILTIN_SOUNDS: &[(&str, &str, &[u8])] = &[
    (
        "soft-chime",
        "Soft Chime",
        include_bytes!("../../../../assets/sounds/soft-chime.wav"),
    ),
    (
        "gentle-ping",
        "Gentle Ping",
        include_bytes!("../../../../assets/sounds/gentle-ping.wav"),
    ),
    (
        "marimba-pop",
        "Marimba Pop",
        include_bytes!("../../../../assets/sounds/marimba-pop.wav"),
    ),
    (
        "warm-bell",
        "Warm Bell",
        include_bytes!("../../../../assets/sounds/warm-bell.wav"),
    ),
    (
        "rising-blip",
        "Rising Blip",
        include_bytes!("../../../../assets/sounds/rising-blip.wav"),
    ),
];

/// Where a sound's real bytes come from - embedded in the binary, or a real file on disk. Kept
/// separate from [`LibrarySound`] itself so [`crate::sound::player::SoundPlayer`] can match on it
/// without needing to know how the id/name were derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundSource {
    Builtin(&'static [u8]),
    File(PathBuf),
}

/// One entry in the sound library - a built-in sound or a user-imported one, indistinguishable to
/// the settings-page dropdown beyond [`Self::is_builtin`]'s "Built-in"/"Imported" badge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySound {
    /// Stable identity: the filename stem for both built-in and imported sounds (`"soft-chime"`,
    /// never `"soft-chime.wav"`) - what `settings.toml`'s `sound.*.sound` fields actually store,
    /// and what [`resolve`] looks up.
    pub id: String,
    pub name: String,
    pub source: SoundSource,
}

impl LibrarySound {
    pub fn is_builtin(&self) -> bool {
        matches!(self.source, SoundSource::Builtin(_))
    }
}

/// Every real, specific way loading or importing a sound file can fail - shown verbatim in the
/// settings page's status banner rather than collapsed into one generic message, matching
/// `custom_theme::ThemeFileError`'s own discipline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SoundFileError {
    Io(String),
    /// The *source* file [`import_sound_file`] was asked to import is over
    /// [`MAX_SOUND_FILE_BYTES`] - checked before that file is ever read.
    TooLarge {
        bytes: u64,
        max_bytes: u64,
    },
    /// The file's extension isn't one of [`ACCEPTED_EXTENSIONS`].
    UnsupportedExtension(String),
    /// The file has an accepted extension but didn't actually decode as audio - a real,
    /// non-silent rejection rather than a file that joins the library only to fail the first time
    /// something tries to play it.
    NotDecodable(String),
    /// This id is already a built-in sound's id - imported sounds share the same id namespace
    /// (the filename stem) as built-ins, so a colliding import is refused rather than shadowing
    /// the built-in.
    IdCollidesWithBuiltin(String),
}

impl std::fmt::Display for SoundFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SoundFileError::Io(msg) => write!(f, "couldn't read the sound file: {msg}"),
            SoundFileError::TooLarge { bytes, max_bytes } => write!(
                f,
                "{bytes} bytes exceeds the {max_bytes}-byte limit for a sound file"
            ),
            SoundFileError::UnsupportedExtension(ext) => write!(
                f,
                "\".{ext}\" isn't a supported sound format - expected .wav, .mp3, or .ogg"
            ),
            SoundFileError::NotDecodable(detail) => {
                write!(f, "couldn't decode this file as audio: {detail}")
            }
            SoundFileError::IdCollidesWithBuiltin(id) => write!(
                f,
                "\"{id}\" is already a built-in sound - rename the file before importing it"
            ),
        }
    }
}

/// The full built-in library, in declaration order - the settings page's "Sound library" section
/// lists these first, followed by whatever [`load_user_sounds_from_dir`] finds.
pub fn builtin_sounds() -> Vec<LibrarySound> {
    BUILTIN_SOUNDS
        .iter()
        .map(|(id, name, bytes)| LibrarySound {
            id: (*id).to_string(),
            name: (*name).to_string(),
            source: SoundSource::Builtin(bytes),
        })
        .collect()
}

/// The directory real user-imported sounds live in - `{settings.toml's own directory}/sounds`,
/// the same derivation `custom_theme::custom_themes_dir_for` uses for `themes/`. Takes the
/// settings path itself (not `$HOME` directly) so this stays testable against a temp directory
/// exactly the way theme loading already is.
pub fn sounds_dir_for(settings_path: &Path) -> PathBuf {
    match settings_path.parent() {
        Some(parent) => parent.join("sounds"),
        None => PathBuf::from("sounds"),
    }
}

fn has_accepted_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            ACCEPTED_EXTENSIONS
                .iter()
                .any(|accepted| ext.eq_ignore_ascii_case(accepted))
        })
}

fn file_stem_id(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Loads every real, accepted sound file from `dir`, returning `(sounds, errors)` exactly like
/// `custom_theme::load_custom_themes_from_dir` does. A missing directory (never imported anything
/// yet) is empty results, not an error - this is the same "not configured" case
/// [`crate::sound::flow`] otherwise falls back to built-ins for.
///
/// Files are processed in sorted-path order so which one wins an id collision is deterministic
/// rather than dependent on `std::fs::read_dir`'s own unspecified iteration order (same reasoning
/// as the theme loader). A file whose stem collides with an already-loaded id (another user file,
/// or a built-in) is skipped and reported rather than silently overwriting the earlier entry in
/// the returned list - two library rows sharing one dropdown value would be a real, confusing bug,
/// not a cosmetic one.
pub fn load_user_sounds_from_dir(dir: &Path) -> (Vec<LibrarySound>, Vec<String>) {
    let mut errors = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), errors);
    };

    let mut candidate_paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| has_accepted_extension(path))
        .collect();
    candidate_paths.sort();

    let mut known_ids: std::collections::HashSet<String> = BUILTIN_SOUNDS
        .iter()
        .map(|(id, _, _)| id.to_string())
        .collect();
    let mut sounds = Vec::new();

    for path in candidate_paths {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() > MAX_SOUND_FILE_BYTES => {
                errors.push(format!(
                    "{file_name}: {} bytes exceeds the {MAX_SOUND_FILE_BYTES}-byte limit for a \
                     sound file - skipping",
                    metadata.len()
                ));
                continue;
            }
            Ok(_) => {}
            Err(err) => {
                errors.push(format!("{file_name}: couldn't read metadata: {err}"));
                continue;
            }
        }

        let id = file_stem_id(&path);
        if known_ids.contains(&id) {
            errors.push(format!(
                "{file_name}: another sound already uses the id \"{id}\" - skipping"
            ));
            continue;
        }
        known_ids.insert(id.clone());

        let name = display_name_from_id(&id);
        sounds.push(LibrarySound {
            id,
            name,
            source: SoundSource::File(path),
        });
    }

    (sounds, errors)
}

/// `"gentle-ping"` -> `"Gentle Ping"` - a readable display name derived from a user-imported
/// file's stem, since (unlike a theme file) a sound file carries no `name` field of its own to
/// read instead.
fn display_name_from_id(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Real decode validation - attempts to build a [`rodio::Decoder`] over the file's bytes. This is
/// the whole reason a sound file can't be validated the cheap way a theme's TOML is: there is no
/// schema to check, only "does this actually decode as audio". Never touches the audio device
/// itself (`Decoder::new` only builds a decoder over the byte stream, it doesn't open output) -
/// safe to call from a headless test with no sound card.
fn decode_check(bytes: &[u8]) -> Result<(), SoundFileError> {
    let cursor = std::io::Cursor::new(bytes.to_vec());
    rodio::Decoder::new(cursor)
        .map(|_| ())
        .map_err(|err| SoundFileError::NotDecodable(err.to_string()))
}

/// Validates `source_path` (extension, size cap, and a real decode check) and, only if every
/// check passes, copies it into `dest_dir` under a real, collision-safe filename - the sound
/// counterpart of `custom_theme::import_theme_file`. Nothing is written to disk if any check
/// fails.
pub fn import_sound_file(
    source_path: &Path,
    dest_dir: &Path,
) -> Result<LibrarySound, SoundFileError> {
    if !has_accepted_extension(source_path) {
        let ext = source_path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_string();
        return Err(SoundFileError::UnsupportedExtension(ext));
    }

    let metadata =
        std::fs::metadata(source_path).map_err(|err| SoundFileError::Io(err.to_string()))?;
    if metadata.len() > MAX_SOUND_FILE_BYTES {
        return Err(SoundFileError::TooLarge {
            bytes: metadata.len(),
            max_bytes: MAX_SOUND_FILE_BYTES,
        });
    }

    let bytes = std::fs::read(source_path).map_err(|err| SoundFileError::Io(err.to_string()))?;
    decode_check(&bytes)?;

    let id = file_stem_id(source_path);
    if BUILTIN_SOUNDS
        .iter()
        .any(|(builtin_id, _, _)| *builtin_id == id)
    {
        return Err(SoundFileError::IdCollidesWithBuiltin(id));
    }

    std::fs::create_dir_all(dest_dir).map_err(|err| SoundFileError::Io(err.to_string()))?;
    let dest_path = non_colliding_dest_path(dest_dir, &id, source_path);
    std::fs::copy(source_path, &dest_path).map_err(|err| SoundFileError::Io(err.to_string()))?;

    let dest_id = file_stem_id(&dest_path);
    Ok(LibrarySound {
        name: display_name_from_id(&dest_id),
        id: dest_id,
        source: SoundSource::File(dest_path),
    })
}

/// Picks a real, never-overwriting destination path inside `dest_dir` for a source file whose
/// stem is `id` - `{id}.{ext}` if free, else `{id}-2.{ext}`, `{id}-3.{ext}`, ... A sound file
/// carries no internal identity to compare against the way a theme's `name` field does (see the
/// module docs), so unlike `custom_theme::non_colliding_dest_path` there is no "same file,
/// legitimate re-import" case to special-case - every collision just gets the next free suffix.
fn non_colliding_dest_path(dest_dir: &Path, id: &str, source_path: &Path) -> PathBuf {
    let ext = source_path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    let mut candidate = dest_dir.join(format!("{id}.{ext}"));
    let mut suffix = 2;
    while candidate.symlink_metadata().is_ok() {
        candidate = dest_dir.join(format!("{id}-{suffix}.{ext}"));
        suffix += 1;
    }
    candidate
}

/// Looks `id` up in the full library (built-ins first, then loaded user sounds) - the shared
/// resolution [`crate::sound::flow`] uses to turn a `settings.toml` `sound.*.sound` value into
/// real bytes to play, and the settings page uses to render a dropdown's current selection.
pub fn resolve<'a>(id: &str, library: &'a [LibrarySound]) -> Option<&'a LibrarySound> {
    library.iter().find(|sound| sound.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write test fixture");
        path
    }

    /// A real, minimal decodable WAV: 8 silent 16-bit mono samples at 8kHz - small, but a real
    /// `rodio::Decoder` accepts it, unlike an arbitrary byte string.
    fn tiny_valid_wav() -> Vec<u8> {
        BUILTIN_SOUNDS[0].2.to_vec()
    }

    #[test]
    fn sounds_dir_for_is_a_sibling_of_the_settings_file() {
        let settings_path = Path::new("/home/user/.config/jerry/settings.toml");
        assert_eq!(
            sounds_dir_for(settings_path),
            Path::new("/home/user/.config/jerry/sounds")
        );
    }

    #[test]
    fn builtin_sounds_have_stable_non_empty_ids_and_real_bytes() {
        let sounds = builtin_sounds();
        assert_eq!(sounds.len(), BUILTIN_SOUNDS.len());
        for sound in &sounds {
            assert!(!sound.id.is_empty());
            assert!(sound.is_builtin());
            let SoundSource::Builtin(bytes) = sound.source else {
                panic!("expected a builtin source");
            };
            assert!(!bytes.is_empty());
        }
    }

    #[test]
    fn missing_sounds_dir_loads_as_empty_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let (sounds, errors) = load_user_sounds_from_dir(&missing);
        assert!(sounds.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn loads_accepted_extensions_case_insensitively() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "one.wav", &tiny_valid_wav());
        write_file(dir.path(), "TWO.WAV", &tiny_valid_wav());
        write_file(
            dir.path(),
            "three.Mp3",
            b"not really mp3 but extension matches",
        );
        let (sounds, _errors) = load_user_sounds_from_dir(dir.path());
        let ids: Vec<&str> = sounds.iter().map(|sound| sound.id.as_str()).collect();
        assert!(ids.contains(&"one"));
        assert!(ids.contains(&"TWO"));
        assert!(ids.contains(&"three"));
    }

    #[test]
    fn ignores_files_with_unaccepted_extensions() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "readme.txt", b"not audio");
        write_file(dir.path(), "cover.png", b"not audio either");
        let (sounds, errors) = load_user_sounds_from_dir(dir.path());
        assert!(sounds.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    fn oversized_file_is_reported_and_skipped_not_loaded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let oversized = vec![0u8; (MAX_SOUND_FILE_BYTES + 1) as usize];
        write_file(dir.path(), "huge.wav", &oversized);
        let (sounds, errors) = load_user_sounds_from_dir(dir.path());
        assert!(sounds.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("huge.wav"));
    }

    #[test]
    fn id_collision_between_two_user_files_keeps_the_first_sorted_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "ping.mp3", &tiny_valid_wav());
        write_file(dir.path(), "ping.wav", &tiny_valid_wav());
        let (sounds, errors) = load_user_sounds_from_dir(dir.path());
        // Sorted path order: "ping.mp3" < "ping.wav", so the mp3 wins and the wav is reported.
        assert_eq!(sounds.len(), 1);
        assert_eq!(sounds[0].id, "ping");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("ping.wav"));
    }

    #[test]
    fn id_collision_with_a_builtin_is_reported_and_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "soft-chime.mp3", &tiny_valid_wav());
        let (sounds, errors) = load_user_sounds_from_dir(dir.path());
        assert!(sounds.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("soft-chime"));
    }

    #[test]
    fn loaded_sounds_are_in_deterministic_sorted_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_file(dir.path(), "zebra.wav", &tiny_valid_wav());
        write_file(dir.path(), "alpha.wav", &tiny_valid_wav());
        let (sounds, _errors) = load_user_sounds_from_dir(dir.path());
        let ids: Vec<&str> = sounds.iter().map(|sound| sound.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "zebra"]);
    }

    #[test]
    fn import_copies_a_valid_wav_into_the_dest_dir() {
        let src_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source = write_file(src_dir.path(), "my-ding.wav", &tiny_valid_wav());

        let imported = import_sound_file(&source, dest_dir.path()).expect("import to succeed");

        assert_eq!(imported.id, "my-ding");
        assert_eq!(imported.name, "My Ding");
        let SoundSource::File(path) = &imported.source else {
            panic!("expected a File source");
        };
        assert!(path.exists());
        assert_eq!(path.parent(), Some(dest_dir.path()));
    }

    #[test]
    fn import_rejects_unsupported_extension_and_writes_nothing() {
        let src_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source = write_file(src_dir.path(), "notes.txt", b"just text");

        let result = import_sound_file(&source, dest_dir.path());

        assert_eq!(
            result,
            Err(SoundFileError::UnsupportedExtension("txt".to_string()))
        );
        assert!(std::fs::read_dir(dest_dir.path())
            .map(|mut it| it.next().is_none())
            .unwrap_or(true));
    }

    #[test]
    fn import_rejects_a_file_that_does_not_really_decode_and_writes_nothing() {
        let src_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source = write_file(src_dir.path(), "fake.wav", b"this is not really a wav file");

        let result = import_sound_file(&source, dest_dir.path());

        assert!(matches!(result, Err(SoundFileError::NotDecodable(_))));
        assert!(std::fs::read_dir(dest_dir.path())
            .map(|mut it| it.next().is_none())
            .unwrap_or(true));
    }

    #[test]
    fn import_rejects_oversized_source_and_writes_nothing() {
        let src_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let oversized = vec![0u8; (MAX_SOUND_FILE_BYTES + 1) as usize];
        let source = write_file(src_dir.path(), "huge.wav", &oversized);

        let result = import_sound_file(&source, dest_dir.path());

        assert!(matches!(result, Err(SoundFileError::TooLarge { .. })));
        assert!(std::fs::read_dir(dest_dir.path())
            .map(|mut it| it.next().is_none())
            .unwrap_or(true));
    }

    #[test]
    fn import_rejects_a_stem_that_collides_with_a_builtin_id() {
        let src_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source = write_file(src_dir.path(), "soft-chime.wav", &tiny_valid_wav());

        let result = import_sound_file(&source, dest_dir.path());

        assert_eq!(
            result,
            Err(SoundFileError::IdCollidesWithBuiltin(
                "soft-chime".to_string()
            ))
        );
    }

    #[test]
    fn importing_two_files_with_the_same_stem_never_overwrites_the_first() {
        let src_dir = tempfile::tempdir().expect("tempdir");
        let dest_dir = tempfile::tempdir().expect("tempdir");
        let source_a_dir = src_dir.path().join("a");
        std::fs::create_dir_all(&source_a_dir).expect("mkdir a");
        let source_a = write_file(&source_a_dir, "ping.wav", &tiny_valid_wav());
        let source_b_dir = src_dir.path().join("b");
        std::fs::create_dir_all(&source_b_dir).expect("mkdir b");
        let source_b = write_file(&source_b_dir, "ping.wav", &tiny_valid_wav());

        let first = import_sound_file(&source_a, dest_dir.path()).expect("first import");
        let second = import_sound_file(&source_b, dest_dir.path()).expect("second import");

        assert_eq!(first.id, "ping");
        assert_eq!(second.id, "ping-2");
        let SoundSource::File(first_path) = &first.source else {
            panic!("expected File source");
        };
        assert!(first_path.exists(), "first import must not be overwritten");
    }

    /// Unlike `crate::settings::builtin_themes` (whose checked-in files are one derivation's own
    /// *output*, so a drift test compares two independently-computable sources of truth),
    /// [`BUILTIN_SOUNDS`]'s bytes come straight from `include_bytes!` on the checked-in files
    /// themselves - there is no second, independently-computed copy that could disagree with
    /// them. What *can* still drift is the file list: someone drops a new `.wav` into
    /// `assets/sounds/` without adding its `include_bytes!` entry (silently never offered in the
    /// library), or removes an entry's own referenced file (which would already be a hard
    /// compile error, not a silent one - `include_bytes!` fails the build). This test catches the
    /// first, real, silent case: every real file in `assets/sounds/` must be exactly the set
    /// [`BUILTIN_SOUNDS`] declares.
    #[test]
    fn every_real_file_in_assets_sounds_is_a_declared_builtin_and_vice_versa() {
        let assets_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/sounds")
            .canonicalize()
            .expect("the real assets/sounds directory must exist");

        let mut real_files: Vec<String> = std::fs::read_dir(&assets_dir)
            .expect("assets/sounds must be readable")
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
            })
            .filter_map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .collect();
        real_files.sort();

        let mut declared: Vec<String> = BUILTIN_SOUNDS
            .iter()
            .map(|(id, _, _)| format!("{id}.wav"))
            .collect();
        declared.sort();

        assert_eq!(
            real_files, declared,
            "assets/sounds/*.wav must exactly match BUILTIN_SOUNDS' declared ids - a file was \
             added or removed on disk without updating the embedded list in library.rs"
        );
    }

    #[test]
    fn resolve_finds_a_builtin_and_a_user_sound_and_returns_none_for_unknown() {
        let mut library = builtin_sounds();
        library.push(LibrarySound {
            id: "my-custom".to_string(),
            name: "My Custom".to_string(),
            source: SoundSource::File(PathBuf::from("/tmp/my-custom.wav")),
        });

        assert!(resolve("soft-chime", &library).is_some());
        assert!(resolve("my-custom", &library).is_some());
        assert!(resolve("does-not-exist", &library).is_none());
    }
}
