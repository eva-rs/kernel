//! Группы задают пространство имён, обработчики получают общее состояние модуля.

use std::sync::Arc;

use eva::{_priv::schemars::JsonSchema, data, time::HumanDuration};
use serde::de::DeserializeOwned;

use eva_protocol::{
    types::tool::{GroupDecl, Need, ResultForm, ToolContent, ToolDecl},
    wire::{RpcError, tools::ToolResult},
};

use crate::tool::{self, BoxFuture, ToolCtx, ToolEntry, ToolError};

type Handler<S> = Arc<
    dyn Fn(ToolCtx<S>, serde_json::Value) -> BoxFuture<Result<ToolResult, RpcError>> + Send + Sync,
>;

/// Результат обработчика, который SDK превращает в ответ модели.
pub trait IntoToolResult {
    fn into_tool_result(self) -> ToolResult;
}

impl IntoToolResult for ToolResult {
    fn into_tool_result(self) -> ToolResult {
        self
    }
}

impl IntoToolResult for String {
    fn into_tool_result(self) -> ToolResult {
        ToolResult::ok(vec![ToolContent::text(self)])
    }
}

impl IntoToolResult for &str {
    fn into_tool_result(self) -> ToolResult {
        self.to_owned().into_tool_result()
    }
}

impl IntoToolResult for ToolContent {
    fn into_tool_result(self) -> ToolResult {
        ToolResult::ok(vec![self])
    }
}

impl IntoToolResult for Vec<ToolContent> {
    fn into_tool_result(self) -> ToolResult {
        ToolResult::ok(self)
    }
}

/// Полный набор настроек тула; `text: None` берёт описание из типа входа.
#[data]
#[serde(default)]
pub struct Description {
    pub text: Option<String>,
    pub master_only: bool,
    pub trusted_only: bool,
    pub needs: Vec<Need>,
    pub ends_turn: bool,
    pub parallel_safe: bool,
    pub waits_on_cancel: bool,
    pub result: ResultForm,
    pub timeout: Option<HumanDuration>,
    pub idempotent: bool,
}

impl Default for Description {
    fn default() -> Self {
        Self {
            text: None,
            master_only: false,
            trusted_only: false,
            needs: Vec::new(),
            ends_turn: false,
            parallel_safe: true,
            waits_on_cancel: false,
            result: ResultForm::default(),
            timeout: None,
            idempotent: true,
        }
    }
}

/// Локальное имя, обработчик и настройки одного тула.
pub struct Tool<S = ()> {
    pub(crate) path: String,
    input: serde_json::Value,
    input_description: String,
    description: Description,
    run: Handler<S>,
}

