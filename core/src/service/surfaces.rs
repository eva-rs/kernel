service_trait! {
    /// Surface: модуль создаёт своё лицо сам, хозяин настраивает доверие.
    pub trait Surfaces(eva_protocol::requests::surfaces) {
        create, get, list, update,
    }
}
