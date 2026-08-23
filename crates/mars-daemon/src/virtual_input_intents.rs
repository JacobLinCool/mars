use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use mars_types::{AppVirtualInputSpec, AppVirtualInputs};
use serde::{Deserialize, Serialize};

const STORE_VERSION: u32 = 1;

pub(crate) type IntentApps = BTreeMap<String, Vec<AppVirtualInputSpec>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IntentStoreFile {
    version: u32,
    #[serde(default)]
    apps: IntentApps,
}

#[derive(Debug)]
pub(crate) struct VirtualInputIntentStore {
    path: PathBuf,
    apps: IntentApps,
}

impl VirtualInputIntentStore {
    pub(crate) fn load(path: PathBuf) -> Result<Self, String> {
        let apps = match fs::read_to_string(&path) {
            Ok(raw) => {
                let stored = serde_json::from_str::<IntentStoreFile>(&raw).map_err(|error| {
                    format!("failed to parse virtual-input intent store: {error}")
                })?;
                if stored.version != STORE_VERSION {
                    return Err(format!(
                        "unsupported virtual-input intent store version: expected {STORE_VERSION}, found {}",
                        stored.version
                    ));
                }
                stored.apps
            }
            Err(error) if error.kind() == ErrorKind::NotFound => IntentApps::new(),
            Err(error) => {
                return Err(format!(
                    "failed to read virtual-input intent store {}: {error}",
                    path.display()
                ));
            }
        };

        Ok(Self { path, apps })
    }

    pub(crate) fn default_path() -> Result<PathBuf, String> {
        if let Ok(path) = std::env::var("MARS_VIRTUAL_INPUT_INTENTS_PATH") {
            return Ok(PathBuf::from(path));
        }
        let home = dirs::home_dir().ok_or_else(|| "cannot determine home directory".to_string())?;
        Ok(home.join("Library/Application Support/mars/virtual_input_intents.json"))
    }

    pub(crate) fn apps(&self) -> &IntentApps {
        &self.apps
    }

    pub(crate) fn app(&self, app_id: &str) -> AppVirtualInputs {
        AppVirtualInputs {
            app_id: app_id.to_string(),
            inputs: self.apps.get(app_id).cloned().unwrap_or_default(),
        }
    }

    pub(crate) fn candidate(&self, app_id: &str, inputs: Vec<AppVirtualInputSpec>) -> IntentApps {
        let mut candidate = self.apps.clone();
        if inputs.is_empty() {
            candidate.remove(app_id);
        } else {
            candidate.insert(app_id.to_string(), inputs);
        }
        candidate
    }

    pub(crate) fn persist(&self, apps: &IntentApps) -> Result<(), String> {
        let parent = self.path.parent().ok_or_else(|| {
            format!(
                "virtual-input intent store path has no parent: {}",
                self.path.display()
            )
        })?;
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "failed to create virtual-input intent store directory {}: {error}",
                parent.display()
            )
        })?;

        let payload = serde_json::to_vec_pretty(&IntentStoreFile {
            version: STORE_VERSION,
            apps: apps.clone(),
        })
        .map_err(|error| format!("failed to serialize virtual-input intent store: {error}"))?;

        let temporary = temporary_path(&self.path)?;
        let write_result = (|| -> Result<(), String> {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&temporary)
                .map_err(|error| {
                    format!(
                        "failed to open virtual-input intent temporary file {}: {error}",
                        temporary.display()
                    )
                })?;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| {
                    format!(
                        "failed to secure virtual-input intent temporary file {}: {error}",
                        temporary.display()
                    )
                })?;
            file.write_all(&payload).map_err(|error| {
                format!(
                    "failed to write virtual-input intent temporary file {}: {error}",
                    temporary.display()
                )
            })?;
            file.sync_all().map_err(|error| {
                format!(
                    "failed to sync virtual-input intent temporary file {}: {error}",
                    temporary.display()
                )
            })?;
            fs::rename(&temporary, &self.path).map_err(|error| {
                format!(
                    "failed to commit virtual-input intent store {}: {error}",
                    self.path.display()
                )
            })?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        write_result
    }

    pub(crate) fn install(&mut self, apps: IntentApps) {
        self.apps = apps;
    }
}

fn temporary_path(path: &Path) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            format!(
                "invalid virtual-input intent store path: {}",
                path.display()
            )
        })?;
    Ok(path.with_file_name(format!(".{file_name}.tmp")))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_path(case: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("mars-intents-{case}-{nanos}.json"))
    }

    fn spec(id: &str, uid: &str) -> AppVirtualInputSpec {
        AppVirtualInputSpec {
            id: id.to_string(),
            name: id.to_string(),
            uid: uid.to_string(),
            sample_rate: 48_000,
            channels: 1,
        }
    }

    #[test]
    fn missing_store_loads_empty_and_round_trips_versioned_state() {
        let path = temp_path("round-trip");
        let store = VirtualInputIntentStore::load(path.clone()).expect("load missing store");
        assert!(store.apps().is_empty());

        let candidate = store.candidate("com.example.app", vec![spec("mic", "example.mic")]);
        store.persist(&candidate).expect("persist candidate");

        let loaded = VirtualInputIntentStore::load(path.clone()).expect("reload store");
        assert_eq!(loaded.apps(), &candidate);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn corrupt_and_unknown_store_versions_fail_hard() {
        let corrupt = temp_path("corrupt");
        fs::write(&corrupt, "not-json").expect("write corrupt store");
        assert!(VirtualInputIntentStore::load(corrupt.clone()).is_err());
        let _ = fs::remove_file(corrupt);

        let unknown = temp_path("unknown");
        fs::write(&unknown, r#"{"version":2,"apps":{}}"#).expect("write unknown store");
        let error = VirtualInputIntentStore::load(unknown.clone()).expect_err("reject version");
        assert!(error.contains("unsupported"), "got: {error}");
        let _ = fs::remove_file(unknown);

        let retired = temp_path("retired-array");
        fs::write(&retired, "[]").expect("write retired overlay shape");
        assert!(
            VirtualInputIntentStore::load(retired.clone()).is_err(),
            "the former raw-array overlay shape must not be accepted"
        );
        let _ = fs::remove_file(retired);
    }

    #[test]
    fn replacement_is_app_scoped_and_empty_input_removes_only_that_app() {
        let path = temp_path("replace");
        let store = VirtualInputIntentStore::load(path.clone()).expect("load store");
        let alpha = store.candidate(
            "com.example.alpha",
            vec![spec("mic", "com.example.alpha.mic")],
        );
        let mut store = VirtualInputIntentStore {
            path: path.clone(),
            apps: alpha,
        };
        let both = store.candidate(
            "com.example.beta",
            vec![spec("mic", "com.example.beta.mic")],
        );
        store.install(both);

        let without_alpha = store.candidate("com.example.alpha", Vec::new());
        assert!(!without_alpha.contains_key("com.example.alpha"));
        assert_eq!(without_alpha["com.example.beta"][0].id, "mic");
    }
}
