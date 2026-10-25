//! Тулы вложений используют тот же blob API, что и клиент.

use crate::{
    components::Components,
    shared::Shared,
    subsystems::{tools::schema::input_schema, turns::ports::ToolsetCall},
};
use eva::data;
use eva_core::{CallStep as _, service::Blobs::Trait as _};
use eva_protocol::{
    errors::Aux,
    requests::blobs::{get, put},
    types::{
        BlobId, Bytes,
        blob::Kind,
        tool::{Need, ResultForm, ToolContent, ToolDecl},
    },
    wire::tools::ToolResult,
};
use futures::StreamExt as _;
use serde_json::json;

#[data]
pub struct Read {
    pub id: BlobId,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
}
#[data]
pub struct View {
    pub id: BlobId,
}
#[data]
pub struct Save {
    pub name: String,
    pub content: String,
    pub media_type: Option<String>,
}

pub fn decls() -> Vec<ToolDecl> {
    [
        (
            "files.read",
            "Read a text attachment in line windows.",
            input_schema::<Read>(),
        ),
        (
            "files.view",
            "Show an image attachment to the model.",
            input_schema::<View>(),
        ),
        (
            "files.save",
            "Save text as an attachment in this chat.",
            input_schema::<Save>(),
        ),
    ]
    .into_iter()
    .map(|(path, description, input)| ToolDecl {
        path: path.into(),
        description: description.into(),
        input,
        master_only: true,
        trusted_only: true,
        needs: vec![Need::Private],
        ends_turn: false,
        parallel_safe: path != "files.save",
        waits_on_cancel: false,
        result: ResultForm::Uncapped,
        timeout: None,
        idempotent: false,
    })
    .collect()
}

pub async fn call<C: Components>(
    shared: &Shared<C>,
    call: ToolsetCall<'_>,
) -> Result<ToolResult, Aux> {
    let mut session = crate::session::for_tool(shared, call.ctx.chat);
    if call.canonical == "files_save" {
        let args: Save = match serde_json::from_value(call.input) {
            Ok(args) => args,
            Err(e) => return Ok(ToolResult::error(format!("invalid input: {e}"))),
        };
        let bytes = args.content.into_bytes();
        let size = bytes.len() as u64;
        let args = put::Args {
            meta: put::Meta {
                chat: call.ctx.chat,
                name: args.name,
                media_type: args.media_type.unwrap_or_else(|| "text/plain".into()),
                kind: Kind::File,
                size,
                hash: None,
            },
            body: Box::pin(futures::stream::once(async move {
                Ok(bytes::Bytes::from(bytes))
            })),
        };
        return Ok(match session.blobs().inner_mut().put().call(args).await {
            Ok(result) => ToolResult::ok(vec![
                ToolContent::text(json!({"id": result.id}).to_string()),
                ToolContent::Blob { id: result.id },
            ]),
            Err(error) => ToolResult::error(error.to_string()),
        });
    }
    let args: Read = match serde_json::from_value(call.input) {
        Ok(args) => args,
        Err(e) => return Ok(ToolResult::error(format!("invalid input: {e}"))),
    };
    let mut stream = match session.blobs().get(args.id, 0).await.map(|ok| ok.0) {
        Ok(stream) => stream,
        Err(error) => return Ok(ToolResult::error(error.to_string())),
    };
    let mut bytes = Vec::new();
    let mut media_type = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(get::Item::Meta { media_type: ty, .. }) => media_type = ty,
            Ok(get::Item::Chunk(chunk)) => bytes.extend(chunk.0),
            Err(error) => return Ok(ToolResult::error(error.to_string())),
        }
    }
    if call.canonical == "files_view" {
        return Ok(if media_type.starts_with("image/") {
            ToolResult::ok(vec![ToolContent::Image {
                media_type,
                data: Bytes(bytes),
            }])
        } else {
            ToolResult::error("not an image")
        });
    }
    Ok(match std::str::from_utf8(&bytes) {
        Err(_) => ToolResult::ok(vec![ToolContent::text(format!(
            "binary {media_type}, {} bytes",
            bytes.len()
        ))]),
        Ok(text) => match super::hands::window(
            text,
            args.offset.unwrap_or(1),
            args.limit.unwrap_or(shared.cfg().work.read_default_lines),
            shared.cfg().work.reader_result_tokens,
            call.ratio.unwrap_or(shared.cfg().context.ratio_default),
        ) {
            Ok(text) => ToolResult::ok(vec![ToolContent::text(text)]),
            Err(error) => ToolResult::error(error),
        },
    })
}
