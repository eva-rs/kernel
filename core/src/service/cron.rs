service_trait! {
    /// Крон: джобы и журнал запусков.
    pub trait Cron(eva_protocol::requests::cron) {
        list, runs,
    }
}
