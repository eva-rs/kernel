service_trait! {
    /// Чаты: вход речи с поверхности, жизненный цикл, поиск, история.
    pub trait Chats(eva_protocol::requests::chats) {
        mut ingest, mut create, get, list, mut update, mut r#move, mut delete, mut restore,
        search, history_read, mut compact, mut notify,
    }
}
