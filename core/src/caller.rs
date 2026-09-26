//! Кто зовёт операцию. Единственная ось авторизации библиотеки; гейты тулов
//! (актор × trust чата) — другая ось и живут в реестре инструментов.

use eva::data;

use eva_protocol::types::{ModuleId, module::TokenKind};

/// Вызывающий сессию.
///
/// Взломанный мост в мессенджер не должен дотянуться до чатов хозяина и не
/// должен снимать доверие: `Service` работает только с surface, которые
/// создал сам.
#[data]
pub enum Caller {
    /// Само ядро: тулы модели, крон, сон, GC, уведомления.
    Kernel,
    /// Клиент хозяина: телефон, терминал, десктоп. Может всё, что может он.
    Client { module: ModuleId },
    /// Служебный модуль: мост, MCP, удалённый провайдер.
    Service { module: ModuleId },
}

impl Caller {
    /// Caller сессии модуля: единственный бит прав — `kind` его токена.
    pub const fn from_token(kind: TokenKind, module: ModuleId) -> Self {
        match kind {
            TokenKind::Client => Self::Client { module },
            TokenKind::Service => Self::Service { module },
        }
    }

    /// Бит токена для `welcome.kind`; у ядра токена нет.
    pub const fn token_kind(&self) -> Option<TokenKind> {
        match self {
            Self::Kernel => None,
            Self::Client { .. } => Some(TokenKind::Client),
            Self::Service { .. } => Some(TokenKind::Service),
        }
    }

    /// Действует от лица хозяина — ядро или его клиент.
    pub const fn is_owner(&self) -> bool {
        matches!(self, Self::Kernel | Self::Client { .. })
    }

    pub const fn is_kernel(&self) -> bool {
        matches!(self, Self::Kernel)
    }

    /// Имя модуля; у ядра его нет.
    pub const fn module(&self) -> Option<&ModuleId> {
        match self {
            Self::Kernel => None,
            Self::Client { module } | Self::Service { module } => Some(module),
        }
    }

    /// Имя для логов и метрик.
    pub fn label(&self) -> &str {
        match self {
            Self::Kernel => "kernel",
            Self::Client { module } | Self::Service { module } => module.as_str(),
        }
    }
}
