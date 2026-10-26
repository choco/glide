// Copyright The Glide Authors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! State saved on exit and restored on the next launch in the same login
//! session.
//!
//! The [`Saver`] also saves the state as it changes, so it survives Glide
//! exiting unexpectedly. Hard crashes like segfaults give us no chance to save
//! on the way out.

use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::SystemTime;
use std::{fs, thread};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::actor::layout::LayoutManager;
use crate::actor::space_manager::SpaceState;
use crate::config::Config;
use crate::sys::session::LoginSessionId;

#[derive(Serialize, Deserialize)]
pub struct SavedState<L = LayoutManager> {
    /// The version of Glide that saved the state.
    pub version: String,
    /// The login session the state was saved in. State is only restored in
    /// the same session.
    pub session: Option<LoginSessionId>,
    pub spaces: SpaceState,
    pub layout: L,
}

impl SavedState {
    fn serialize(
        layout: &LayoutManager,
        spaces: SpaceState,
        session: LoginSessionId,
    ) -> io::Result<String> {
        let state = SavedState {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            session: Some(session),
            spaces,
            layout,
        };
        ron::ser::to_string(&state).map_err(io::Error::other)
    }

    /// Load the state at `path` without checking the session.
    pub fn load(path: &Path, config: Arc<Config>) -> anyhow::Result<Self> {
        let serialized = fs::read_to_string(path)?;
        Self::parse(&serialized, config)
    }

    /// Restore the state saved at `path` if it was saved in `session`.
    pub fn restore(
        path: &Path,
        session: Option<&LoginSessionId>,
        config: Arc<Config>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(session) = session else {
            info!("Not restoring state because the login session is unknown");
            return Ok(None);
        };
        let serialized = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let state = Self::parse(&serialized, config)
            .with_context(|| format!("Could not restore state saved in {}", path.display()))?;
        if state.session.as_ref() != Some(session) {
            info!(
                "Not restoring state saved by version {} in a different login session",
                state.version
            );
            return Ok(None);
        }
        info!("Restoring state saved by version {}", state.version);
        Ok(Some(state))
    }

    fn parse(serialized: &str, config: Arc<Config>) -> anyhow::Result<Self> {
        let state: SavedState = ron::from_str(serialized)?;
        Ok(SavedState {
            layout: state.layout.with_restored_config(config),
            ..state
        })
    }
}

/// Restore the layout saved at `path` by a version of Glide before
/// [`SavedState`], if it was saved after `boot_time`.
pub fn restore_legacy_layout(
    path: &Path,
    boot_time: Option<SystemTime>,
    config: Arc<Config>,
) -> anyhow::Result<Option<LayoutManager>> {
    let modified = match fs::metadata(path).and_then(|m| m.modified()) {
        Ok(modified) => modified,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    match boot_time {
        Some(boot_time) if modified > boot_time => {
            info!("Restoring layout saved by an older version");
            load_legacy_layout(path, config)
                .map(Some)
                .with_context(|| format!("Could not restore layout saved in {}", path.display()))
        }
        _ => {
            info!("Not restoring layout saved by an older version before this boot");
            Ok(None)
        }
    }
}

fn load_legacy_layout(path: &Path, config: Arc<Config>) -> anyhow::Result<LayoutManager> {
    let serialized = fs::read_to_string(path)?;
    let layout: LayoutManager = ron::from_str(&serialized)?;
    Ok(layout.with_restored_config(config))
}

/// Replace the contents of `path` such that a crash at any point leaves either
/// the old or the new contents in place.
fn write(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)
}

/// Saves the state as it changes, writing on a background thread.
///
/// All writes happen in order on that thread, so a newer state is never
/// replaced by an older one.
pub struct Saver {
    session: LoginSessionId,
    tx: mpsc::Sender<String>,
    /// Returns the result of the last write once `tx` is dropped.
    writer: JoinHandle<io::Result<()>>,
    /// What was last saved, to skip saving when nothing changed.
    last_layout_generation: Option<u64>,
    last_spaces: Option<SpaceState>,
    last_serialized: String,
}

impl Saver {
    pub fn spawn(path: PathBuf, session: LoginSessionId) -> Self {
        let (tx, rx) = mpsc::channel::<String>();
        let writer = thread::Builder::new()
            .name("saver".to_string())
            .spawn(move || {
                let mut result = Ok(());
                while let Ok(mut contents) = rx.recv() {
                    // Only the most recent state matters.
                    while let Ok(newer) = rx.try_recv() {
                        contents = newer;
                    }
                    result = write(&path, &contents);
                    match &result {
                        Ok(()) => debug!("Saved state to {}", path.display()),
                        Err(e) => warn!("Could not save state: {e}"),
                    }
                }
                result
            })
            .unwrap();
        Saver {
            session,
            tx,
            writer,
            last_layout_generation: None,
            last_spaces: None,
            last_serialized: String::new(),
        }
    }

    /// Save the state in the background if it changed since the last call.
    pub fn save_if_changed(&mut self, layout: &LayoutManager, spaces: &SpaceState) {
        if self.last_layout_generation == Some(layout.generation())
            && self.last_spaces.as_ref() == Some(spaces)
        {
            return;
        }
        self.last_layout_generation = Some(layout.generation());
        self.last_spaces = Some(spaces.clone());
        let serialized = match SavedState::serialize(layout, spaces.clone(), self.session.clone()) {
            Ok(s) => s,
            Err(e) => {
                warn!("Could not serialize state: {e}");
                return;
            }
        };
        // Layout generations change more often than the serialized state.
        if serialized == self.last_serialized {
            return;
        }
        _ = self.tx.send(serialized.clone());
        self.last_serialized = serialized;
    }

