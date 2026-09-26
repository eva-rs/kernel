service_trait! {
    /// Файлы модуля в его каталоге.
    pub trait Fs(eva_protocol::requests::fs) {
        read, mut write, list, stat, mut remove,
    }
}
