//! Handshake со стороны ядра (`06 §2`, `§3`): соединение → id модуля и
//! caller (клиент — по токену, сервис — по записи реестра, которую набрало
//! ядро), разбор capabilities, строка `modules` и вытеснение прежней сессии
//! того же модуля. Всё это — до `welcome`: первый же запрос модуля
//! (`surfaces.create`) опирается на строку `modules`.

use std::time::Duration;

use eva_core::{Caller, NAMESPACES};
use eva_protocol::{
    types::{
        ModuleId, SessionId,
        module::ModuleKind,
        tool::{GroupDecl, ToolDecl},
    },
    wire::{
        Capabilities, ErrorCode, RpcError,
        capabilities::ToolsCap,
        handshake::{Hello, ProtoVersion, Warning, Welcome},
    },
};
use eva_rpc::{Authenticator, Origin};

use crate::{
    components::Components,
    db::{Db as _, DbResult, Queries as _, stats::Metric},
    markers, registry,
    shared::Shared,
    state::Policy,
    subsystems::modules::{
        cleanup_superseded,
        dial::Attempt,
        live::{Entry, Refused, Source},
    },
    world::{Demiurge as _, God},
};

/// Предельная длина каноничного имени тула (`02 §1.1`).
const CANONICAL_MAX: usize = 64;

/// Пауза перед отказом по токену: вместе с лимитом handshake'ей на сеть
/// она держит перебор слабого токена на десятках попыток в секунду.
pub const REFUSED_DELAY: Duration = Duration::from_millis(250);

pub struct KernelAuth<C: Components> {
    shared: Shared<C>,
}

/// Кто пришёл: id модуля, вид, откуда запись и номер отзыва для `claim`.
struct Identity {
    module: ModuleId,
    kind: ModuleKind,
    source: Source,
    /// `None` — идентичность не отзывается (токен конфига).
    epoch: Option<u64>,
}

fn refused() -> RpcError {
    RpcError::new(ErrorCode::Unauthorized, "invalid module token")
}

impl<C: Components> KernelAuth<C> {
    pub const fn new(shared: Shared<C>) -> Self {
        Self { shared }
    }

    /// Запись конфига `modules.tokens`, иначе живая строка `module_tokens`;
    /// выданный токен модулю, которого называет конфиг, не годится.
    async fn identify(&self, presented: &blake3::Hash) -> Result<(ModuleId, Source), RpcError> {
        let tokens = self.shared.tokens();
        if let Some(module) = tokens.find(presented) {
            return Ok((module, Source::ConfigToken));
        }
        match self.issued(presented).await? {
            Some(module) if !self.shared.services().shadowed(&module, tokens) => {
                Ok((module, Source::IssuedToken))
            }
            _ => Err(refused()),
        }
    }

    /// Живой выданный токен по `blake3`: в БД лежит только хэш.
    async fn issued(&self, presented: &blake3::Hash) -> Result<Option<ModuleId>, RpcError> {
        let hash = presented.as_bytes().to_vec();
        self.shared
            .db()
            .reader()
            .await
            .map_err(unverified)?
            .run(move |c| c.modules().token(&hash))
            .await
            .map_err(unverified)
    }

    /// Клиент на слушателе: токен обязателен, UDS от него не освобождает.
    async fn client(&self, hello: &Hello) -> Result<Identity, RpcError> {
        let token = match &hello.token {
            Some(token) if !token.expose().is_empty() => token,
            _ => return Err(refused()),
        };
        let presented = blake3::hash(token.expose().as_bytes());
        let (module, source) = self.identify(&presented).await?;
        let epoch = self.shared.live().epoch(&module);
        // Отзыв между первым чтением и номером отзыва прошёл бы незамеченным:
        // выданный токен перечитывается уже после номера.
        if source == Source::IssuedToken && self.issued(&presented).await?.as_ref() != Some(&module)
        {
            return Err(refused());
        }
        Ok(Identity {
            module,
            kind: ModuleKind::Client,
            source,
            epoch: (source == Source::IssuedToken).then_some(epoch),
        })
    }
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(ErrorCode::Internal, e)
}

