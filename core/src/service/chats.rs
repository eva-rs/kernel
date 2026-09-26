service_trait! {
    /// Чаты.
    pub trait Chats(eva_protocol::requests::chats) {
        get, list,
    }
}
