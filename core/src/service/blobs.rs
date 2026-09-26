service_trait! {
    /// Blob-store: загрузка и выдача потоками.
    pub trait Blobs(eva_protocol::requests::blobs) {
        upload put, stream get, stat,
    }
}