/// Сбой проверки токена: пир ещё не опознан, подробность — только в лог.
fn unverified(e: impl std::fmt::Display) -> RpcError {
    log::error!(error:% = e; "module token check failed");
    RpcError::new(ErrorCode::Internal, "internal error")
}

impl<C: Components> Authenticator for KernelAuth<C> {
    type Dialed = Attempt;

    async fn authenticate(
        &self,
        hello: &Hello,
        origin: Origin<'_, Attempt>,
    ) -> Result<Welcome, RpcError> {
        let identity = match origin {
            Origin::Listener => match self.client(hello).await {
                Ok(identity) => identity,
                Err(e) => {
                    tokio::time::sleep(REFUSED_DELAY).await;
                    return Err(e);
                }
            },
            // PSK уже доказан Noise; номер отзыва прочитан до connect.
            Origin::Dialed(attempt) => Identity {
                module: attempt.module.clone(),
                kind: ModuleKind::Service,
                source: attempt.source,
                epoch: Some(attempt.epoch),
            },
        };
        let Identity {
            module,
            kind,
            source,
            epoch,
        } = identity;
        let live = self.shared.live();
        let caller = Caller::from_kind(kind, module.clone());
        let now = self.shared.now();

        let mut warnings = Vec::new();
        let capabilities = vet(
            &self.shared,
            &caller,
            hello.capabilities.clone(),
            &mut warnings,
        );
        let row = registration(&module, hello, &capabilities).map_err(internal)?;

        // Id сессии не из мира ядра: тестовый мир детерминирован по числу сессий.
        let session: SessionId = God.make_world().mint();
        let mut entry = Entry::new(session.clone(), caller, source, capabilities.clone(), now);
        if capabilities.events.is_some() {
            entry = entry.with_events(self.shared.bus().subscribe());
        }
        match live.claim(module.clone(), entry, epoch) {
            Ok(Some(old)) => cleanup_superseded(&self.shared, &module, old),
            Ok(None) => {}
            Err(Refused::Revoked) => {
                let message = match kind {
                    ModuleKind::Client => {
                        format!("token of module {module} was revoked during the handshake")
                    }
                    ModuleKind::Service => {
                        format!("module {module} was removed during the handshake")
                    }
                };
                return Err(RpcError::new(ErrorCode::Unauthorized, message));
            }
            Err(Refused::Closing(reason)) => return Err(reason),
        }

        let (resumed, zone) = (hello.resume.is_some(), self.shared.day_zone());
        let written = self
            .shared
            .db()
            .write(move |tx| {
                tx.modules()
                    .connected(&row.0, &row.1, &row.2, &row.3, now)?;
                let stats = tx.stats();
                stats.bump(Metric::ModulesConnected, row.0.as_str(), now, &zone)?;
                if resumed {
                    stats.bump(Metric::ModulesReconnects, row.0.as_str(), now, &zone)?;
                }
                DbResult::Ok(())
            })
            .await;
        if let Err(e) = written {
            let error = internal(e);
            if let Some(mut entry) = live.release(&module, &session) {
                entry.close(error.clone());
            }
            return Err(error);
        }

        let models_accepted = accept_models(&self.shared, &module, &capabilities, &mut warnings);

        Ok(Welcome {
            session_id: session,
            module,
            kernel_version: crate::version(),
            proto_version: ProtoVersion::CURRENT,
            kind,
            services: NAMESPACES.iter().map(|(ns, _)| (*ns).to_owned()).collect(),
            tool_ids: Default::default(),
            models_accepted,
            warnings,
            // Окно идемпотентности держит транспорт, он и ставит флаг.
            resumed: false,
        })
    }
}

/// Строка `modules` сессии: id, версия, протокол, msgpack принятых capabilities.
pub fn registration(
    module: &ModuleId,
    hello: &Hello,
    capabilities: &Capabilities,
) -> Result<(ModuleId, String, String, Vec<u8>), rmp_serde::encode::Error> {
    Ok((
        module.clone(),
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
