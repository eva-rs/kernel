//! Токены клиентов `modules.tokens` (`06 §2`, `§11`): секреты читаются один
//! раз на старте, в памяти остаётся только `blake3`. Имя записи — id модуля.

use std::collections::BTreeMap;

use eyre::bail;

use eva_protocol::types::ModuleId;

use crate::{
    config::modules::TokenEntry,
    handling::modules::{RESERVED, check_name},
};

/// Токен конфига короче — перебираем по сети за обозримое время.
const WEAK_TOKEN: usize = 32;

#[derive(Debug, Clone)]
struct Entry {
    module: ModuleId,
    hash: blake3::Hash,
}

/// Записи `modules.tokens` без секретов.
#[derive(Debug, Clone, Default)]
pub struct ConfigTokens(Vec<Entry>);

impl ConfigTokens {
    /// Ключ `kind`, нечитаемый или пустой секрет, зарезервированное или
    /// невалидное имя и один токен у двух записей — ошибка старта с путём ключа.
    pub fn load(tokens: &BTreeMap<ModuleId, TokenEntry>) -> eyre::Result<Self> {
        let mut entries: Vec<Entry> = Vec::with_capacity(tokens.len());
        for (module, entry) in tokens {
            if RESERVED.contains(&module.as_str()) {
                bail!("modules.tokens.{module}: {module} is a reserved module id");
            }
            if let Err(e) = check_name(module) {
                bail!("modules.tokens.{module}: {e}");
            }
            if entry.kind {
                bail!(
                    "modules.tokens.{module}.kind: a token entry is always a client and takes no kind; a service module goes to modules.services.{module} = {{address, key}}"
                );
            }
            let read = entry
                .token
                .read()
                .map_err(|e| eyre::eyre!("modules.tokens.{module}.token: {e:#}"))?;
            // файл секрета обычно кончается переводом строки, в токен он не входит
            let token = read.trim_end();
            // Пустой токен пускал бы любого с пустым `hello.token`.
            if token.is_empty() {
                bail!("modules.tokens.{module}.token is empty");
            }
            if token.len() < WEAK_TOKEN {
                log::warn!(module:% = module, len = token.len(), min = WEAK_TOKEN; "modules.tokens token is short and guessable");
            }

            let hash = blake3::hash(token.as_bytes());
            // Идентичность — из токена: у двух записей с одним токеном её не выбрать.
            if let Some(twin) = entries.iter().find(|e| e.hash == hash) {
                bail!(
                    "modules.tokens.{} and modules.tokens.{module} share one token",
                    twin.module
                );
            }
            entries.push(Entry {
                module: module.clone(),
                hash,
            });
        }
        Ok(Self(entries))
    }

    /// Запись, чей токен предъявлен; сравниваются все записи без раннего
    /// выхода, `blake3::Hash` сравнивается за постоянное время.
    pub fn find(&self, presented: &blake3::Hash) -> Option<ModuleId> {
        self.0.iter().fold(None, |found, entry| {
            let matches = entry.hash == *presented;
            match found {
                None if matches => Some(entry.module.clone()),
                found => found,
            }
        })
    }

    /// Конфиг называет модуль: выданный токен ему не годится.
    pub fn configures(&self, module: &ModuleId) -> bool {
        self.0.iter().any(|entry| entry.module == *module)
    }
}
