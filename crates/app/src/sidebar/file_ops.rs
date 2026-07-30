//! The real filesystem primitives behind the file tree's context menu (GitHub issue #19 §2/§3):
//! name validation, collision-free destination naming, recursive copy, move, and delete.
//!
//! Pure and GPUI-free, like [`crate::sidebar::file_tree`] beside it, so every rule here is unit
//! testable against a real `tempfile::TempDir` without a window. The `impl AdeApp` glue that
//! decides *when* to call these - and what to do to open tabs afterwards - lives in
//! [`crate::sidebar::tree_ops`].
//!
//! ## Why these are plain `std::fs` calls and not `crate::code_surface`'s save pipeline
//!
//! `crate::code_surface::editing::AdeApp::save_active_file` guards a *write of buffer content*
//! against a concurrent external edit: it compares a fresh `std::fs::metadata` read against the
//! `saved_mtime`/`saved_len` the buffer was seeded with, so an agent CLI that rewrote the file
//! since it was opened can't have its work silently overwritten. That guard has nothing to
//! protect here: none of these operations write buffer content. They rename, copy or remove a
//! path as a whole, and the identity question they need answered is "does something already
//! exist at the destination", which every one of them asks immediately before acting.
//!
//! Neither [`move_path`] nor [`copy_path`] is atomic about that check, and both say so on
//! themselves rather than here: `std::fs::rename` silently replaces an existing destination and
//! `std::fs::copy` silently truncates one, so an entry created in the window between the check
//! and the syscall is clobbered either way. `renameat2(RENAME_NOREPLACE)`/`O_EXCL` would close
//! it, at the cost of a Linux-only path and a hand-rolled copy loop; the honest position taken
//! here is a real check with a real, documented residual window.
//!
//! ## Trash
//!
//! See [`resolve_delete_mechanism`]. The short version: on Linux/FreeBSD this shells out to a
//! real `gio trash`, which implements the freedesktop.org trash specification (verified by
//! running it: it really does move both files and directories into
//! `~/.local/share/Trash/files`). On every other platform this app has no verified, quoting-safe
//! CLI trash mechanism, so the delete is a real, permanent one - and the confirmation copy says
//! exactly that rather than claiming a trash that never happened.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// How many `name copy N` candidates [`unique_destination`] will try before giving up. A real
/// bound rather than an unbounded loop: a directory that somehow contains every candidate would
/// otherwise spin the foreground thread forever.
const MAX_COPY_SUFFIX_ATTEMPTS: usize = 1000;

/// Validates a single path *component* typed into an inline name editor (new file, new folder,
/// rename), returning the trimmed name.
///
/// The one implementation of this rule in the app: `crate::root::new_file::AdeApp::
/// create_new_file` - the pre-existing "New file" flow - calls this too rather than keeping its
/// own inline copy, so the tree's editors and the `+` menu's prompt can never drift into
/// disagreeing about what a legal name is.
///
/// Rejects, with the real message shown next to the editor:
/// - an empty (or whitespace-only) name;
/// - a path separator, so this can only ever name something *in* the chosen directory;
/// - `.` and `..`, which name a directory that already exists and would make every subsequent
///   operation act on the wrong path;
/// - an interior NUL, which no filesystem accepts and which `std::fs` would surface as an
///   opaque `InvalidInput` error much further along.
pub fn validate_entry_name(raw: &str) -> Result<&str, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("name can't be empty".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("name can't contain a path separator".to_string());
    }
    if name == "." || name == ".." {
        return Err(format!("\"{name}\" isn't a usable name"));
    }
    if name.contains('\0') {
        return Err("name can't contain a null byte".to_string());
    }
    Ok(name)
}

