//! How a client finds the host for a repository: each host publishes `<instance>.json` beside
//! `<instance>.sock` in a private runtime directory. Liveness is the socket accepting a
//! connection; a dead entry is unlinked on discovery, and no name is ever reclaimed.

use crate::wire::PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// macOS caps `sun_path` at 104 bytes; the runtime directory must stay short enough that an
/// instance socket fits with room to spare.
pub const MAX_SOCKET_PATH_BYTES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    pub protocol_version: u32,
    pub pid: u32,
    /// Seconds since the Unix epoch.
    pub started_at: u64,
    /// The repository's common `.git` directory, canonicalized.
    pub repo: PathBuf,
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
/// `/tmp/jerry-<user>` fallback.
pub fn runtime_dir_for(
    os: Os,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf, RegistryError> {
    let user = || {
        env("USER")
            .or_else(|| env("LOGNAME"))
            .unwrap_or_else(|| "unknown".to_owned())
    };
    Ok(match os {
        Os::Windows => {
            PathBuf::from(env("LOCALAPPDATA").ok_or(RegistryError::MissingEnv("LOCALAPPDATA"))?)
                .join("jerry")
                .join("run")
        }
        Os::MacOs => PathBuf::from(env("TMPDIR").unwrap_or_else(|| "/tmp".to_owned()))
            .join(format!("jerry-{}", user())),
        Os::Unix => match env("XDG_RUNTIME_DIR") {
            Some(dir) => PathBuf::from(dir).join("jerry"),
            None => PathBuf::from("/tmp").join(format!("jerry-{}", user())),
        },
    })
}

/// [`runtime_dir_for`] on this host, reading the real environment.
pub fn runtime_dir() -> Result<PathBuf, RegistryError> {
    runtime_dir_for(Os::host(), &|key| std::env::var(key).ok())
}

pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    /// Creates the directory if needed, private to the user on Unix (`0700`).
    pub fn open(dir: PathBuf) -> Result<Registry, RegistryError> {
        create_private_dir(&dir)?;
        Ok(Registry { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Reserves a fresh instance name. Random, never derived from a repository or reused.
    pub fn allocate(&self) -> Result<Instance, RegistryError> {
        let name = format!("{:x}-{:08x}", std::process::id(), random_u32());
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

    /// Writes the descriptor atomically (temp file, then rename).
    pub fn publish(&self, instance: &Instance, repo: &Path) -> Result<Descriptor, RegistryError> {
        let repo = fs::canonicalize(repo).map_err(|source| RegistryError::Io {
            path: repo.to_path_buf(),
            source,
        })?;
        let descriptor = Descriptor {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            repo,
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
        for path in [&instance.descriptor, &instance.socket] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(RegistryError::Io {
                        path: path.clone(),
                        source,
                    })
                }
            }
        }
        Ok(())
    }

    /// Every readable descriptor, live or not. Unparseable files are skipped, not fatal: one
    /// half-written entry must not hide every other host.
    pub fn entries(&self) -> Result<Vec<Descriptor>, RegistryError> {
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
                entries.push(descriptor);
            }
        }
        Ok(entries)
    }

    /// The live host(s) for `repo`, sweeping dead entries for that repository along the way.
    pub fn resolve(&self, repo: &Path) -> Result<Resolution, RegistryError> {
        let repo = fs::canonicalize(repo).map_err(|source| RegistryError::Io {
            path: repo.to_path_buf(),
            source,
        })?;
        let mut live = Vec::new();
        for descriptor in self.entries()? {
            if descriptor.repo != repo {
                continue;
            }
            match probe(&descriptor.socket) {
                Liveness::Live => live.push(descriptor),
                Liveness::Dead => {
                    let instance = self.instance_of(&descriptor);
                    self.remove(&instance)?;
                }
            }
        }
        Ok(match live.len() {
            0 => Resolution::None,
            1 => Resolution::One(live.remove(0)),
            _ => Resolution::Many(live),
        })
    }

    fn instance_of(&self, descriptor: &Descriptor) -> Instance {
        let name = descriptor
            .socket
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        Instance {
            descriptor: self.dir.join(format!("{name}.json")),
            socket: descriptor.socket.clone(),
            name,
        }
    }
}

/// A socket is live if something accepts on it. A missing file or a refused connection is dead.
pub fn probe(socket: &Path) -> Liveness {
    match crate::client::Stream::connect(socket) {
        Ok(_) => Liveness::Live,
        Err(_) => Liveness::Dead,
    }
}

fn random_u32() -> u32 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(std::process::id().into());
    hasher.finish() as u32
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), RegistryError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let io = |source| RegistryError::Io {
        path: dir.to_path_buf(),
        source,
    };
    match fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(io(error)),
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(io)
}

#[cfg(windows)]
fn create_private_dir(dir: &Path) -> Result<(), RegistryError> {
    // `%LOCALAPPDATA%` is already private to the user; its default ACL is the defence here.
    fs::create_dir_all(dir).map_err(|source| RegistryError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod registry_tests {
    use super::{runtime_dir_for, Os, Registry, RegistryError, Resolution};
    use crate::client::Listener;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
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

    /// Windows' AF_UNIX path limit is 108 bytes and a nested temp dir is often longer; anchor the
    /// registry at the real runtime dir on Windows and clean up after ourselves.
    fn short_registry() -> (Registry, Option<tempfile::TempDir>) {
        if cfg!(windows) {
            let dir = super::runtime_dir()
                .expect("runtime dir")
                .join(format!("t-{:x}", std::process::id()));
            (Registry::open(dir).expect("registry"), None)
        } else {
            let temp = tempfile::TempDir::new().expect("tempdir");
            (
                Registry::open(temp.path().join("r")).expect("registry"),
                Some(temp),
            )
        }
    }

    #[test]
    fn a_published_host_is_found_by_its_repository_and_a_dead_one_is_swept() {
        let (registry, _keep) = short_registry();
        let repo = test_support::seed_empty_repo();

        let dead = registry.allocate().expect("allocate");
        registry.publish(&dead, repo.path()).expect("publish dead");
        assert!(dead.descriptor.exists());

        let live = registry.allocate().expect("allocate");
        assert_ne!(live.name, dead.name);
        let _listener = Listener::bind(&live.socket).expect("bind");
        registry.publish(&live, repo.path()).expect("publish live");

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
        let _ = fs::remove_dir_all(registry.dir());
    }

    #[test]
    fn a_half_written_descriptor_hides_nothing_else() {
        let (registry, _keep) = short_registry();
        fs::write(registry.dir().join("broken.json"), b"{").expect("write");
        assert_eq!(registry.entries().expect("entries"), Vec::new());
        let _ = fs::remove_dir_all(registry.dir());
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
}
