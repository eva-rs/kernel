service_trait! {
    /// Разовый вызов модели модулем.
    pub trait Inference(eva_protocol::requests::inference) {
        mut complete,
    }
}
