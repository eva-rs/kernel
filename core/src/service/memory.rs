service_trait! {
    /// Память: чтение и восстановление; мутации — только тулы модели.
    pub trait Memory(eva_protocol::requests::memory) {
        list, get, history, mut restore,
    }
}
