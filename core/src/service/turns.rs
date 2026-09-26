service_trait! {
    /// Туры: старт, лента items, доставка, отмена, ответы на pending.
    pub trait Turns(eva_protocol::requests::turns) {
        mut start, stream attach, mut append_silent, mut delivered, mut cancel, mut respond,
    }
}
