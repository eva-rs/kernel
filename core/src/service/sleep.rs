service_trait! {
    /// Сон: журнал ночей.
    pub trait Sleep(eva_protocol::requests::sleep) {
        runs, phase_runs,
    }
}
