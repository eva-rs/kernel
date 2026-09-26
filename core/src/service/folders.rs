service_trait! {
    /// Папки хозяина.
    pub trait Folders(eva_protocol::requests::folders) {
        mut create, mut update, mut delete, list,
    }
}