    /// Save the state and wait for all writes to finish.
    pub fn save_and_finish(self, layout: &LayoutManager, spaces: SpaceState) -> io::Result<()> {
        let serialized = SavedState::serialize(layout, spaces, self.session.clone())?;
        if serialized != self.last_serialized {
            _ = self.tx.send(serialized);
        }
        self.finish()
    }

    /// Wait for pending writes to finish, returning the result of the last one.
    pub(crate) fn finish(self) -> io::Result<()> {
        drop(self.tx);
        self.writer.join().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use tempfile::tempdir;

    use super::*;
    use crate::sys::screen::SpaceId;

    fn spaces() -> SpaceState {
        SpaceState {
            enabled: BTreeSet::from([SpaceId::new(1)]),
            disabled: BTreeSet::from([SpaceId::new(2)]),
            globally_enabled: false,
        }
    }

    fn config() -> Arc<Config> {
        Arc::new(Config::default())
    }

    fn save_legacy_layout(path: &Path) {
        let layout = LayoutManager::new_for_test();
        fs::write(path, ron::ser::to_string(&layout).unwrap()).unwrap();
    }

    fn save_in_session(path: &Path, session: &str) {
        let state = SavedState {
            version: "0.0.0".to_owned(),
            session: Some(LoginSessionId::new_for_test(session)),
            spaces: spaces(),
            layout: &LayoutManager::new_for_test(),
        };
        fs::write(path, ron::ser::to_string(&state).unwrap()).unwrap();
    }

    #[test]
    fn restores_state_from_the_same_session() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.ron");
        save_in_session(&path, "a");
        let session = LoginSessionId::new_for_test("a");

        let state = SavedState::restore(&path, Some(&session), config()).unwrap().unwrap();
        assert_eq!(state.spaces, spaces());
    }

    #[test]
    fn restoring_leaves_the_file_in_place() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.ron");
        save_in_session(&path, "a");
        let session = LoginSessionId::new_for_test("a");

        SavedState::restore(&path, Some(&session), config()).unwrap().unwrap();
        SavedState::restore(&path, Some(&session), config()).unwrap().unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn ignores_state_from_another_session() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.ron");
        save_in_session(&path, "a");

        let other = LoginSessionId::new_for_test("b");
        assert!(SavedState::restore(&path, Some(&other), config()).unwrap().is_none());
    }

    #[test]
    fn ignores_state_when_session_is_unknown() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.ron");
        save_in_session(&path, "a");

        assert!(SavedState::restore(&path, None, config()).unwrap().is_none());
    }

    #[test]
    fn invalid_state_is_an_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.ron");
        fs::write(&path, "not a saved state").unwrap();
        let session = LoginSessionId::new_for_test("a");

        assert!(SavedState::restore(&path, Some(&session), config()).is_err());
    }

    #[test]
    fn legacy_layout_saved_this_boot_is_restored() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("layout.ron");
        save_legacy_layout(&path);
        let boot = SystemTime::UNIX_EPOCH;

        assert!(restore_legacy_layout(&path, Some(boot), config()).unwrap().is_some());
    }

    #[test]
    fn legacy_layout_saved_before_boot_is_not_restored() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("layout.ron");
        save_legacy_layout(&path);
        let boot = SystemTime::now() + std::time::Duration::from_secs(60);

        assert!(restore_legacy_layout(&path, Some(boot), config()).unwrap().is_none());
    }

    #[test]
    fn legacy_layout_is_not_restored_without_boot_time() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("layout.ron");
        save_legacy_layout(&path);

        assert!(restore_legacy_layout(&path, None, config()).unwrap().is_none());
    }

    #[test]
    fn save_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.ron");
        let session = LoginSessionId::new_for_test("a");
        let saver = Saver::spawn(path.clone(), session.clone());
        saver.save_and_finish(&LayoutManager::new_for_test(), spaces()).unwrap();

        let state = SavedState::load(&path, config()).unwrap();
        assert_eq!(state.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(state.session, Some(session));
        assert_eq!(state.spaces, spaces());
    }

    #[test]
    fn autosave_can_be_restored() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.ron");
        let session = LoginSessionId::new_for_test("a");
        let mut saver = Saver::spawn(path.clone(), session.clone());
        saver.save_if_changed(&LayoutManager::new_for_test(), &spaces());
        saver.finish().unwrap();

        let state = SavedState::restore(&path, Some(&session), config()).unwrap().unwrap();
        assert_eq!(state.spaces, spaces());
    }

    #[test]
    fn save_and_finish_is_not_overwritten_by_earlier_saves() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.ron");
        let layout = LayoutManager::new_for_test();
        let mut saver = Saver::spawn(path.clone(), LoginSessionId::new_for_test("a"));
        let mut earlier = spaces();
        earlier.globally_enabled = true;
        saver.save_if_changed(&layout, &earlier);
        saver.save_and_finish(&layout, spaces()).unwrap();

        let state = SavedState::load(&path, config()).unwrap();
        assert_eq!(state.spaces, spaces());
    }
}
