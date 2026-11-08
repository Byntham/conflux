use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::fsutil;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum HubState {
    #[default]
    Active,
    Moving {
        id: String,
    },
    Aborted {
        id: String,
    },
    Retired {
        id: String,
        hub: String,
    },
}

pub struct HubStateStore {
    path: PathBuf,
    current: Mutex<HubState>,
}

impl HubStateStore {
    pub fn load(path: &Path) -> Result<Self> {
        let current = fsutil::read_json(path)?.unwrap_or_default();
        Ok(Self {
            path: path.to_path_buf(),
            current: Mutex::new(current),
        })
    }

    pub fn get(&self) -> HubState {
        self.current.lock().unwrap().clone()
    }

    pub fn compare_and_set(&self, expected: &HubState, next: HubState) -> Result<bool> {
        let mut current = self.current.lock().unwrap();
        if *current != *expected {
            return Ok(false);
        }
        if let Some(dir) = self.path.parent() {
            fsutil::ensure_private_dir(dir)?;
        }
        fsutil::write_json_durable(&self.path, &next)?;
        *current = next;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_compares() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("hub-state.json");
        let s = HubStateStore::load(&p).unwrap();
        assert_eq!(s.get(), HubState::Active);
        let moving = HubState::Moving { id: "a".into() };
        assert!(s.compare_and_set(&HubState::Active, moving.clone()).unwrap());
        assert!(
            !s.compare_and_set(&HubState::Active, HubState::Moving { id: "b".into() })
                .unwrap()
        );
        assert_eq!(HubStateStore::load(&p).unwrap().get(), moving);
    }

    #[test]
    fn only_one_transition_wins_a_race() {
        let d = tempfile::tempdir().unwrap();
        let s = std::sync::Arc::new(HubStateStore::load(&d.path().join("s.json")).unwrap());
        let moving = HubState::Moving { id: "x".into() };
        s.compare_and_set(&HubState::Active, moving.clone()).unwrap();
        let handles: Vec<_> = (0..16)
            .map(|i| {
                let (s, moving) = (s.clone(), moving.clone());
                std::thread::spawn(move || {
                    let next = if i % 2 == 0 {
                        HubState::Retired {
                            id: "x".into(),
                            hub: "new".into(),
                        }
                    } else {
                        HubState::Aborted { id: "x".into() }
                    };
                    s.compare_and_set(&moving, next).unwrap()
                })
            })
            .collect();
        let wins = handles.into_iter().map(|h| h.join().unwrap()).filter(|w| *w).count();
        assert_eq!(wins, 1);
    }
}
