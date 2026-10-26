//! Подагент (`07 §2`) → тип протокола.

use eva_protocol::types::agent::Agent;

use crate::db::agents::AgentRow;

pub fn from_db(row: AgentRow) -> Agent {
    let AgentRow {
        chat,
        parent,
        name,
        task,
        tools,
        report_schema,
        master_turn,
        status,
        unread,
        started_at,
        finished_at,
    } = row;

    Agent {
        chat,
        parent,
        name,
        task,
        tools,
        report_schema,
        master_turn,
        status,
        unread,
        started_at,
        finished_at,
    }
}
