use eva::time::Timestamp;
use eva_kernel::{
    db::agents::PlanRow,
    subsystems::turns::plan::{self, NewTask, Op},
};
use eva_protocol::types::turn::TaskStatus;
use proptest::prelude::*;

proptest! {
    #[test]
    fn active_bounds_property(states in prop::collection::vec(0u8..4,0..80),agents in 0u32..8) {
        let mut rows:Vec<_>=states.iter().enumerate().map(|(i,s)|PlanRow {task_id:i as u32,position:i as u32,phase:None,title:format!("task {i}"),status:match s {0=>TaskStatus::Pending,1=>TaskStatus::InProgress,2=>TaskStatus::Completed,_=>TaskStatus::Abandoned},outcome:None,updated_at:Timestamp::TEST_ORIGIN}).collect();
        let open=rows.iter().any(|r|matches!(r.status,TaskStatus::Pending|TaskStatus::InProgress));
        plan::normalize(&mut rows,agents,None,Timestamp::TEST_ORIGIN);
        let active=rows.iter().filter(|r|r.status==TaskStatus::InProgress).count();
        prop_assert!(active<=1+agents as usize);
        prop_assert_eq!(active>0,open);
        if agents==0&&open {prop_assert_eq!(active,1);}
    }
}

#[test]
fn rejection_leaves_rows_untouched() {
    let rows = plan::apply(
        &[],
        &Op::Init {
            tasks: vec![NewTask {
                title: "first".into(),
                phase: None,
            }],
        },
        Timestamp::TEST_ORIGIN,
        0,
    )
    .unwrap();
    let before = rows.clone();
    assert!(plan::apply(&rows, &Op::Start { id: 999 }, Timestamp::TEST_ORIGIN, 0).is_err());
    assert_eq!(rows, before);
}
