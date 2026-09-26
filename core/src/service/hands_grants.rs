service_trait! {
    /// Постоянные разрешения рук.
    pub trait HandsGrants(eva_protocol::requests::hands_grants) {
        list, mut revoke,
    }
}
