service_trait! {
    /// Статистика, только чтение.
    pub trait Stats(eva_protocol::requests::stats) {
        tools_usage, spend, spend_series, chat_spend, health,
    }
}
