//! # The durable migration store (P4b plan §3)
//!
//! One JSON file per [`MigrationId`] (`{dir}/{migration_id}.json`),
//! atomically saved with the house `DrbdState` discipline: serialize →
//! write a sibling temp file (owner-only `0600`) → fsync the file →
//! rename over the target → fsync the directory. A crash can never
//! leave a torn or half-renamed record; a leftover `.tmp` file is
//! ignored on load (it is the discarded half of an interrupted save).
//!
//! The store is deliberately dumb: it persists exactly the record it
//! is handed and never invents timestamps, state transitions or
//! history entries — those belong to the coordinator (the single
//! writer through the append-only transition helper). Corrupt or
//! unparseable record files are a **typed startup error**, never
//! silently dropped: a migration record is authority-relevant state
//! and guessing it is forbidden (AGENTS rule 8's fail-closed reading).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use volvisor_types::id::MigrationId;
use volvisor_types::{ApiError, ApiErrorCode};

use crate::types::MigrationRecord;

/// The per-migration record store: an in-memory index over one
/// directory of atomically-saved JSON records.
///
/// Not internally synchronized — the coordinator owns it behind a
/// mutex and every mutation is a full-record upsert, so there is no
/// torn in-memory state to defend against.
#[derive(Debug)]
pub struct MigrationStore {
    /// The store directory (one `{migration_id}.json` file per record).
    dir: PathBuf,
    /// The loaded index, keyed by migration identity.
    records: BTreeMap<MigrationId, MigrationRecord>,
    /// The store-save crash seam (P5 plan §3.1): inert by default,
    /// armed only by the constructing test rig through
    /// [`Self::store_crash_hooks`].
    crash: std::sync::Arc<volvisor_types::crash::StoreCrashHooks>,
}

impl MigrationStore {
    /// Open the store at `dir`, loading every record file into the
    /// index. The directory is created if missing (first start).
    ///
    /// # Errors
    /// `INTERNAL` when the directory cannot be created or read, or
    /// when any `*.json` record file fails to parse, carries an
    /// unknown field, or disagrees with its file name — a corrupt
    /// record is never silently dropped.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, ApiError> {
        let dir = dir.into();
        let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
        fs::create_dir_all(&dir).map_err(|e| {
            internal(format!(
                "failed to create migration store {}: {e}",
                dir.display()
            ))
        })?;
        let mut records = BTreeMap::new();
        let entries = fs::read_dir(&dir).map_err(|e| {
            internal(format!(
                "failed to read migration store {}: {e}",
                dir.display()
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| {
                internal(format!(
                    "failed to read migration store entry in {}: {e}",
                    dir.display()
                ))
            })?;
            let path = entry.path();
            // Only finished records are loaded; a leftover `.tmp` is the
            // discarded half of an interrupted atomic save.
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let record = Self::load_record(&path)?;
            let file_id = MigrationId::new(
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or_default(),
            )
            .map_err(|e| {
                internal(format!(
                    "migration record file {} has an invalid identity: {e}",
                    path.display()
                ))
            })?;
            if record.migration_id != file_id {
                return Err(internal(format!(
                    "migration record file {} holds a record for {} (name/record mismatch)",
                    path.display(),
                    record.migration_id
                )));
            }
            records.insert(file_id, record);
        }
        Ok(Self {
            dir,
            records,
            crash: std::sync::Arc::new(volvisor_types::crash::StoreCrashHooks::new()),
        })
    }

    /// Load one record file, failing typed on any parse or validation
    /// error (deny_unknown_fields is part of the record's serde shape).
    fn load_record(path: &Path) -> Result<MigrationRecord, ApiError> {
        let path_display = path.display();
        let text = fs::read_to_string(path).map_err(|e| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("failed to read migration record {path_display}: {e}"),
            )
        })?;
        serde_json::from_str(&text).map_err(|e| {
            ApiError::new(
                ApiErrorCode::Internal,
                format!("failed to parse migration record {path_display}: {e}"),
            )
        })
    }

    /// All stored records, ordered by migration identity. Terminal
    /// records are retained (pruning by age is out of scope).
    #[must_use]
    pub fn load_all(&self) -> Vec<MigrationRecord> {
        self.records.values().cloned().collect()
    }

    /// Look up one record.
    #[must_use]
    pub fn get(&self, migration_id: &MigrationId) -> Option<MigrationRecord> {
        self.records.get(migration_id).cloned()
    }

    /// Insert or replace one record, persisting it atomically.
    ///
    /// The store is dumb: `updated_at` and the history are the
    /// coordinator's responsibility; this method persists exactly the
    /// record it is handed.
    ///
    /// # Errors
    /// `INTERNAL` when the atomic save (serialize, tmp-write, fsync,
    /// rename, directory fsync) fails; the previous record file
    /// remains intact.
    pub fn upsert(&mut self, record: &MigrationRecord) -> Result<(), ApiError> {
        let path = self.record_path(&record.migration_id);
        save_record_atomic(&path, record, &self.crash)?;
        self.records
            .insert(record.migration_id.clone(), record.clone());
        Ok(())
    }

    /// Remove one record and its file. Removing an absent record is a
    /// no-op.
    ///
    /// # Errors
    /// `INTERNAL` when the file cannot be removed (other than it
    /// already being absent).
    pub fn remove(
        &mut self,
        migration_id: &MigrationId,
    ) -> Result<Option<MigrationRecord>, ApiError> {
        let path = self.record_path(migration_id);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(ApiError::new(
                    ApiErrorCode::Internal,
                    format!("failed to remove migration record {}: {e}", path.display()),
                ));
            }
        }
        Ok(self.records.remove(migration_id))
    }

    /// The store directory (diagnostics and tests).
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The store-save crash seam (P5 plan §3.1): the armed table a
    /// test rig aims and the kill switch fires into. Inert unless a
    /// rig arms it; no route or input reaches it.
    #[must_use]
    pub fn store_crash_hooks(&self) -> &std::sync::Arc<volvisor_types::crash::StoreCrashHooks> {
        &self.crash
    }

    fn record_path(&self, migration_id: &MigrationId) -> PathBuf {
        self.dir.join(format!("{migration_id}.json"))
    }
}

