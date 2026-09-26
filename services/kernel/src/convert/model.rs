//! Строка реестра моделей (`01 §8.2`) → тип протокола.

use eva_protocol::types::model::ModelEntry;

use crate::registry::Models;

/// Реестр по имени модели.
pub fn from_registry(models: &Models) -> Vec<ModelEntry> {
    models.iter().cloned().collect()
}
