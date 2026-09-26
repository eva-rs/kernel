//! Запасной вход провайдера (`06 §4, §6`): те же типы, что у трейта
//! `Provider` — `GenerateRequest`, поток `GenItem`, `EmbedRequest`/`EmbedResponse`.
//! Ошибка — `stream_end{error}`/`response{error}` с `data` = msgpack `ProviderError`.

use eva::data;

/// Ответ `provider.count_tokens`.
#[data(copy)]
pub struct TokenCount {
    pub tokens: u32,
}
