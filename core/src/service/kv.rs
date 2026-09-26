service_trait! {
    /// KV модуля.
    pub trait Kv(eva_protocol::requests::kv) {
        get, mut set, mut del, list,
    }
}
