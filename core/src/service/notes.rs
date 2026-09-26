service_trait! {
    /// Блокнот чата, только чтение.
    pub trait Notes(eva_protocol::requests::notes) {
        list,
    }
}
