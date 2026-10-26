//! Сервис с типизированным состоянием; секция module использует дефолты SDK.

use eva::data;
use eva_sdk::{Group, Tool, ToolCtx, ToolError, include_translations, module, msg};

#[data]
struct EchoConfig {
    prefix: String,
}

fn main() -> eyre::Result<()> {
    module!()
        .name("echo")
        .init("echo", async |cfg: EchoConfig| Ok(cfg.prefix))
        .translations(include_translations!("examples/locales"))
        .group(groups::echo())
        .run()
}

mod groups {
    use super::*;

    pub fn echo() -> Group<String> {
        Group::new("echo").about("Echo tools").tool(
            Tool::new("say", async |ctx: ToolCtx<String>, input: SayInput| {
                ctx.progress(msg!("Echoing {text}", text = &input.text))
                    .await;
                Ok::<_, ToolError>(format!("{}{}", ctx.state(), input.text))
            })
            .description("Repeat the given text"),
        )
    }

    #[data]
    struct SayInput {
        /// Text to repeat.
        text: String,
    }
}
