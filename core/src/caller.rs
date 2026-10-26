//! Кто зовёт операцию. Единственная ось авторизации библиотеки; гейты тулов
//! (актор × trust чата) — другая ось и живут в реестре инструментов.

use eva::data;

use eva_protocol::types::{ModuleId, module::ModuleKind};

/// Вызывающий сессию.
///
/// Взломанный мост в мессенджер не должен дотянуться до чатов хозяина и не
/// должен снимать доверие: `Service` работает только с surface, которые
/// создал сам.
#[data]
pub enum Caller {
    /// Сам soul: тулы модели, крон, сон, GC, уведомления.
    Soul,
    /// Клиент хозяина: телефон, терминал, десктоп. Может всё, что может он.
    Client { module: ModuleId },
    /// Служебный модуль: мост, MCP, удалённый провайдер.
    Service { module: ModuleId },
}

impl Caller {
    /// Caller сессии модуля: единственный бит прав — вид соединения.
    pub const fn from_kind(kind: ModuleKind, module: ModuleId) -> Self {
        match kind {
            ModuleKind::Client => Self::Client { module },
            ModuleKind::Service => Self::Service { module },
        }
    }

    /// Вид модуля для `welcome.kind`; soul не модуль.
    pub const fn kind(&self) -> Option<ModuleKind> {
        match self {
            Self::Soul => None,
            Self::Client { .. } => Some(ModuleKind::Client),
            Self::Service { .. } => Some(ModuleKind::Service),
        }
    }

    /// Действует от лица хозяина — soul или его клиент.
    pub const fn is_owner(&self) -> bool {
        matches!(self, Self::Soul | Self::Client { .. })
    }

    pub const fn is_soul(&self) -> bool {
        matches!(self, Self::Soul)
    }

    /// Имя модуля; у soul его нет.
    pub const fn module(&self) -> Option<&ModuleId> {
        match self {
            Self::Soul => None,
            Self::Client { module } | Self::Service { module } => Some(module),
        }
    }

    /// Имя для логов и метрик.
    pub fn label(&self) -> &str {
        match self {
            Self::Soul => "soul",
            Self::Client { module } | Self::Service { module } => module.as_str(),
        }
    }
}