impl<S: Send + Sync + 'static> Tool<S> {
    pub fn new<I, O, E, F, Fut>(name: impl Into<String>, handler: F) -> Self
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: IntoToolResult + Send + 'static,
        E: Into<ToolError> + 'static,
        F: Fn(ToolCtx<S>, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, E>> + Send + 'static,
    {
        let name = name.into();
        check_name(&name);
        Self::at_path(name, handler)
    }

    pub(crate) fn at_path<I, O, E, F, Fut>(path: String, handler: F) -> Self
    where
        I: DeserializeOwned + JsonSchema + Send + 'static,
        O: IntoToolResult + Send + 'static,
        E: Into<ToolError> + 'static,
        F: Fn(ToolCtx<S>, I) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<O, E>> + Send + 'static,
    {
        let mut schema = tool::input_schema::<I>();
        let input_description = schema
            .as_object_mut()
            .and_then(|s| s.remove("description"))
            .and_then(|s| s.as_str().map(str::to_owned))
            .unwrap_or_default();
        let handler = Arc::new(handler);
        let input = schema.clone();
        let run = Arc::new(move |ctx: ToolCtx<S>, mut value: serde_json::Value| {
            tool::coerce(&mut value, &input);
            let parsed = serde_json::from_value::<I>(value);
            let handler = handler.clone();
            Box::pin(async move {
                let input = match parsed {
                    Ok(input) => input,
                    Err(e) => return Ok(ToolResult::error(format!("invalid input: {e}"))),
                };
                match handler(ctx, input).await {
                    Ok(result) => Ok(result.into_tool_result()),
                    Err(error) => error.into().into_outcome(),
                }
            }) as BoxFuture<Result<ToolResult, RpcError>>
        });
        Self {
            path,
            input: schema,
            input_description,
            description: Description::default(),
            run,
        }
    }

    /// Полностью заменяет настройки; имя, схема входа и обработчик сохраняются.
    pub fn describe(mut self, description: Description) -> Self {
        self.description = description;
        self
    }

    fn map_description(mut self, f: impl FnOnce(Description) -> Description) -> Self {
        let description = std::mem::take(&mut self.description);
        self.describe(f(description))
    }

    pub fn description(self, text: impl Into<String>) -> Self {
        self.map_description(|description| Description {
            text: Some(text.into()),
            ..description
        })
    }
    pub fn ends_turn(self, on: bool) -> Self {
        self.map_description(|description| Description {
            ends_turn: on,
            ..description
        })
    }
    pub fn master_only(self, on: bool) -> Self {
        self.map_description(|description| Description {
            master_only: on,
            ..description
        })
    }
    pub fn trusted_only(self, on: bool) -> Self {
        self.map_description(|description| Description {
            trusted_only: on,
            ..description
        })
    }
    pub fn needs(self, needs: &[Need]) -> Self {
        self.map_description(|description| Description {
            needs: needs.to_vec(),
            ..description
        })
    }
    pub fn parallel_safe(self, on: bool) -> Self {
        self.map_description(|description| Description {
            parallel_safe: on,
            ..description
        })
    }
    pub fn idempotent(self, on: bool) -> Self {
        self.map_description(|description| Description {
            idempotent: on,
            ..description
        })
    }
    pub fn waits_on_cancel(self, on: bool) -> Self {
        self.map_description(|description| Description {
            waits_on_cancel: on,
            ..description
        })
    }
    pub fn result(self, result: ResultForm) -> Self {
        self.map_description(|description| Description {
            result,
            ..description
        })
    }
    pub fn timeout(self, timeout: HumanDuration) -> Self {
        self.map_description(|description| Description {
            timeout: Some(timeout),
            ..description
        })
    }

    pub(crate) fn decl(&self) -> ToolDecl {
        let description = &self.description;
        ToolDecl {
            path: self.path.clone(),
            description: description
                .text
                .as_ref()
                .unwrap_or(&self.input_description)
                .clone(),
            input: self.input.clone(),
            master_only: description.master_only,
            trusted_only: description.trusted_only,
            needs: description.needs.clone(),
            ends_turn: description.ends_turn,
            parallel_safe: description.parallel_safe,
            waits_on_cancel: description.waits_on_cancel,
            result: description.result,
            timeout: description.timeout,
            idempotent: description.idempotent,
        }
    }

    pub(crate) fn bind(
        self,
        state: Arc<S>,
        translations: Arc<crate::Catalogs>,
        domain: Arc<str>,
    ) -> ToolEntry {
        let run = self.run;
        ToolEntry {
            run: Arc::new(move |ctx, input| {
                run(
                    ctx.with_state(state.clone())
                        .with_translations(translations.clone(), domain.clone()),
                    input,
                )
            }),
        }
    }
}

/// Группа тулов и вложенных групп; имена её детей локальны.
pub struct Group<S = ()> {
    name: String,
    about: String,
    tools: Vec<Tool<S>>,
    groups: Vec<Group<S>>,
}

impl<S: Send + Sync + 'static> Group<S> {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        check_name(&name);
        Self {
            name,
            about: String::new(),
            tools: Vec::new(),
            groups: Vec::new(),
        }
    }

    pub fn about(mut self, about: impl Into<String>) -> Self {
        self.about = about.into();
        self
    }

    pub fn tool(mut self, tool: Tool<S>) -> Self {
        assert!(
            !self.tools.iter().any(|t| t.path == tool.path),
            "tool {} is declared twice in {}",
            tool.path,
            self.name
        );
        self.tools.push(tool);
        self
    }

    pub fn group(mut self, group: Group<S>) -> Self {
        assert!(
            !self.groups.iter().any(|g| g.name == group.name),
            "group {} is declared twice in {}",
            group.name,
            self.name
        );
        self.groups.push(group);
        self
    }

    pub(crate) fn flatten(
        self,
        parent: &str,
        groups: &mut Vec<GroupDecl>,
        tools: &mut Vec<Tool<S>>,
    ) {
        let path = if parent.is_empty() {
            self.name
        } else {
            format!("{parent}.{}", self.name)
        };
        assert!(
            !groups.iter().any(|g| g.path == path),
            "group {path} is declared twice"
        );
        groups.push(GroupDecl {
            path: path.clone(),
            about: self.about,
        });
        for mut tool in self.tools {
            tool.path = format!("{path}.{}", tool.path);
            tools.push(tool);
        }
        for group in self.groups {
            group.flatten(&path, groups, tools);
        }
    }
}

fn check_name(name: &str) {
    assert!(
        !name.is_empty()
            && name.len() <= 24
            && name.as_bytes()[0].is_ascii_lowercase()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "group and tool names must match [a-z][a-z0-9_]{{0,23}}: {name:?}"
    );
}
