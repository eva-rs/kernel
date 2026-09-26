service_trait! {
    /// Личности и их адреса на поверхностях.
    pub trait Persons(eva_protocol::requests::persons) {
        mut resolve, list, mut rename, mut merge, mut whitelist,
    }
}
