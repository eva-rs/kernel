//! Сценарий одного `generate`.

use serde_json::Value;
use tokio::sync::oneshot;

use eva_core::GenSink;
use eva_protocol::types::generate::{GenItem, ProviderError, Stop, StopReason, Usage};

/// Элементы по порядку, затем успех либо ошибка — ошибка после дельт
/// воспроизводит обрыв посреди ответа.
#[derive(Debug, Default)]
pub struct Script {
    steps: Vec<Step>,
    end: Option<ProviderError>,
}

#[derive(Debug)]
enum Step {
    Item(GenItem),
    Gate(oneshot::Receiver<()>),
}

/// Держит поток сценария, пока тест не откроет; брошенные ворота открыты —
/// тест не обязан дожить до каждого `open`.
#[derive(Debug)]
pub struct Gate(oneshot::Sender<()>);

impl Gate {
    pub fn open(self) {
        _ = self.0.send(());
    }
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// Текст одним куском и `end_turn`.
    pub fn reply(text: impl Into<String>) -> Self {
        Self::new().text(text).stop(StopReason::EndTurn)
    }

    /// Один вызов тула и `tool_use`.
    pub fn call(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Self::new()
            .tool_use(id, name, input)
            .stop(StopReason::ToolUse)
    }

    /// Ошибка без единого элемента.
    pub fn error(error: ProviderError) -> Self {
        Self::new().fail(error)
    }

    pub fn item(mut self, item: GenItem) -> Self {
        self.steps.push(Step::Item(item));
        self
    }

    pub fn text(self, text: impl Into<String>) -> Self {
        self.item(GenItem::TextDelta { text: text.into() })
    }

    pub fn thinking(self, text: impl Into<String>) -> Self {
        self.item(GenItem::ThinkingDelta { text: text.into() })
    }

    pub fn tool_use(self, id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        self.item(GenItem::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        })
    }

    pub fn usage(self, usage: Usage) -> Self {
        self.item(GenItem::Usage(usage))
    }

    pub fn stop(self, reason: StopReason) -> Self {
        self.item(GenItem::Stop(Stop {
            reason,
            details: None,
        }))
    }

    /// Ворота в текущей точке потока.
    pub fn gate(mut self) -> (Self, Gate) {
        let (tx, rx) = oneshot::channel();
        self.steps.push(Step::Gate(rx));
        (self, Gate(tx))
    }

    /// Поток кончается этой ошибкой вместо `Ok`.
    pub fn fail(mut self, error: ProviderError) -> Self {
        self.end = Some(error);
        self
    }

    pub(crate) async fn play(self, sink: &mut GenSink) -> Result<(), ProviderError> {
        for step in self.steps {
            match step {
                Step::Item(item) => {
                    if sink.push(item).await.is_err() {
                        return Ok(());
                    }
                }
                Step::Gate(gate) => _ = gate.await,
            }
        }
        self.end.map_or(Ok(()), Err)
    }
}

impl From<Vec<GenItem>> for Script {
    fn from(items: Vec<GenItem>) -> Self {
        items.into_iter().fold(Self::new(), Self::item)
    }
}

impl From<ProviderError> for Script {
    fn from(error: ProviderError) -> Self {
        Self::error(error)
    }
}
