//! Инструменты управления модулями используют библиотечные операции и их политику.

use eva::data;

use eva_protocol::{
    errors::{Aux, Generic},
    types::{
        ModuleId, PersonId,
        module::{Module, State},
        tool::{ResultForm, ToolContent, ToolDecl},
        turn::ActorKind,
    },
    wire::tools::ToolResult,
};
use serde_json::Value;

use crate::{
    components::Components,
    markers::sanitize_inline,
    shared::Shared,
    subsystems::{tools::schema::input_schema, turns::ports::ToolsetCall},
};

#[data]
#[serde(deny_unknown_fields)]
pub struct List {}

#[data]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub module: ModuleId,
}

pub fn decls() -> Vec<ToolDecl> {
    [
        ("system.modules.list", "List modules, connection states, versions, capabilities and service addresses.", input_schema::<List>()),
        ("system.modules.token_issue", "Issue a client token, shown once in this trusted master turn. Rotates previous tokens for this module.", input_schema::<Token>()),
        ("system.modules.token_revoke", "Revoke a module's client tokens and close its connected sessions.", input_schema::<Token>()),
    ].into_iter().map(|(path, description, input)| ToolDecl {
        path: path.into(), description: description.into(), input, master_only: true,
        trusted_only: path == "system.modules.token_issue", needs: vec![], ends_turn: false,
        parallel_safe: path == "system.modules.list", waits_on_cancel: false,
        result: if path == "system.modules.token_issue" { ResultForm::Uncapped } else { ResultForm::Windowed },
        timeout: None, idempotent: path == "system.modules.list",
    }).collect()
}

fn parse<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, Aux> {
    serde_json::from_value(input).map_err(|e| Aux::Invalid(format!("invalid input: {e}")))
}

fn api_error<E: std::fmt::Display>(error: Generic<E>) -> Aux {
    match error {
        Generic::Aux(error) => error,
        Generic::Spec(error) => Aux::Invalid(error.to_string()),
    }
}

pub async fn call<C: Components>(
    shared: &Shared<C>,
    call: ToolsetCall<'_>,
) -> Result<ToolResult, Aux> {
    let result = async {
        if !call.view.master_turn {
            return Err(Aux::forbidden("module tools require a master turn"));
        }
        let mut session = crate::session::internal(shared);
        match call.canonical {
            "system_modules_list" => {
                let _: List = parse(call.input)?;
                let result = session.modules().list().await.map_err(api_error)?;
                Ok(render(result.modules))
            }
            "system_modules_token_issue" => {
                if !call.view.trusted_chat {
                    return Err(Aux::forbidden(
                        "client tokens require a trusted master turn",
                    ));
                }
                let actor = call
                    .view
                    .actor
                    .as_ref()
                    .filter(|actor| **actor != PersonId::system())
                    .filter(|_| call.ctx.actor.kind == ActorKind::Master)
                    .ok_or_else(|| Aux::forbidden("a human master must issue client tokens"))?;
                let args: Token = parse(call.input)?;
                let module = args.module.clone();
                let result = session
                    .modules()
                    .token_issue(args.module, Some(actor.clone()))
                    .await
                    .map_err(api_error)?;
                // Секрет намеренно выдан хозяину; форма не похожа на случайную утечку присваивания.
                Ok(format!(
                    "Client token for module {module} (shown once)\n{}",
                    result.token
                ))
            }
            "system_modules_token_revoke" => {
                let args: Token = parse(call.input)?;
                session
                    .modules()
                    .token_revoke(args.module)
                    .await
                    .map_err(api_error)?;
                Ok("{}".into())
            }
            _ => Err(Aux::Internal(format!(
                "unknown module tool {}",
                call.canonical
            ))),
        }
    }
    .await;
    match result {
        Ok(body) => Ok(ToolResult::ok(vec![ToolContent::text(body)])),
        Err(error @ (Aux::Db(_) | Aux::Internal(_))) => Err(error),
        Err(error) => Ok(ToolResult::error(error.to_string())),
    }
}

fn render(modules: Vec<Module>) -> String {
    if modules.is_empty() {
        return "no modules".into();
    }
    let mut text =
        String::from("name | kind | state | version | since | capabilities | address | error\n");
    for module in modules {
        let (address, error) = module
            .dial
            .map(|dial| (dial.address.to_string(), dial.error.unwrap_or_default()))
            .unwrap_or_default();
        text.push_str(&format!(
            "{} | {} | {} | {} | {} | {} | {} | {}\n",
            sanitize_inline(module.name.as_str()),
            module
                .kind
                .map(|kind| kind.to_string())
                .unwrap_or_else(|| "none".into()),
            match module.state {
                State::Connected => "connected",
                State::Offline => "offline",
                State::Forgotten => "forgotten",
            },
            sanitize_inline(&module.version),
            module.since,
            sanitize_inline(&module.capabilities.join(", ")),
            sanitize_inline(&address),
            sanitize_inline(&error)
        ));
    }
    text
}
