use eva_protocol::types::{Tseq, tool::ResultForm};
use eva_soul::{
    config::ContextConfig,
    subsystems::turns::{dispatch::Window, estimate::Estimator},
};

#[test]
fn windowed_head_tail_and_context_recall_offset_header() {
    let est = Estimator::new(None, &ContextConfig::default());
    let input = (1..=100)
        .map(|n| format!("line {n}: content content content"))
        .collect::<Vec<_>>()
        .join("\n");
    let window = Window {
        form: ResultForm::Windowed,
        max_tokens: 120,
        head_share: 0.7,
        reader_tokens: 100,
    };
    let result = window.apply(&input, Tseq(17), &est);
    assert!(result.starts_with("line 1:"));
    assert!(result.contains("line 100:"));
    assert!(result.contains("t17"));
    assert!(result.contains("omitted"));
    assert!(result.len() < input.len());
}
