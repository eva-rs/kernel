//! Конфиг soul: по секции на компонент (`00 §5`). Каждая секция
//! регистрируется в `eva::cli::App` отдельно; `Config` собирает их для
//! `Soul::start`, поэтому тесты строят его литералом без `App`.

use std::path::PathBuf;

use eva::{
    cli::App,
    component_configs::{ComponentConfigs, IsComponentConfig},
    data,
};

pub mod chats;
pub mod context;
pub mod cron;
pub mod db;
pub mod gc;
pub mod hands;
pub mod llm;
pub mod memory;
pub mod modules;
pub mod notes;
pub mod secrets;
pub mod sleep;
pub mod stats;
pub mod system;
pub mod tools;
pub mod wall;
pub mod work;

pub use self::{
    chats::ChatsConfig, context::ContextConfig, cron::CronConfig, db::DbConfig, gc::GcConfig,
    hands::HandsConfig, llm::LlmConfig, memory::MemoryConfig, modules::ModulesConfig,
    notes::NotesConfig, secrets::SecretsConfig, sleep::SleepConfig, stats::StatsConfig,
    system::SystemConfig, tools::ToolsConfig, work::WorkConfig,
};

/// Все секции soul. `llm` без дефолта (`require`), остальные — `optional`.
#[data]
pub struct Config {
    pub system: SystemConfig,
    pub db: DbConfig,
    pub chats: ChatsConfig,
    pub context: ContextConfig,
    pub modules: ModulesConfig,
    pub llm: LlmConfig,
    pub hands: HandsConfig,
    pub tools: ToolsConfig,
    pub work: WorkConfig,
    pub notes: NotesConfig,
    pub secrets: SecretsConfig,
    pub memory: MemoryConfig,
    pub cron: CronConfig,
    pub sleep: SleepConfig,
    pub gc: GcConfig,
    pub stats: StatsConfig,
}

impl Config {
    /// Секции для `eva::cli::App`.
    pub fn register(app: App) -> App {
        app.optional::<SystemConfig>("system")
            .optional::<DbConfig>("db")
            .optional::<ChatsConfig>("chats")
            .optional::<ContextConfig>("context")
            .optional::<ModulesConfig>("modules")
            .require::<LlmConfig>("llm")
            .optional::<HandsConfig>("hands")
            .optional::<ToolsConfig>("tools")
            .optional::<WorkConfig>("work")
            .optional::<NotesConfig>("notes")
            .optional::<SecretsConfig>("secrets")
            .optional::<MemoryConfig>("memory")
            .optional::<CronConfig>("cron")
            .optional::<SleepConfig>("sleep")
            .optional::<GcConfig>("gc")
            .optional::<StatsConfig>("stats")
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
            context: get(cfgs),
            modules: get(cfgs),
            llm: get(cfgs),
            hands: get(cfgs),
            tools: get(cfgs),
            work: get(cfgs),
            notes: get(cfgs),
            secrets: get(cfgs),
            cron: get(cfgs),
            sleep: get(cfgs),
            gc: get(cfgs),
            stats: get(cfgs),
            memory: get(cfgs),
        }
    }

    /// Инварианты секций, которые serde не выразит: отказ старта, не тихий дефолт.
    pub fn check(&self) -> eyre::Result<()> {
        self.chats.check()?;
        self.notes.check()?;
        crate::subsystems::turns::secrets::Scrubber::new(&self.secrets)?;
        self.memory.check()
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
