//! How a client finds the host for a repository: each host publishes `<instance>.json` beside
//! `<instance>.sock` in a private runtime directory. Liveness is the socket accepting a
//! connection; a dead entry is unlinked on discovery, and no name is ever reclaimed.

use crate::wire::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The tightest `sun_path` is macOS's 104 bytes (Linux and Windows allow 108); 100 leaves room
/// for the terminator on every platform.
pub const MAX_SOCKET_PATH_BYTES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    pub protocol_version: u32,
    pub pid: u32,
    /// Seconds since the Unix epoch.
    pub started_at: u64,
    /// The common `.git` directory of every repository this host serves, canonicalized. An
    /// in-process host serves every repository the app has open; a stage 3 host serves one.
    pub repos: Vec<PathBuf>,
    pub socket: PathBuf,
}

/// A reserved instance name and the two paths it owns. The host binds `socket` first, then
/// publishes the descriptor, so a discoverable entry always has a listener behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub name: String,
    pub socket: PathBuf,
    pub descriptor: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Live,
    Dead,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    None,
    One(Descriptor),
    /// More than one live host claims the repository; the caller must pick with `--instance`.
    Many(Vec<Descriptor>),
}

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("no runtime directory: {0} is not set")]
    MissingEnv(&'static str),
    #[error("socket path {} is {len} bytes, over the {MAX_SOCKET_PATH_BYTES}-byte limit", path.display())]
    SocketPathTooLong { path: PathBuf, len: usize },
    #[error("{} exists but is not a plain directory", path.display())]
    NotADirectory { path: PathBuf },
    #[error("registry I/O at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("descriptor {} is not valid: {source}", path.display())]
    Descriptor {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// The platform whose runtime-directory rule applies; a parameter so every rule is testable on
/// every host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Unix,
}

impl Os {
    pub const fn host() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Unix
        }
    }
}

/// The private runtime directory for `os`, from an environment lookup:
/// `%LOCALAPPDATA%\jerry\run`, `$TMPDIR/jerry-<user>`, or `$XDG_RUNTIME_DIR/jerry` with a
/// `/tmp/jerry-<user>` fallback. The lookup hands back `OsString`s because these are paths.
pub fn runtime_dir_for(
    os: Os,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, RegistryError> {
    let user = || {
        env("USER")
            .or_else(|| env("LOGNAME"))
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_owned())
    };
    Ok(match os {
        Os::Windows => {
            PathBuf::from(env("LOCALAPPDATA").ok_or(RegistryError::MissingEnv("LOCALAPPDATA"))?)
                .join("jerry")
                .join("run")
        }
        Os::MacOs => PathBuf::from(env("TMPDIR").unwrap_or_else(|| "/tmp".into()))
            .join(format!("jerry-{}", user())),
        Os::Unix => match env("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir).join("jerry"),
            None => PathBuf::from("/tmp").join(format!("jerry-{}", user())),
        },
    })
}

/// [`runtime_dir_for`] on this host, reading the real environment.
pub fn runtime_dir() -> Result<PathBuf, RegistryError> {
    runtime_dir_for(Os::host(), &|key| std::env::var_os(key))
}

pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// Creates the directory if needed, private to the user on Unix (`0700`). An existing path
    /// that is not a plain directory (a symlink, a file) is refused rather than adopted.
    pub fn open(dir: PathBuf) -> Result<Registry, RegistryError> {
        create_private_dir(&dir)?;
        Ok(Registry { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Reserves a fresh instance name, unique per process and never derived from a repository.
    pub fn allocate(&self) -> Result<Instance, RegistryError> {
        let name = format!("{:x}-{:08x}", std::process::id(), fresh_u32());
        let socket = self.dir.join(format!("{name}.sock"));
        let len = socket.as_os_str().len();
        if len > MAX_SOCKET_PATH_BYTES {
            return Err(RegistryError::SocketPathTooLong { path: socket, len });
        }
        Ok(Instance {
            descriptor: self.dir.join(format!("{name}.json")),
            socket,
            name,
        })
    }

    /// Writes the descriptor atomically (temp file, then rename); republish to change `repos`.
    pub fn publish(
        &self,
        instance: &Instance,
        repos: &[PathBuf],
    ) -> Result<Descriptor, RegistryError> {
        let repos = repos
            .iter()
            .map(|repo| {
                fs::canonicalize(repo).map_err(|source| RegistryError::Io {
                    path: repo.clone(),
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let descriptor = Descriptor {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            repos,
            socket: instance.socket.clone(),
        };
        let body =
            serde_json::to_vec_pretty(&descriptor).map_err(|source| RegistryError::Descriptor {
                path: instance.descriptor.clone(),
                source,
            })?;
        let temp = instance.descriptor.with_extension("json.tmp");
        fs::write(&temp, body).map_err(|source| RegistryError::Io {
            path: temp.clone(),
            source,
        })?;
        fs::rename(&temp, &instance.descriptor).map_err(|source| RegistryError::Io {
            path: instance.descriptor.clone(),
            source,
        })?;
        Ok(descriptor)
    }

    /// Removes an instance's descriptor and socket file; missing files are not errors.
    pub fn remove(&self, instance: &Instance) -> Result<(), RegistryError> {
        remove_if_present(&instance.descriptor)?;
        remove_if_present(&instance.socket)
    }

    /// Every readable descriptor, live or not. Unparseable files are skipped, not fatal: one
    /// half-written entry must not hide every other host.
    pub fn entries(&self) -> Result<Vec<Descriptor>, RegistryError> {
        Ok(self
            .read_entries()?
            .into_iter()
            .map(|(_, descriptor)| descriptor)
            .collect())
    }

    /// The live host(s) for `repo`, sweeping dead entries for that repository along the way.
    pub fn resolve(&self, repo: &Path) -> Result<Resolution, RegistryError> {
        let repo = fs::canonicalize(repo).map_err(|source| RegistryError::Io {
            path: repo.to_path_buf(),
            source,
        })?;
        let mut live = Vec::new();
        for (path, descriptor) in self.read_entries()? {
            if !descriptor.repos.contains(&repo) {
                continue;
            }
            match probe(&descriptor.socket) {
                Liveness::Live => live.push(descriptor),
                Liveness::Dead => self.sweep(&path, &descriptor)?,
            }
        }
        Ok(match live.len() {
            0 => Resolution::None,
            1 => Resolution::One(live.remove(0)),
            _ => Resolution::Many(live),
        })
    }

    /// Claims the right to spawn a host for `repo`: creates `<key>.spawning` via `create_new`,
    /// so exactly one of several racing callers wins it (`docs/architecture/decisions.md` §24 -
    /// `resolve` seeing `Resolution::None` and then spawning has no claim step between the two,
    /// which let two concurrent callers both spawn a host for the same repository). `Ok(true)`
    /// means this call holds the claim and should spawn; `Ok(false)` means a fresh claim already
    /// exists and this call should wait for its holder's descriptor instead of spawning its own.
    /// A claim older than `max_age` is stale - its holder must have died mid-spawn - and is swept
    /// exactly like a dead descriptor already is, then retried once.
    pub fn claim(&self, repo: &Path, max_age: Duration) -> Result<bool, RegistryError> {
        let repo = fs::canonicalize(repo).map_err(|source| RegistryError::Io {
            path: repo.to_path_buf(),
            source,
        })?;
        let path = self.claim_path(&repo);
        if try_create_claim(&path)? {
            return Ok(true);
        }
        if !claim_is_fresh(&path, max_age) {
            remove_if_present(&path)?;
            return try_create_claim(&path);
        }
        Ok(false)
    }

    /// Releases a claim this process holds - a no-op if it is already gone (published, swept as
    /// stale, or never held). Every `Self::claim` caller must call this exactly once, win or
    /// lose the race, once it either spawns and publishes or gives up waiting.
    pub fn release_claim(&self, repo: &Path) -> Result<(), RegistryError> {
        let repo = fs::canonicalize(repo).map_err(|source| RegistryError::Io {
            path: repo.to_path_buf(),
            source,
        })?;
        remove_if_present(&self.claim_path(&repo))
    }

    /// A short, deterministic file name for `canonical_repo`'s claim - the same repository, from
    /// any process, must hash to the same path for `Self::claim` to arbitrate between them at
    /// all. Not `Registry::allocate`'s own random name: that identifies *a* new instance, this
    /// identifies *the* claim for one specific repository.
    fn claim_path(&self, canonical_repo: &Path) -> PathBuf {
        let digest = fnv1a64(canonical_repo.to_string_lossy().as_bytes());
        self.dir.join(format!("{digest:016x}.spawning"))
    }

    fn read_entries(&self) -> Result<Vec<(PathBuf, Descriptor)>, RegistryError> {
        let read = fs::read_dir(&self.dir).map_err(|source| RegistryError::Io {
            path: self.dir.clone(),
            source,
        })?;
        let mut entries = Vec::new();
        for entry in read {
            let path = entry
                .map_err(|source| RegistryError::Io {
                    path: self.dir.clone(),
                    source,
                })?
                .path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(body) = fs::read(&path) else { continue };
            if let Ok(descriptor) = serde_json::from_slice::<Descriptor>(&body) {
                entries.push((path, descriptor));
            }
        }
        Ok(entries)
    }

    /// Unlinks a dead entry's own files only: the descriptor at the path it was read from, and
    /// its socket only when that sits in this registry directory. A descriptor is untrusted
    /// input and must never make this process delete an arbitrary path.
    fn sweep(&self, descriptor_path: &Path, descriptor: &Descriptor) -> Result<(), RegistryError> {
        remove_if_present(descriptor_path)?;
        if descriptor.socket.parent() == Some(self.dir.as_path()) {
            remove_if_present(&descriptor.socket)?;
        }
        Ok(())
    }
}

/// A socket is live if something accepts on it. A missing file or a refused connection is dead.
pub fn probe(socket: &Path) -> Liveness {
    match crate::client::Stream::connect(socket) {
        Ok(_) => Liveness::Live,
        Err(_) => Liveness::Dead,
    }
}

/// Creates `path` only if it does not already exist - the one atomic primitive `Registry::claim`
/// needs, so two processes racing this call can never both get `Ok(true)`.
fn try_create_claim(path: &Path) -> Result<bool, RegistryError> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(_file) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(source) => Err(RegistryError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Whether `path`'s claim was created within `max_age`. Unreadable metadata (the claim was
/// removed - its holder published and released it - between `try_create_claim` failing and this
/// check) counts as *not* fresh: `Registry::claim` then retries creating it, which is correct
/// either way - the previous holder is done either way, whether it finished or just vanished.
fn claim_is_fresh(path: &Path, max_age: Duration) -> bool {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map(|modified| modified.elapsed().unwrap_or(Duration::ZERO) <= max_age)
        .unwrap_or(false)
}

/// A plain FNV-1a over `bytes` - deterministic across processes and Rust versions (unlike
/// `std::collections::hash_map::DefaultHasher`, which offers no such guarantee, or `RandomState`,
/// which is keyed per-process by design), so every caller claiming the same repository computes
/// the same file name.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn remove_if_present(path: &Path) -> Result<(), RegistryError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(RegistryError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Distinct on every call within a process: a per-process counter mixed with the clock and the
/// pid through the standard library's randomly keyed hasher.
fn fresh_u32() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    hasher.write_u32(nanos);
    hasher.write_u32(COUNTER.fetch_add(1, Ordering::Relaxed));
    hasher.finish() as u32
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), RegistryError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let io = |source| RegistryError::Io {
        path: dir.to_path_buf(),
        source,
    };
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            return Err(RegistryError::NotADirectory {
                path: dir.to_path_buf(),
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(io)?;
        }
        Err(error) => return Err(io(error)),
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io)
}

#[cfg(windows)]
fn create_private_dir(dir: &Path) -> Result<(), RegistryError> {
    // `%LOCALAPPDATA%` is already private to the user; its default ACL is the defence here.
    match fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_dir() => Ok(()),
        Ok(_) => Err(RegistryError::NotADirectory {
            path: dir.to_path_buf(),
        }),
        Err(_) => fs::create_dir_all(dir).map_err(|source| RegistryError::Io {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod registry_tests {
    use super::{runtime_dir_for, Os, Registry, RegistryError, Resolution};
    use crate::client::Listener;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(*v)))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn each_platform_has_its_own_short_private_runtime_directory() {
        let windows = runtime_dir_for(
            Os::Windows,
            &env(&[("LOCALAPPDATA", r"C:\Users\c\AppData\Local")]),
        )
        .expect("windows");
        assert_eq!(
            windows,
            PathBuf::from(r"C:\Users\c\AppData\Local")
                .join("jerry")
                .join("run")
        );

        let macos = runtime_dir_for(
            Os::MacOs,
            &env(&[("TMPDIR", "/var/folders/xy/T"), ("USER", "colin")]),
        )
        .expect("macos");
        assert_eq!(macos, PathBuf::from("/var/folders/xy/T/jerry-colin"));

        let xdg =
            runtime_dir_for(Os::Unix, &env(&[("XDG_RUNTIME_DIR", "/run/user/1000")])).expect("xdg");
        assert_eq!(xdg, PathBuf::from("/run/user/1000/jerry"));

        let fallback = runtime_dir_for(Os::Unix, &env(&[("LOGNAME", "c")])).expect("fallback");
        assert_eq!(fallback, PathBuf::from("/tmp/jerry-c"));

        assert!(matches!(
            runtime_dir_for(Os::Windows, &env(&[])),
            Err(RegistryError::MissingEnv("LOCALAPPDATA"))
        ));
    }

    /// A registry in a directory of its own, short enough for [`super::MAX_SOCKET_PATH_BYTES`]
    /// on every platform (a nested temp dir is often too long on Windows, so there it sits
    /// under the real runtime dir), and removed on drop even when the test fails.
    struct TestRegistry {
        registry: Registry,
        _temp: Option<tempfile::TempDir>,
    }

    impl Drop for TestRegistry {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.registry.dir());
        }
    }

    fn short_registry(tag: &str) -> TestRegistry {
        if cfg!(windows) {
            let dir = super::runtime_dir()
                .expect("runtime dir")
                .join(format!("t-{:x}-{tag}", std::process::id()));
            TestRegistry {
                registry: Registry::open(dir).expect("registry"),
                _temp: None,
            }
        } else {
            let temp = tempfile::TempDir::new().expect("tempdir");
            TestRegistry {
                registry: Registry::open(temp.path().join("r")).expect("registry"),
                _temp: Some(temp),
            }
        }
    }

    /// The regression `docs/architecture/decisions.md` §24 names: two threads racing
    /// `Registry::claim` for the same repository must not both win it - the gap between
    /// `resolve` answering `None` and a caller actually spawning a host.
    #[test]
    fn exactly_one_of_two_concurrent_claims_for_the_same_repository_wins() {
        let test = short_registry("claim-race");
        let repo = test_support::seed_empty_repo();
        let registry = &test.registry;
        let repo_path = repo.path();

        let barrier = std::sync::Barrier::new(2);
        let results: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();

                        registry
                            .claim(repo_path, Duration::from_secs(5))
                            .expect("claim")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread"))
                .collect()
        });

        let winners = results.iter().filter(|&&won| won).count();
        assert_eq!(
            winners, 1,
            "exactly one of two concurrent claimants must win: {results:?}"
        );
    }

    #[test]
    fn releasing_a_claim_lets_a_later_caller_win_it() {
        let test = short_registry("claim-release");
        let repo = test_support::seed_empty_repo();
        let registry = &test.registry;

        assert!(registry
            .claim(repo.path(), Duration::from_secs(5))
            .expect("first claim"));
        assert!(
            !registry
                .claim(repo.path(), Duration::from_secs(5))
                .expect("second claim"),
            "a live claim must refuse a second claimant"
        );

        registry.release_claim(repo.path()).expect("release");
        assert!(
            registry
                .claim(repo.path(), Duration::from_secs(5))
                .expect("reclaim"),
            "releasing a claim must let the next caller win it"
        );
    }

    #[test]
    fn a_stale_claim_is_swept_and_reclaimed() {
        let test = short_registry("claim-stale");
        let repo = test_support::seed_empty_repo();
        let registry = &test.registry;

        assert!(registry
            .claim(repo.path(), Duration::from_secs(5))
            .expect("first claim"));
        // A `max_age` of zero makes the claim just created immediately stale, without a real
        // wait - `claim_is_fresh` compares `modified.elapsed()` against it, and any real elapsed
        // duration is greater than zero.
        assert!(
            registry
                .claim(repo.path(), Duration::ZERO)
                .expect("second claim sees the first as stale"),
            "a claim older than max_age must be swept and reclaimed, not block forever"
        );
    }

    #[test]
    fn a_published_host_is_found_by_its_repository_and_a_dead_one_is_swept() {
        let test = short_registry("resolve");
        let registry = &test.registry;
        let repo = test_support::seed_empty_repo();
        let repos = vec![repo.path().to_path_buf()];

        let dead = registry.allocate().expect("allocate");
        registry.publish(&dead, &repos).expect("publish dead");
        assert!(dead.descriptor.exists());

        let live = registry.allocate().expect("allocate");
        assert_ne!(live.name, dead.name);
        let _listener = Listener::bind(&live.socket).expect("bind");
        registry.publish(&live, &repos).expect("publish live");

        match registry.resolve(repo.path()).expect("resolve") {
            Resolution::One(descriptor) => {
                assert_eq!(descriptor.socket, live.socket);
                assert_eq!(descriptor.protocol_version, crate::wire::PROTOCOL_VERSION);
                assert_eq!(descriptor.pid, std::process::id());
            }
            other => panic!("expected one live host, got {other:?}"),
        }
        assert!(
            !dead.descriptor.exists(),
            "a dead descriptor is unlinked on discovery"
        );

        let elsewhere = test_support::seed_empty_repo();
        assert_eq!(
            registry.resolve(elsewhere.path()).expect("resolve"),
            Resolution::None
        );

        registry.remove(&live).expect("remove");
        assert_eq!(
            registry.resolve(repo.path()).expect("resolve"),
            Resolution::None
        );
    }

    #[test]
    fn a_dead_descriptor_never_makes_the_sweep_unlink_a_foreign_path() {
        let test = short_registry("foreign");
        let registry = &test.registry;
        let repo = test_support::seed_empty_repo();
        let outside = tempfile::TempDir::new().expect("tempdir");
        let victim = outside.path().join("not-ours.sock");
        fs::write(&victim, b"").expect("victim");

        let descriptor = super::Descriptor {
            protocol_version: crate::wire::PROTOCOL_VERSION,
            pid: 1,
            started_at: 0,
            repos: vec![fs::canonicalize(repo.path()).expect("canonical")],
            socket: victim.clone(),
        };
        let path = registry.dir().join("forged.json");
        fs::write(&path, serde_json::to_vec(&descriptor).expect("json")).expect("write");

        assert_eq!(
            registry.resolve(repo.path()).expect("resolve"),
            Resolution::None
        );
        assert!(!path.exists(), "the dead descriptor itself is swept");
        assert!(victim.exists(), "a path outside the registry is left alone");
    }

    #[test]
    fn a_half_written_descriptor_hides_nothing_else() {
        let test = short_registry("broken");
        fs::write(test.registry.dir().join("broken.json"), b"{").expect("write");
        assert_eq!(test.registry.entries().expect("entries"), Vec::new());
    }

    #[test]
    fn a_socket_path_over_the_limit_is_refused_up_front() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let deep = temp.path().join("a".repeat(120));
        let registry = Registry::open(deep).expect("open");
        assert!(matches!(
            registry.allocate(),
            Err(RegistryError::SocketPathTooLong { .. })
        ));
    }

    #[test]
    fn a_file_where_the_registry_directory_should_be_is_refused() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let file = temp.path().join("run");
        fs::write(&file, b"").expect("file");
        assert!(matches!(
            Registry::open(file),
            Err(RegistryError::NotADirectory { .. })
        ));
    }
}
