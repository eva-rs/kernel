//! Vendor-форма запроса каждого встроенного адаптера генерации: тест I1 soul
//! (`01 §12`) сравнивает префиксы, не называя вендоров (I14).

#![cfg_attr(not(any(feature = "openai", feature = "mock")), allow(unused_imports))]

use serde_json::Value;

use eva_protocol::types::{
    generate::GenerateRequest,
    model::{ModelCaps, ModelDecl},
};

pub struct Adapter {
    pub name: &'static str,
    /// Гасит capabilities, которых синтаксис адаптера не выражает.
    pub caps: fn(ModelCaps) -> ModelCaps,
    /// Части запроса в порядке кэша вендора: префикс запроса — префикс частей.
    pub parts: fn(&GenerateRequest, &ModelDecl) -> Result<Vec<Value>, String>,
}

pub fn adapters() -> Vec<Adapter> {
    #[allow(unused_mut)]
    let mut out = Vec::new();
    #[cfg(feature = "openai")]
    out.extend([
        Adapter {
            name: "openai/openrouter",
            caps: chat::caps,
            parts: |req, model| chat::parts(req, model, crate::openai::Dialect::Openrouter),
        },
        Adapter {
            name: "openai/plain",
            caps: chat::caps,
            parts: |req, model| chat::parts(req, model, crate::openai::Dialect::Plain),
        },
    ]);
    #[cfg(feature = "mock")]
    out.push(Adapter {
        name: "mock",
        caps: |caps| caps,
        parts: mock_parts,
    });
    out
}

#[cfg(feature = "openai")]
mod chat {
    use super::*;
    use crate::openai::{Dialect, render};

    /// Chat Completions не знает `clear_at`, `output_config`, `tool_changes` и keep-alive.
    pub fn caps(caps: ModelCaps) -> ModelCaps {
        ModelCaps {
            clear_at: false,
            effort_in_messages: false,
            tool_changes: false,
            cache_keep_alive: false,
            ..caps
        }
    }

    pub fn parts(
        req: &GenerateRequest,
        model: &ModelDecl,
        dialect: Dialect,
    ) -> Result<Vec<Value>, String> {
        let body = render(req, model, dialect).map_err(|e| e.to_string())?;
        let array = |key: &str| body[key].as_array().cloned().unwrap_or_default();
        let mut out = array("tools");
        out.extend(array("messages"));
        Ok(out)
    }
}

/// Мок шлёт нормализованный запрос как есть, но сперва проверяет его, как проверил бы вендор.
#[cfg(feature = "mock")]
fn mock_parts(req: &GenerateRequest, model: &ModelDecl) -> Result<Vec<Value>, String> {
    crate::mock::validate::request(req, model)?;
    let value = |v: Result<Value, serde_json::Error>| v.map_err(|e| e.to_string());
    let mut out = Vec::new();
    for tool in &req.tools {
        out.push(value(serde_json::to_value(tool))?);
    }
    for section in &req.system {
        out.push(value(serde_json::to_value(section))?);
    }
    for message in &req.messages {
        out.push(value(serde_json::to_value(message))?);
    }
    Ok(out)
}
