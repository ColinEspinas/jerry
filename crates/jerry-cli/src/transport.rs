//! Connected or standalone: a pure decision from the environment, the repository and the
//! registry's contents. Nothing here opens a connection.

use jerry_core::registry::{Descriptor, Registry, RegistryError, Resolution};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// A registry entry for this repository; version-checked before connecting.
    Host(Descriptor),
    /// A socket named explicitly, by `JERRY_HOST_SOCKET` or `--instance`.
    Socket(PathBuf),
    /// No Jerry serves this repository; git-only requests run here.
    Standalone,
}

#[derive(Debug)]
pub enum ChooseError {
    /// Several live Jerry instances serve the repository.
    Ambiguous(Vec<Descriptor>),
    Registry(RegistryError),
}

/// `JERRY_HOST_SOCKET` wins, then `--instance`, then the registry's answer for `repo`. No
/// registry at all means standalone.
pub fn choose(
    env: &dyn Fn(&str) -> Option<OsString>,
    repo: &Path,
    registry: Option<&Registry>,
    instance: Option<&Path>,
) -> Result<Transport, ChooseError> {
    if let Some(socket) = env(crate::SOCKET_ENV) {
        return Ok(Transport::Socket(PathBuf::from(socket)));
    }
    if let Some(socket) = instance {
        return Ok(Transport::Socket(socket.to_path_buf()));
    }
    let Some(registry) = registry else {
        return Ok(Transport::Standalone);
    };
    match registry.resolve(repo).map_err(ChooseError::Registry)? {
        Resolution::None => Ok(Transport::Standalone),
        Resolution::One(descriptor) => Ok(Transport::Host(descriptor)),
        Resolution::Many(descriptors) => Err(ChooseError::Ambiguous(descriptors)),
    }
}

#[cfg(test)]
mod transport_choice_tests {
    use super::{choose, ChooseError, Transport};
    use jerry_core::client::Listener;
    use jerry_core::registry::Registry;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use test_support::seed_empty_repo;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(*v)))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    /// A registry directory short enough for every platform's `sun_path`, removed on drop.
    struct TestRegistry {
        registry: Registry,
        _temp: Option<tempfile::TempDir>,
    }

    impl Drop for TestRegistry {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.registry.dir());
        }
    }

    fn registry(tag: &str) -> TestRegistry {
        if cfg!(windows) {
            let dir = jerry_core::registry::runtime_dir()
                .expect("runtime dir")
                .join(format!("cli-{:x}-{tag}", std::process::id()));
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

    #[test]
    fn the_environment_and_the_flag_name_a_socket_directly() {
        let by_env = choose(
            &env(&[(crate::SOCKET_ENV, "/run/x.sock")]),
            Path::new("/repo/.git"),
            None,
            Some(Path::new("/ignored.sock")),
        )
        .expect("env wins");
        assert_eq!(by_env, Transport::Socket(PathBuf::from("/run/x.sock")));

        let by_flag = choose(
            &env(&[]),
            Path::new("/repo/.git"),
            None,
            Some(Path::new("/run/y.sock")),
        )
        .expect("flag");
        assert_eq!(by_flag, Transport::Socket(PathBuf::from("/run/y.sock")));
    }

    #[test]
    fn the_registry_decides_between_standalone_one_host_and_ambiguity() {
        let test = registry("choose");
        let repo = seed_empty_repo();
        let repos = vec![repo.path().to_path_buf()];
        let no_env = env(&[]);

        assert_eq!(
            choose(&no_env, repo.path(), None, None).expect("no registry"),
            Transport::Standalone
        );
        assert_eq!(
            choose(&no_env, repo.path(), Some(&test.registry), None).expect("empty"),
            Transport::Standalone
        );

        let first = test.registry.allocate().expect("allocate");
        let _first_listener = Listener::bind(&first.socket).expect("bind");
        test.registry.publish(&first, &repos).expect("publish");
        match choose(&no_env, repo.path(), Some(&test.registry), None).expect("one") {
            Transport::Host(descriptor) => assert_eq!(descriptor.socket, first.socket),
            other => panic!("expected one host, got {other:?}"),
        }

        let second = test.registry.allocate().expect("allocate");
        let _second_listener = Listener::bind(&second.socket).expect("bind");
        test.registry.publish(&second, &repos).expect("publish");
        assert!(matches!(
            choose(&no_env, repo.path(), Some(&test.registry), None),
            Err(ChooseError::Ambiguous(ref many)) if many.len() == 2
        ));
    }
}