/// `"foo.rs"` + 1 -> `"foo copy.rs"`, + 2 -> `"foo copy 2.rs"`; `"src"` + 1 -> `"src copy"`.
///
/// macOS Finder's convention rather than GNOME Files' `"foo (copy).rs"`, chosen because it keeps
/// the suffix out of the *stem* only, leaves the extension exactly where a language chip and
/// every editor tooling look for it, and reads the same for a directory (which has no extension)
/// as for a file.
///
/// A multi-part extension is split by `Path::extension`'s own rule, so `"archive.tar.gz"`
/// becomes `"archive.tar copy.gz"`. That is the same answer Finder gives, and preserving the
/// *final* extension is what actually matters for how the copy is then opened.
pub fn copy_suffixed_name(name: &str, attempt: usize) -> String {
    let suffix = if attempt <= 1 {
        " copy".to_string()
    } else {
        format!(" copy {attempt}")
    };
    let as_path = Path::new(name);
    match (as_path.file_stem(), as_path.extension()) {
        (Some(stem), Some(extension)) if !stem.is_empty() => format!(
            "{}{suffix}.{}",
            stem.to_string_lossy(),
            extension.to_string_lossy()
        ),
        // No extension at all (a directory, `Makefile`), or a dotfile whose whole name is the
        // "extension" (`.gitignore` -> stem is empty): suffix the whole name.
        _ => format!("{name}{suffix}"),
    }
}

/// The first free path for `name` inside `dir` - `dir/name` itself when nothing is there, and
/// otherwise the first [`copy_suffixed_name`] candidate that doesn't exist.
///
/// This is what makes "paste into the folder you copied from" produce a real second file rather
/// than either silently overwriting the original or failing with a collision error (issue #19
/// §3). Existence is checked with `Path::symlink_metadata`, not `Path::exists`: a *broken*
/// symlink is still a real directory entry that a create would collide with, and `exists()`
/// follows the link and reports `false` for it.
///
/// A name counts as free only on a real `NotFound`, never on any other error: a `EACCES` on the
/// parent directory means "I can't tell", and treating that as free would hand the caller a
/// destination whose create then fails with an unrelated-looking error several frames later.
pub fn unique_destination(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let is_free = |path: &Path| matches!(path.symlink_metadata(), Err(err) if err.kind() == io::ErrorKind::NotFound);
    let direct = dir.join(name);
    if is_free(&direct) {
        return Ok(direct);
    }
    for attempt in 1..=MAX_COPY_SUFFIX_ATTEMPTS {
        let candidate = dir.join(copy_suffixed_name(name, attempt));
        if is_free(&candidate) {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("\"{name}\" and {MAX_COPY_SUFFIX_ATTEMPTS} \"copy\" variants of it all already exist in {}", dir.display()),
    ))
}

/// Whether `path` is `ancestor` itself or lives underneath it - the guard that stops a folder
/// being copied or moved into its own subtree, which would otherwise either recurse until the
/// disk filled or (for a move) make the whole subtree unreachable.
pub fn is_self_or_descendant(ancestor: &Path, path: &Path) -> bool {
    path == ancestor || path.starts_with(ancestor)
}

/// Moves `source` to `destination` (a real `std::fs::rename`), used by both Rename and a
/// Cut+Paste.
///
/// Refuses when something already exists at `destination`. That check is deliberately *not*
/// claimed to be atomic: `std::fs::rename` on Unix silently replaces an existing destination,
/// so a file created at `destination` between this check and the syscall would be clobbered.
/// Closing that window needs `renameat2(RENAME_NOREPLACE)`, which is Linux-only and not exposed
/// by `std`; this app targets three platforms from one code path, so the honest position is a
/// real check with a real, documented residual window rather than a claim of atomicity it can't
/// keep on every platform.
pub fn move_path(source: &Path, destination: &Path) -> io::Result<()> {
    if source == destination {
        return Ok(());
    }
    if destination.symlink_metadata().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", destination.display()),
        ));
    }
    if source.is_dir() && is_self_or_descendant(source, destination) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a folder can't be moved inside itself".to_string(),
        ));
    }
    fs::rename(source, destination)
}

