//! Handshake со стороны ядра (`06 §2`, `§3`): токен → caller, зарезервированные
//! имена, разбор capabilities, строка `modules` и вытеснение прежней сессии
//! того же имени. Всё это — до `welcome`: первый же запрос модуля
//! (`surfaces.create`) опирается на строку `modules`.

use eva_core::{Caller, NAMESPACES};
use eva_protocol::{
    types::{
        ModuleId, SessionId,
        module::TokenKind,
        tool::{GroupDecl, ToolDecl},
    },
    wire::{
        Capabilities, ErrorCode, RpcError,
        capabilities::ToolsCap,
        handshake::{Hello, ProtoVersion, Warning, Welcome},
    },
};
use eva_rpc::Authenticator;

use crate::{
    components::Components,
    db::{Db as _, DbResult, Queries as _, stats::Metric},
    handling::modules::{RESERVED, check_name},
    markers, registry,
    shared::Shared,
    state::Policy,
    subsystems::modules::{
        cleanup_superseded,
        live::{Entry, TokenSource},
    },
    world::{Demiurge as _, God},
};

/// Предельная длина каноничного имени тула (`02 §1.1`).
const CANONICAL_MAX: usize = 64;

pub struct KernelAuth<C: Components> {
    shared: Shared<C>,
}

impl<C: Components> KernelAuth<C> {
    pub const fn new(shared: Shared<C>) -> Self {
        Self { shared }
    }

    /// Токен из `modules.tokens.<module>` сильнее выданного тулом: если
    /// конфиг называет модуль, принимается только его токен.
    async fn token(&self, hello: &Hello) -> Result<(TokenKind, TokenSource), RpcError> {
        let presented = blake3::hash(hello.token.expose().as_bytes());
        let refused = || {
            RpcError::new(
                ErrorCode::Unauthorized,
                format_args!("invalid token for module {}", hello.module),
            )
        };

        if let Some(entry) = self.shared.cfg().modules.tokens.get(&hello.module) {
            // Сравнение хэшей постоянно по времени: длина и префикс токена не утекают.
            let expected = blake3::hash(entry.token.expose().as_bytes());
            return (expected == presented)
                .then_some((entry.kind, TokenSource::Config))
                .ok_or_else(refused);
        }

        let hash = presented.as_bytes().to_vec();
        let found = self
            .shared
            .db()
            .reader()
            .await
            .map_err(internal)?
            .run(move |c| c.modules().token(&hash))
            .await
            .map_err(internal)?;
        match found {
            Some((owner, kind)) if owner == hello.module => Ok((kind, TokenSource::Issued)),
            _ => Err(refused()),
        }
    }
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(ErrorCode::Internal, e)
}

impl<C: Components> Authenticator for KernelAuth<C> {
    async fn authenticate(&self, hello: &Hello) -> Result<Welcome, RpcError> {
        let module = &hello.module;
        if RESERVED.contains(&module.as_str()) {
            return Err(RpcError::new(
                ErrorCode::Unauthorized,
                format_args!("module name {module} is reserved"),
            ));
        }
        check_name(module).map_err(RpcError::invalid_params)?;

        let (kind, source) = self.token(hello).await?;
        let caller = Caller::from_token(kind, module.clone());
        let now = self.shared.now();

        let mut warnings = Vec::new();
        let capabilities = vet(
            &self.shared,
            &caller,
            hello.capabilities.clone(),
            &mut warnings,
        );

        let row = registration(hello, &capabilities).map_err(internal)?;
        let (resumed, zone) = (hello.resume.is_some(), self.shared.day_zone());
        self.shared
            .db()
            .write(move |tx| {
                tx.modules()
                    .connected(&row.0, &row.1, &row.2, &row.3, now)?;
                let stats = tx.stats();
                stats.bump(Metric::ModulesConnected, row.0.as_str(), now, zone)?;
                if resumed {
                    stats.bump(Metric::ModulesReconnects, row.0.as_str(), now, zone)?;
                }
                DbResult::Ok(())
            })
            .await
            .map_err(internal)?;

        // Id сессии не из мира ядра: тестовый мир детерминирован по числу сессий.
        let session: SessionId = God.make_world().mint();
        let entry = Entry::new(session.clone(), caller, source, capabilities.clone(), now);
        if let Some(old) = self.shared.live().claim(module.clone(), entry) {
            cleanup_superseded(&self.shared, module, old);
        }

        let models_accepted = accept_models(&self.shared, module, &capabilities, &mut warnings);

        Ok(Welcome {
            session_id: session,
            kernel_version: crate::version(),
            proto_version: ProtoVersion::CURRENT,
            kind,
            services: NAMESPACES.iter().map(|(ns, _)| (*ns).to_owned()).collect(),
            tool_ids: Default::default(),
            models_accepted,
            warnings,
        })
    }
}

