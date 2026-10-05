//! Blob-store глазами тура (`07 §6`): байты и метаданные для рендера,
//! картинки тулов и модели — в `blobs` до записи события.

use eva_core::{CallStep as _, service::blobs::Blobs as _};
use std::collections::BTreeMap;

use eva_protocol::{
    errors::Aux,
    types::{BlobId, Bytes, ChatId, blob::Kind, content::Block as Content},
};

use crate::{
    components::Components,
    db::{Db as _, DbResult, Queries as _, blobs::Source},
    handling::blobs::file,
    shared::Shared,
    subsystems::turns::{
        log::{Event, Payload, ResultContent},
        render::{BlobMeta, Blobs},
    },
};

/// Blob'ы одного рендера, прочитанные заранее: рендер чистый.
#[derive(Default)]
pub struct Loaded(BTreeMap<BlobId, (BlobMeta, Option<Bytes>)>);

impl Blobs for Loaded {
    fn meta(&self, id: &BlobId) -> Option<BlobMeta> {
        self.0.get(id).map(|(meta, _)| meta.clone())
    }

    fn bytes(&self, id: &BlobId) -> Option<Bytes> {
        self.0.get(id).and_then(|(_, bytes)| bytes.clone())
    }
}

fn referenced(events: &[Event]) -> Vec<BlobId> {
    let mut ids = Vec::new();
    for event in events {
        match &event.payload {
            Payload::Message(m) => {
                for block in &m.blocks {
                    match block {
                        Content::Image { blob, .. } | Content::File { blob } => {
                            ids.push(blob.clone())
                        }
                        Content::Text { .. } => {}
                    }
                }
            }
            Payload::ToolResult(r) => {
                for part in &r.content {
                    if let ResultContent::Image { blob, .. } | ResultContent::File { blob } = part {
                        ids.push(blob.clone());
                    }
                }
            }
            _ => {}
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

/// Метаданные всех blob'ов лога; байты картинок — только модели с `vision`.
pub async fn load<C: Components>(
    shared: &Shared<C>,
    events: &[Event],
    vision: bool,
) -> Result<Loaded, Aux> {
    let ids = referenced(events);
    if ids.is_empty() {
        return Ok(Loaded::default());
    }
    let rows = shared
        .db()
        .reader()
        .await?
        .run(move |c| {
            ids.iter()
                .filter_map(|id| c.blobs().get(id).transpose())
                .collect::<DbResult<Vec<_>>>()
        })
        .await?;
    let dir = shared.cfg().blob_dir();
    let mut out = BTreeMap::new();
    for row in rows {
        let meta = BlobMeta {
            name: row.name.clone(),
            size: row.size,
        };
        let bytes = if vision && row.kind == Kind::Image {
            // Нечитаемый файл — маркер вместо картинки, а не провал шага.
            tokio::fs::read(file(&dir, &row)).await.ok().map(Bytes)
        } else {
            None
        };
        out.insert(row.id.clone(), (meta, bytes));
    }
    Ok(Loaded(out))
}

/// Картинка тула или модели в blob-store; `name` — лист без разделителей.
pub async fn store<C: Components>(
    shared: &Shared<C>,
    chat: ChatId,
    media_type: &str,
    data: &[u8],
    source: Source,
) -> Result<BlobId, Aux> {
    let work = &shared.cfg().work;
    if data.len() as u64 > work.blob_max.as_u64() {
        return Err(Aux::Quota(format!(
            "image is {} bytes, work.blob_max is {}",
            data.len(),
            work.blob_max.as_u64()
        )));
    }
    let ext = media_type
        .split_once('/')
        .map(|(_, sub)| sub)
        .filter(|s| s.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin");
    let mut session = crate::session::internal(shared);
    let data = bytes::Bytes::copy_from_slice(data);
    let args = eva_protocol::requests::blobs::put::Args {
        meta: eva_protocol::requests::blobs::put::Meta {
            chat,
            name: format!("image.{ext}"),
            media_type: media_type.to_owned(),
            kind: Kind::Image,
            size: data.len() as u64,
            hash: None,
        },
        body: Box::pin(futures::stream::once(async move { Ok(data) })),
    };
    let id = session
        .blobs()
        .put()
        .call(args)
        .await
        .map_err(|e| match e {
            eva_protocol::errors::Generic::Aux(e) => e,
            e => Aux::Invalid(e.to_string()),
        })?
        .id;
    let written = id.clone();
    shared
        .db()
        .write(move |tx| tx.blobs().set_source(&written, source))
        .await?;
    Ok(id)
}