/// Copies `source` to `destination`, recursing for a directory. Used by Duplicate and by a
/// Copy+Paste.
///
/// Refuses to copy a directory into its own subtree ([`is_self_or_descendant`]) *before*
/// creating anything.
///
/// **Symlinks are followed, not recreated**, and the dir/file decision uses `fs::metadata`
/// (following) rather than `fs::symlink_metadata`. That is a real fix, not a stylistic choice: an
/// earlier version decided with `symlink_metadata`, so a symlink *to a directory* reported
/// `is_dir() == false` and fell through to `fs::copy`, which fails with `EISDIR` - and inside the
/// recursion, a symlinked subdirectory aborted the whole walk mid-tree. Following matches this
/// tree's own walk (`crate::sidebar::file_tree::build_file_tree` doesn't distinguish them
/// either), and silently producing a link pointing outside the worktree would be the more
/// surprising of the two behaviours. A symlink *cycle* is bounded by the same recursion guard the
/// walk uses - see [`MAX_COPY_DEPTH`].
///
/// **Cleanup on failure.** A directory copy that fails part-way (a permission error, a full disk)
/// removes the destination tree it created before returning, so the sidebar never repaints
/// showing a half-copied `foo copy/` that looks complete. Only ever `remove_dir_all`s a directory
/// this call itself created one line earlier - `copy_path` has already refused an occupied
/// destination above.
///
/// **Not atomic, and the window is real.** `fs::copy` opens the destination `O_CREAT|O_TRUNC`, so
/// something created at `destination` between the existence check above and the copy is
/// truncated. Same class as [`move_path`]'s own documented `fs::rename` window, with a worse
/// outcome; closing it needs `O_EXCL` plus a hand-rolled streaming copy (and loses `fs::copy`'s
/// permission-preservation and platform fast paths), which isn't worth it for a
/// destination name that was chosen microseconds earlier by [`unique_destination`].
pub fn copy_path(source: &Path, destination: &Path) -> io::Result<()> {
    if destination.symlink_metadata().is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", destination.display()),
        ));
    }
    // Following (`metadata`), deliberately - see this function's own docs.
    if fs::metadata(source)?.is_dir() {
        if is_self_or_descendant(source, destination) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a folder can't be copied inside itself".to_string(),
            ));
        }
        copy_dir_recursive(source, destination, 0).inspect_err(|_| {
            // Best-effort: if this fails too there is nothing further to try, and the real
            // error to report is the copy's, not the cleanup's.
            let _ = fs::remove_dir_all(destination);
        })
    } else {
        fs::copy(source, destination).map(|_| ())
    }
}

/// How deep [`copy_path`] will recurse. Mirrors `crate::sidebar::file_tree::MAX_DEPTH`'s own
/// reasoning, with one addition that matters more here: because the copy *follows* symlinks, a
/// symlink pointing at one of its own ancestors would otherwise recurse until the disk filled.
/// The `is_self_or_descendant` guard only covers the literal source/destination pair, not a link
/// buried inside the tree.
pub const MAX_COPY_DEPTH: usize = crate::sidebar::file_tree::MAX_DEPTH;

