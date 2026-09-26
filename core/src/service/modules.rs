service_trait! {
    /// Модули и их токены.
    pub trait Modules(eva_protocol::requests::modules) {
        list, mut token_issue, mut token_revoke,
    }
}
