service_trait! {
    /// SQLite-файл модуля.
    pub trait Sqlite(eva_protocol::requests::sqlite) {
        mut exec, query, mut tx,
    }
}