fn copy_dir_recursive(source: &Path, destination: &Path, depth: usize) -> io::Result<()> {
    if depth >= MAX_COPY_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is nested more than {MAX_COPY_DEPTH} levels deep (or contains a symlink \
                 cycle) - refusing to copy further",
                source.display()
            ),
        ));
    }
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = destination.join(entry.file_name());
        if fs::metadata(&from)?.is_dir() {
            copy_dir_recursive(&from, &to, depth + 1)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// A real, permanent removal of `path` (a file or a whole directory tree) - only ever reached
/// when [`resolve_delete_mechanism`] reported [`DeleteMechanism::Permanent`] *and* the
/// confirmation the user accepted said so in those words.
pub fn delete_permanently(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// What deleting `path` will genuinely do on this machine - resolved once, immediately before
/// the confirmation is shown, so the confirmation copy and the operation can never disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteMechanism {
    /// A real OS trash command that was found on `$PATH`. Running it moves the path into the
    /// system trash, from which the user can restore it outside this app.
    Trash {
        program: &'static str,
        args: Vec<OsString>,
    },
    /// No trash mechanism is available, so the delete is irreversible.
    Permanent,
}

/// The real trash command for `target_os`, or `None` when this app has no verified one.
///
/// `target_os` is an injected `std::env::consts::OS`-shaped string, exactly like
/// `crate::settings::widgets`' own `open_command_for` - the same "pure decision function plus a
/// thin real-execution wrapper" shape this codebase already uses for `xdg-open`, so the decision
/// is unit testable without spawning anything.
///
/// - **Linux / FreeBSD (and any other Unix)**: real `gio trash -- <path>`. `gio` is GLib's own
///   CLI, and `gio trash` is a direct wrapper around `g_file_trash`, which implements the
///   freedesktop.org trash specification for local files entirely in-process (no session bus, no
///   gvfs daemon needed for a local path). Verified for real in this project's own environment,
///   not assumed: trashing both a file and a non-empty directory succeeded and both appeared in
///   `~/.local/share/Trash/files`. The `--` is a real, supported `gio` argument terminator and is
///   what keeps a filename that begins with `-` from being parsed as an option (also verified by
///   running it).
/// - **macOS / Windows**: deliberately `None`. Neither has a trash CLI this app can invoke
///   safely without a mechanism it has never executed: macOS's usual answer is an `osascript`
///   snippet whose *only* interface is an AppleScript string literal, so every quote, backslash
///   and newline in a filename has to be escaped correctly by hand or the command silently acts
///   on a different path than the one confirmed; Windows has no built-in recycle-bin CLI at all
///   (it needs the `Shell.Application` COM object or the `Microsoft.VisualBasic` assembly through
///   PowerShell). A wrong guess here doesn't degrade gracefully - it destroys the wrong file, or
///   reports "moved to trash" for something still sitting on disk. Returning `None` makes those
///   platforms take the real, clearly-labelled permanent-delete path instead, which is honest.
pub fn trash_command_for(target_os: &str, path: &Path) -> Option<(&'static str, Vec<OsString>)> {
    match target_os {
        "macos" | "windows" => None,
        _ => Some((
            "gio",
            vec!["trash".into(), "--".into(), path.as_os_str().to_os_string()],
        )),
    }
}

/// [`trash_command_for`] plus a real "is that program actually installed" probe - `gio` is a
/// GLib package, not part of a base system, so a machine that simply doesn't have it must take
/// the permanent-delete path with the matching confirmation copy rather than a trash attempt
/// that would fail after the user already confirmed.
///
/// `program_exists` is injected so this stays a pure, testable decision; the one production
/// caller passes `pty_core::resolve_on_path`, the same real `$PATH` walk this workspace already
/// uses to detect agent CLIs and language servers.
pub fn resolve_delete_mechanism(
    target_os: &str,
    path: &Path,
    program_exists: impl Fn(&str) -> bool,
) -> DeleteMechanism {
    match trash_command_for(target_os, path) {
        Some((program, args)) if program_exists(program) => {
            DeleteMechanism::Trash { program, args }
        }
        _ => DeleteMechanism::Permanent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn a_name_must_be_a_single_non_empty_component() {
        assert_eq!(validate_entry_name("  notes.md  "), Ok("notes.md"));
        assert!(validate_entry_name("   ").is_err());
        assert!(validate_entry_name("").is_err());
        assert!(validate_entry_name("src/main.rs").is_err());
        assert!(validate_entry_name("src\\main.rs").is_err());
        assert!(validate_entry_name(".").is_err());
        assert!(validate_entry_name("..").is_err());
        assert!(validate_entry_name("a\0b").is_err());
        // A dotfile is a perfectly ordinary name, even though the tree's own walk hides it.
        assert_eq!(validate_entry_name(".gitignore"), Ok(".gitignore"));
    }

    #[test]
    fn the_copy_suffix_lands_before_the_extension() {
        assert_eq!(copy_suffixed_name("foo.rs", 1), "foo copy.rs");
        assert_eq!(copy_suffixed_name("foo.rs", 2), "foo copy 2.rs");
        assert_eq!(copy_suffixed_name("src", 1), "src copy");
        assert_eq!(copy_suffixed_name("src", 3), "src copy 3");
        assert_eq!(copy_suffixed_name("Makefile", 1), "Makefile copy");
        assert_eq!(
            copy_suffixed_name("archive.tar.gz", 1),
            "archive.tar copy.gz"
        );
        // A dotfile has no stem of its own - suffix the whole name rather than producing
        // " copy.gitignore".
        assert_eq!(copy_suffixed_name(".gitignore", 1), ".gitignore copy");
    }

    /// The core of issue #19 §3: pasting into the folder something was copied from must produce
    /// a real, differently-named second entry - never an overwrite, never an error.
    #[test]
    fn pasting_into_the_source_folder_suffixes_instead_of_colliding() {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("foo.rs"), "original").expect("write");

        let first = unique_destination(dir.path(), "foo.rs").expect("first");
        assert_eq!(first, dir.path().join("foo copy.rs"));
        fs::write(&first, "copy").expect("write");

        let second = unique_destination(dir.path(), "foo.rs").expect("second");
        assert_eq!(second, dir.path().join("foo copy 2.rs"));

        assert_eq!(
            fs::read_to_string(dir.path().join("foo.rs")).expect("read"),
            "original",
            "the original must never be touched"
        );
    }

    #[test]
    fn a_free_name_is_used_as_is() {
        let dir = TempDir::new().expect("tempdir");
        assert_eq!(
            unique_destination(dir.path(), "fresh.txt").expect("free"),
            dir.path().join("fresh.txt")
        );
    }

    /// `Path::exists` follows symlinks and reports `false` for a broken one - but a broken
    /// symlink is still a real directory entry a create would collide with.
    #[cfg(unix)]
    #[test]
    fn a_broken_symlink_still_counts_as_an_existing_entry() {
        let dir = TempDir::new().expect("tempdir");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join("link.txt"))
            .expect("symlink");
        assert!(!dir.path().join("link.txt").exists(), "premise");
        assert_eq!(
            unique_destination(dir.path(), "link.txt").expect("unique"),
            dir.path().join("link copy.txt")
        );
    }

    #[test]
    fn moving_refuses_an_occupied_destination_and_leaves_both_paths_alone() {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "a").expect("write");
        fs::write(dir.path().join("b.txt"), "b").expect("write");

        let err = move_path(&dir.path().join("a.txt"), &dir.path().join("b.txt"))
            .expect_err("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(dir.path().join("b.txt")).expect("read"),
            "b",
            "the destination's real content must survive a refused move"
        );
        assert!(dir.path().join("a.txt").exists());
    }

    #[test]
    fn moving_a_directory_moves_its_whole_subtree() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir_all(dir.path().join("src/inner")).expect("mkdir");
        fs::write(dir.path().join("src/inner/deep.rs"), "deep").expect("write");

        move_path(&dir.path().join("src"), &dir.path().join("lib")).expect("move");

        assert!(!dir.path().join("src").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("lib/inner/deep.rs")).expect("read"),
            "deep"
        );
    }

    #[test]
    fn a_folder_cannot_be_moved_or_copied_into_itself() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir(dir.path().join("src")).expect("mkdir");
        fs::write(dir.path().join("src/a.rs"), "a").expect("write");

        let source = dir.path().join("src");
        let inside = dir.path().join("src/nested");

        assert!(move_path(&source, &inside).is_err());
        assert!(copy_path(&source, &inside).is_err());
        assert!(
            !inside.exists(),
            "a refused copy must not leave a partial tree behind"
        );
    }

    #[test]
    fn copying_a_directory_reproduces_its_whole_subtree_without_touching_the_source() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir_all(dir.path().join("src/inner")).expect("mkdir");
        fs::write(dir.path().join("src/top.rs"), "top").expect("write");
        fs::write(dir.path().join("src/inner/deep.rs"), "deep").expect("write");

        copy_path(&dir.path().join("src"), &dir.path().join("src copy")).expect("copy");

        assert_eq!(
            fs::read_to_string(dir.path().join("src copy/inner/deep.rs")).expect("read"),
            "deep"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/inner/deep.rs")).expect("read"),
            "deep",
            "the source must be untouched by a copy"
        );
    }

    /// A symlink *to a directory* used to abort the copy with `EISDIR` (the dir/file decision
    /// was made with the non-following `symlink_metadata`), and a symlinked subdirectory aborted
    /// the recursion mid-tree. Both are now followed, like the tree's own walk does.
    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_directory_is_followed_rather_than_aborting_the_copy() {
        let dir = TempDir::new().expect("tempdir");
        fs::create_dir(dir.path().join("real")).expect("mkdir");
        fs::write(dir.path().join("real/inside.txt"), "inside").expect("write");
        fs::create_dir(dir.path().join("tree")).expect("mkdir");
        fs::write(dir.path().join("tree/plain.txt"), "plain").expect("write");
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("tree/linked"))
            .expect("symlink");

        // The symlink as the copy's own source.
        copy_path(
            &dir.path().join("tree/linked"),
            &dir.path().join("direct-copy"),
        )
        .expect("a symlinked directory must be copied, not rejected");
        assert_eq!(
            fs::read_to_string(dir.path().join("direct-copy/inside.txt")).expect("read"),
            "inside"
        );

        // And nested inside a tree being copied.
        copy_path(&dir.path().join("tree"), &dir.path().join("tree copy")).expect("copy tree");
        assert_eq!(
            fs::read_to_string(dir.path().join("tree copy/linked/inside.txt")).expect("read"),
            "inside",
            "a symlinked subdirectory must be walked, not abort the whole copy"
        );
        assert!(dir.path().join("tree copy/plain.txt").exists());
    }

    /// A copy that fails part-way must not leave a partial tree the sidebar would then repaint as
    /// though it were complete.
    #[cfg(unix)]
    #[test]
    fn a_copy_that_fails_part_way_removes_what_it_created() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().expect("tempdir");
        fs::create_dir_all(dir.path().join("src/readable")).expect("mkdir");
        fs::write(dir.path().join("src/readable/a.txt"), "a").expect("write");
        let locked = dir.path().join("src/locked");
        fs::create_dir(&locked).expect("mkdir");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
        if fs::read_dir(&locked).is_ok() {
            // Running as root, or a filesystem that ignores the mode - the premise doesn't hold.
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");
            return;
        }

        let destination = dir.path().join("src copy");
        let result = copy_path(&dir.path().join("src"), &destination);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).expect("chmod back");

        assert!(result.is_err(), "the unreadable subdirectory must fail it");
        assert!(
            !destination.exists(),
            "a half-copied tree must be cleaned up, not left looking complete"
        );
    }

    #[test]
    fn deleting_removes_a_file_and_a_whole_directory() {
        let dir = TempDir::new().expect("tempdir");
        fs::write(dir.path().join("a.txt"), "a").expect("write");
        fs::create_dir_all(dir.path().join("tree/inner")).expect("mkdir");
        fs::write(dir.path().join("tree/inner/deep.rs"), "deep").expect("write");

        delete_permanently(&dir.path().join("a.txt")).expect("delete file");
        delete_permanently(&dir.path().join("tree")).expect("delete dir");

        assert!(!dir.path().join("a.txt").exists());
        assert!(!dir.path().join("tree").exists());
    }

    #[test]
    fn only_platforms_with_a_verified_trash_cli_get_one() {
        let path = Path::new("/tmp/x");
        let (program, args) = trash_command_for("linux", path).expect("linux has gio trash");
        assert_eq!(program, "gio");
        assert_eq!(
            args,
            vec![
                OsString::from("trash"),
                OsString::from("--"),
                OsString::from("/tmp/x")
            ],
            "the `--` terminator is what keeps a leading-dash filename from parsing as an option"
        );
        assert!(trash_command_for("freebsd", path).is_some());
        assert!(
            trash_command_for("macos", path).is_none(),
            "no AppleScript quoting this app has never executed"
        );
        assert!(trash_command_for("windows", path).is_none());
    }

    /// The honest half: a platform that *has* a trash command still falls back to a real,
    /// clearly-labelled permanent delete when that command isn't actually installed - never a
    /// trash attempt that fails after the user already confirmed "move to trash".
    #[test]
    fn a_missing_trash_program_resolves_to_a_permanent_delete() {
        let path = Path::new("/tmp/x");
        assert_eq!(
            resolve_delete_mechanism("linux", path, |_| false),
            DeleteMechanism::Permanent
        );
        assert!(matches!(
            resolve_delete_mechanism("linux", path, |program| program == "gio"),
            DeleteMechanism::Trash { program: "gio", .. }
        ));
        assert_eq!(
            resolve_delete_mechanism("macos", path, |_| true),
            DeleteMechanism::Permanent,
            "a platform with no command at all can't be rescued by a PATH hit"
        );
    }
}
