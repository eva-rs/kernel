//! Конфиг ядра: по секции на компонент (`00 §5`). Каждая секция
//! регистрируется в `eva::cli::App` отдельно; `Config` собирает их для
//! `Kernel::start`, поэтому тесты строят его литералом без `App`.

use std::path::PathBuf;

use eva::{
    cli::App,
    component_configs::{ComponentConfigs, IsComponentConfig},
    data,
};

pub mod chats;
pub mod db;
pub mod hands;
pub mod llm;
pub mod modules;
pub mod notes;
pub mod system;
pub mod work;

pub use self::{
    chats::ChatsConfig, db::DbConfig, hands::HandsConfig, llm::LlmConfig, modules::ModulesConfig,
    notes::NotesConfig, system::SystemConfig, work::WorkConfig,
};

/// Все секции ядра. `llm` без дефолта (`require`), остальные — `optional`.
#[data]
pub struct Config {
    pub system: SystemConfig,
    pub db: DbConfig,
    pub chats: ChatsConfig,
    pub modules: ModulesConfig,
    pub llm: LlmConfig,
    pub hands: HandsConfig,
    pub work: WorkConfig,
    pub notes: NotesConfig,
}

impl Config {
    /// Секции для `eva::cli::App`.
    pub fn register(app: App) -> App {
        app.optional::<SystemConfig>("system")
            .optional::<DbConfig>("db")
            .optional::<ChatsConfig>("chats")
            .optional::<ModulesConfig>("modules")
            .require::<LlmConfig>("llm")
            .optional::<HandsConfig>("hands")
            .optional::<WorkConfig>("work")
            .optional::<NotesConfig>("notes")
    }

    /// Снимок секций, зарегистрированных `register`.
    pub fn from_configs(cfgs: &ComponentConfigs) -> Self {
        fn get<T: IsComponentConfig>(cfgs: &ComponentConfigs) -> T {
            T::clone(&cfgs.get::<T>())
        }

        Self {
            system: get(cfgs),
            db: get(cfgs),
            chats: get(cfgs),
            modules: get(cfgs),
            llm: get(cfgs),
            hands: get(cfgs),
            work: get(cfgs),
            notes: get(cfgs),
        }
    }

    /// Файл БД: `db.path`, иначе `<state_dir>/eva.sqlite`.
    pub fn db_path(&self) -> PathBuf {
        self.db
            .path
            .clone()
            .unwrap_or_else(|| self.system.state_dir.join("eva.sqlite"))
    }

    /// Каталог blob'ов: `work.blob_dir`, иначе `<state_dir>/blobs`.
    pub fn blob_dir(&self) -> PathBuf {
        self.work
            .blob_dir
            .clone()
            .unwrap_or_else(|| self.system.state_dir.join("blobs"))
    }
}
