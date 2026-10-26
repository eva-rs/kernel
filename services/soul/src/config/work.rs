//! Секция `work` (`07 §9`): лимиты шагов, pending, читатели, план, blob-store.

use std::{path::PathBuf, time::Duration};

use eva::{bytesize::ByteSize, data, time::HumanDuration};

#[data]
#[serde(default)]
pub struct WorkConfig {
    pub max_agents: u32,
    pub agent_default_tools: Vec<String>,
    pub wait_default: HumanDuration,
    pub wait_max: HumanDuration,
    /// Потолок шагов тура.
    pub max_steps: u32,
    /// Потолок в trusted master-туре (`master_turn ∧ trusted_chat`).
    pub max_steps_hard: u32,
    /// За столько шагов до потолка reminder несёт `[wrapup]`.
    pub wrapup_steps: u32,
    /// Ожидание ответа на `chat_ask`.
    pub ask_ttl: HumanDuration,
    /// Ожидание ответа на pending `permission`.
    pub permission_ttl: HumanDuration,
    pub fs_timeout: HumanDuration,
    pub shell_timeout: HumanDuration,
    pub shell_timeout_max: HumanDuration,
    /// Окно читателей по умолчанию, строк.
    pub read_default_lines: u32,
    /// Хвост результата тулов с собственным окном (`result: uncapped`).
    pub reader_result_tokens: u32,
    pub plan_max_tasks: u32,
    /// Рендер плана в токенах.
    pub plan_budget: u32,
    /// Вставка писем подагентов в лог тимлида за раз, токенов.
    pub inbox_budget: u32,
    /// `None` — `<state_dir>/blobs`.
    pub blob_dir: Option<PathBuf>,
    pub blob_keep: HumanDuration,
    /// Дисковый лимит одного blob'а, не порог контекста.
    #[schemars(with = "String")]
    pub blob_max: ByteSize,
    /// Живые blob'ы чатов одного владельца surface, каждый не легче 4 KiB; surface без модуля делят одну квоту.
    #[schemars(with = "String")]
    pub blob_quota: ByteSize,
}

impl Default for WorkConfig {
    fn default() -> Self {
        Self {
            max_agents: 4,
            agent_default_tools: [
                "fs_read",
                "fs_find",
                "plan_update",
                "agent_say",
                "agent_done",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            wait_default: secs(2 * 60),
            wait_max: secs(15 * 60),
            max_steps: 16,
            max_steps_hard: 200,
            wrapup_steps: 2,
            ask_ttl: secs(5 * 60),
            permission_ttl: secs(5 * 60),
            fs_timeout: secs(2 * 60),
            shell_timeout: secs(60),
            shell_timeout_max: secs(15 * 60),
            read_default_lines: 200,
            reader_result_tokens: 4000,
            plan_max_tasks: 40,
            plan_budget: 600,
            inbox_budget: 2000,
            blob_dir: None,
            blob_keep: secs(30 * 24 * 60 * 60),
            blob_max: ByteSize::mib(64),
            blob_quota: ByteSize::gib(4),
        }
    }
}

const fn secs(n: u64) -> HumanDuration {
    HumanDuration(Duration::from_secs(n))
}
