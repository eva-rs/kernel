//! Инварианты запроса, которые живой вендор проверил бы 400-й, и формы,
//! которых модель не объявляла (fallback — забота ядра, `01 §3`).

use std::collections::BTreeSet;

use eva_protocol::types::{
    generate::{Block, GenerateRequest, Message, Position, Purpose, ResultBlock},
    model::ModelDecl,
};

pub(crate) fn request(req: &GenerateRequest, model: &ModelDecl) -> Result<(), String> {
    if req.system.iter().all(|s| s.text.trim().is_empty()) {
        return Err("request carries an empty system prompt".into());
    }
    tools(req, model)?;
    if req.messages.is_empty() {
        return Err("request has no messages".into());
    }
    for i in 0..req.messages.len() {
        message(req, model, i)?;
    }
    if matches!(req.messages.last(), Some(Message::Assistant { .. })) {
        return Err("request ends with an assistant message".into());
    }
    breakpoints(req, model)?;
    if req.purpose == Purpose::KeepAlive && !model.caps.cache_keep_alive {
        return Err("keep_alive request for a model without cache_keep_alive".into());
    }
    Ok(())
}

fn tools(req: &GenerateRequest, model: &ModelDecl) -> Result<(), String> {
    let mut names = BTreeSet::new();
    for tool in &req.tools {
        if !names.insert(tool.name.as_str()) {
            return Err(format!("tool {} is declared twice", tool.name));
        }
        if tool.deferred && !model.caps.tool_changes {
            return Err(format!(
                "deferred tool {} for a model without tool_changes",
                tool.name
            ));
        }
    }
    Ok(())
}

fn message(req: &GenerateRequest, model: &ModelDecl, i: usize) -> Result<(), String> {
    let prev = i.checked_sub(1).map(|p| &req.messages[p]);
    let next = req.messages.get(i + 1);
    let caps = &model.caps;

    let blocks = match &req.messages[i] {
        Message::System {
            blocks,
            clear_at,
            output_config,
            tool_changes,
            ..
        } => {
            if !caps.system_in_messages {
                return Err(format!(
                    "system message at {i} for a model without system_in_messages"
                ));
            }
            if i == 0 {
                return Err("system message is the first message".into());
            }
            if matches!(prev, Some(Message::Assistant { .. })) {
                return Err(format!(
                    "system message at {i} right after an assistant message"
                ));
            }
            if matches!(next, Some(Message::User { .. })) {
                return Err(format!("system message at {i} right before a user message"));
            }
            if clear_at.is_some() && !caps.clear_at {
                return Err(format!("clear_at at {i} for a model without clear_at"));
            }
            if output_config.is_some() && !caps.effort_in_messages {
                return Err(format!(
                    "output_config at {i} for a model without effort_in_messages"
                ));
            }
            if tool_changes.is_some() && !caps.tool_changes {
                return Err(format!(
                    "tool_changes at {i} for a model without tool_changes"
                ));
            }
            if blocks.iter().any(|b| !matches!(b, Block::Text { .. })) {
                return Err(format!("system message at {i} carries a non-text block"));
            }
            blocks
        }
        Message::User { blocks } => {
            let declared = match prev {
                Some(Message::Assistant { blocks }) => tool_use_ids(blocks),
                _ => BTreeSet::new(),
            };
            let mut answered = BTreeSet::new();
            for block in blocks {
                match block {
                    Block::ToolResult { tool_use_id, .. } => {
                        if !declared.contains(tool_use_id.as_str()) {
                            return Err(format!(
                                "tool_result {tool_use_id} at {i} answers no tool_use in the immediately preceding message"
                            ));
                        }
                        if !answered.insert(tool_use_id.as_str()) {
                            return Err(format!("tool_result {tool_use_id} at {i} is duplicated"));
                        }
                    }
                    Block::ToolUse { .. } => {
                        return Err(format!("tool_use inside a user message at {i}"));
                    }
                    Block::Thinking { .. } | Block::RedactedThinking { .. } => {
                        return Err(format!("thinking block inside a user message at {i}"));
                    }
                    Block::Text { .. } | Block::Image { .. } => {}
                }
            }
            blocks
        }
        Message::Assistant { blocks } => {
            let answered = match next {
                Some(Message::User { blocks }) => tool_result_ids(blocks),
                _ => BTreeSet::new(),
            };
            let mut seen = BTreeSet::new();
            for block in blocks {
                match block {
                    Block::ToolUse { id, name, .. } => {
                        if !answered.contains(id.as_str()) {
                            return Err(format!(
                                "tool_use {id} ({name}) at {i} has no tool_result in the immediately following message"
                            ));
                        }
                        if !seen.insert(id.as_str()) {
                            return Err(format!("tool_use {id} at {i} is duplicated"));
                        }
                    }
                    Block::ToolResult { .. } => {
                        return Err(format!("tool_result inside an assistant message at {i}"));
                    }
                    Block::Thinking { signature, .. } if signature.is_empty() => {
                        return Err(format!("thinking without a signature is replayed at {i}"));
                    }
                    _ => {}
                }
            }
            blocks
        }
    };

    if !caps.vision && blocks.iter().any(carries_image) {
        return Err(format!("image at {i} for a model without vision"));
    }
    Ok(())
}

fn breakpoints(req: &GenerateRequest, model: &ModelDecl) -> Result<(), String> {
    let limit = usize::from(model.caps.cache_breakpoints);
    if limit > 0 && req.breakpoints.len() > limit {
        return Err(format!(
            "{} breakpoints for a model with {limit}",
            req.breakpoints.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for position in &req.breakpoints {
        if !seen.insert(*position) {
            return Err(format!("breakpoint {position:?} is duplicated"));
        }
        if let Position::Message { index } = *position
            && usize::try_from(index).is_ok_and(|index| index >= req.messages.len())
        {
            return Err(format!("breakpoint at message {index} is out of range"));
        }
    }
    Ok(())
}

fn tool_use_ids(blocks: &[Block]) -> BTreeSet<&str> {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_result_ids(blocks: &[Block]) -> BTreeSet<&str> {
    blocks
        .iter()
        .filter_map(|b| match b {
            Block::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

fn carries_image(block: &Block) -> bool {
    match block {
        Block::Image { .. } => true,
        Block::ToolResult { content, .. } => content
            .iter()
            .any(|c| matches!(c, ResultBlock::Image { .. })),
        _ => false,
    }
}
