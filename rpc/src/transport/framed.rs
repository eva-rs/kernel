//! Кадрирование потока байт: `u32 BE` длина и msgpack-кадр (uds, пара в памяти).

use std::path::Path;

use futures::{SinkExt as _, StreamExt as _, stream};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec, LengthDelimitedCodecError};

use crate::{
    config::Config,
    transport::{
        Conn, Outbound, TransportError,
        meter::{Meter, Metered},
    },
};

pub(crate) fn conn<T>(io: T, cfg: &Config, remote: impl Into<String>) -> Conn
where
    T: AsyncRead + AsyncWrite + Send + 'static,
{
    let codec = LengthDelimitedCodec::builder()
        .big_endian()
        .length_field_type::<u32>()
        .max_frame_length(cfg.max_frame())
        .new_codec();
    let meter = Meter::new();
    let (sink, source) = Framed::new(Metered::new(io, meter.clone()), codec).split();

    let tx = sink.sink_map_err(classify).with_flat_map(|out: Outbound| {
        let frame = match out {
            Outbound::Frame(frame) => Some(Ok(frame)),
            Outbound::Close(_) => None,
        };
        stream::iter(frame)
    });
    let rx = source.map(|frame| frame.map(|f| f.freeze()).map_err(classify));

    Conn::metered(tx, rx, remote, meter)
}

pub(crate) async fn connect(path: &Path, cfg: &Config) -> Result<Conn, TransportError> {
    let stream = tokio::net::UnixStream::connect(path)
        .await
        .map_err(TransportError::io)?;
    Ok(conn(stream, cfg, path.display().to_string()))
}

fn classify(e: std::io::Error) -> TransportError {
    let too_large = e
        .get_ref()
        .is_some_and(|inner| inner.is::<LengthDelimitedCodecError>());
    if too_large {
        TransportError::TooLarge
    } else {
        TransportError::io(e)
    }
}