/// Persist `record` at `path` atomically: serialize → write
/// `<path>.tmp` (owner-only `0600` on unix) → fsync → rename over
/// `path` → fsync the parent directory. If any step fails, the temp
/// file is removed (best effort) and the previous file remains
/// intact. The `crash` seam (P5 plan §3.1) can terminate the saving
/// task at either side of the fsync/rename commit boundaries — the
/// store-save windows a real process death lands in.
fn save_record_atomic(
    path: &Path,
    record: &MigrationRecord,
    crash: &volvisor_types::crash::StoreCrashHooks,
) -> Result<(), ApiError> {
    let tmp_path = sibling_tmp_path(path);
    let result = save_record_to(&tmp_path, path, record, crash);
    if result.is_err() {
        // Best-effort cleanup: never leave a stale .tmp behind.
        drop(fs::remove_file(&tmp_path));
    }
    result
}

fn save_record_to(
    tmp_path: &Path,
    path: &Path,
    record: &MigrationRecord,
    crash: &volvisor_types::crash::StoreCrashHooks,
) -> Result<(), ApiError> {
    let internal = |detail: String| ApiError::new(ApiErrorCode::Internal, detail);
    let tmp_display = tmp_path.display();
    let path_display = path.display();
    let data = serde_json::to_vec_pretty(record)
        .map_err(|e| internal(format!("failed to serialize migration record: {e}")))?;
    let mut file = create_owner_only(tmp_path)
        .map_err(|e| internal(format!("failed to create {tmp_display}: {e}")))?;
    file.write_all(&data)
        .map_err(|e| internal(format!("failed to write {tmp_display}: {e}")))?;
    // The store-save crash points (P5 plan §3.1): after the tmp
    // content write, after its fsync, after the rename. Inert unless
    // the rig armed this store's seam.
    crash.consult(
        volvisor_types::crash::STORE_MIGRATION_RECORDS,
        volvisor_types::crash::StoreSavePoint::AfterTmpWrite,
    );
    file.sync_all()
        .map_err(|e| internal(format!("failed to fsync {tmp_display}: {e}")))?;
    drop(file);
    crash.consult(
        volvisor_types::crash::STORE_MIGRATION_RECORDS,
        volvisor_types::crash::StoreSavePoint::AfterFsyncBeforeRename,
    );
    fs::rename(tmp_path, path).map_err(|e| {
        internal(format!(
            "failed to rename {tmp_display} to {path_display}: {e}"
        ))
    })?;
    crash.consult(
        volvisor_types::crash::STORE_MIGRATION_RECORDS,
        volvisor_types::crash::StoreSavePoint::AfterRename,
    );
    // fsync the directory so the rename itself is durable.
    let dir = fs::File::open(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )
    .map_err(|e| internal(format!("failed to open parent of {path_display}: {e}")))?;
    dir.sync_all()
        .map_err(|e| internal(format!("failed to fsync parent of {path_display}: {e}")))?;
    Ok(())
}

/// The `<path>.tmp` sibling used for atomic saves.
fn sibling_tmp_path(path: &Path) -> PathBuf {
    let mut os_name = path.as_os_str().to_owned();
    os_name.push(".tmp");
    PathBuf::from(os_name)
}

/// Create (or truncate) `path` for writing with owner-only
/// permissions (`0600` on unix; the platform default elsewhere).
fn create_owner_only(path: &Path) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        fs::File::create(path)
    }
}