/// Строка `modules` сессии: имя, версия, протокол, msgpack принятых capabilities.
pub fn registration(
    hello: &Hello,
    capabilities: &Capabilities,
) -> Result<(ModuleId, String, String, Vec<u8>), rmp_serde::encode::Error> {
    Ok((
        hello.module.clone(),
        hello.version.clone(),
        hello.proto_version.to_string(),
        rmp_serde::to_vec_named(capabilities)?,
    ))
}

/// Отклонённое уходит в `warnings`, сессия живёт (`06 §3`).
fn vet<C: Components>(
    shared: &Shared<C>,
    caller: &Caller,
    mut caps: Capabilities,
    warnings: &mut Vec<Warning>,
) -> Capabilities {
    if caps.provider.is_some()
        && let Err(e) = Policy::new(caller)
            .capabilities()
            .provider(&shared.cfg().llm)
    {
        warnings.push(Warning {
            code: ErrorCode::Forbidden,
            subject: String::from("provider"),
            message: e.to_string(),
        });
        caps.provider = None;
    }

    if let Some(tools) = caps.tools.take() {
        caps.tools = Some(vet_tools(tools, warnings));
    }
    caps
}

fn vet_tools(tools: ToolsCap, warnings: &mut Vec<Warning>) -> ToolsCap {
    let ToolsCap { groups, tools } = tools;
    let mut reject = |subject: &str, message: &str| {
        warnings.push(Warning {
            code: ErrorCode::InvalidParams,
            subject: subject.to_owned(),
            message: message.to_owned(),
        });
    };

    let groups = groups
        .into_iter()
        .filter(|GroupDecl { path, about }| {
            let problem = if !valid_path(path, 1) {
                Some("group path must be dot-separated [a-z][a-z0-9_]* segments")
            } else if !markers::check_inline(about) {
                Some("about must be one line without kernel markers")
            } else {
                None
            };
            problem.inspect(|m| reject(path, m)).is_none()
        })
        .collect();

    let tools = tools
        .into_iter()
        .filter(|tool: &ToolDecl| {
            let problem = if !valid_path(&tool.path, 2) {
                Some("tool path must have at least two dot-separated [a-z][a-z0-9_]* segments")
            } else if tool.path.len() > CANONICAL_MAX {
                Some("canonical tool name exceeds 64 characters")
            } else if !markers::check_inline(&tool.description) {
                Some("description must be one line without kernel markers")
            } else {
                None
            };
            problem.inspect(|m| reject(&tool.path, m)).is_none()
        })
        .collect();

    ToolsCap { groups, tools }
}

fn valid_path(path: &str, min_segments: usize) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    segments.len() >= min_segments
        && segments.iter().all(|s| {
            let mut chars = s.chars();
            matches!(chars.next(), Some('a'..='z'))
                && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
        })
}

/// Модели провайдера, которые реестр примет, когда сессия встанет
/// (`welcome.models_accepted`); прочие — с причиной в `warnings`.
fn accept_models<C: Components>(
    shared: &Shared<C>,
    module: &ModuleId,
    caps: &Capabilities,
    warnings: &mut Vec<Warning>,
) -> Vec<eva_protocol::types::ModelId> {
    let Some(provider) = &caps.provider else {
        return Vec::new();
    };
    let llm = &shared.cfg().llm;
    let models = shared.models();
    let allow = registry::allow_list(llm, module.as_str());

    provider
        .models
        .iter()
        .filter_map(|model| match models.admit(&model.name, allow, llm) {
            Ok(()) => Some(model.name.clone()),
            Err(refusal) => {
                warnings.push(Warning {
                    code: match refusal {
                        registry::Refusal::NotAllowed => ErrorCode::Forbidden,
                        registry::Refusal::Taken { .. } => ErrorCode::Conflict,
                    },
                    subject: format!("model {}", model.name),
                    message: refusal.to_string(),
                });
                None
            }
        })
        .collect()
}
