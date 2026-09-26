//! Секция `work` (`07 §9`): ключи blob-store, нужные M1.

use std::{path::PathBuf, time::Duration};

use eva::{bytesize::ByteSize, data, time::HumanDuration};

#[data]
#[serde(default)]
pub struct WorkConfig {
    /// `None` — `<state_dir>/blobs`.
    pub blob_dir: Option<PathBuf>,
    pub blob_keep: HumanDuration,
    /// Дисковый лимит одного blob'а, не порог контекста.
    #[schemars(with = "String")]
    pub blob_max: ByteSize,
}

impl Default for WorkConfig {
    fn default() -> Self {
        Self {
            blob_dir: None,
            blob_keep: HumanDuration(Duration::from_secs(30 * 24 * 60 * 60)),
            blob_max: ByteSize::mib(64),
        }
    }
}
