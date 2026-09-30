//! Noise `NNpsk0` сервисного соединения (`06 §1`): ядро — initiator, модуль —
//! responder, обе стороны шлют эфемерный ключ, PSK подмешан в первое
//! сообщение. Сообщение Noise на проводе — `u16 BE` длина и шифротекст; поверх
//! расшифрованного потока идёт обычное кадрирование `uds`.

use std::{
    fmt, io,
    pin::Pin,
    task::{Context, Poll, ready},
};

use eva::{data, utils::SecretString};
use hkdf::Hkdf;
use sha2::Sha256;
use snow::{Builder, HandshakeState, TransportState};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};

const PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

/// Метка HKDF: тот же секрет у `eva-hands` даёт другой PSK.
const INFO: &[u8] = b"eva-module psk v1";

/// Сообщение Noise целиком, вместе с тегом AEAD.
pub const MAX_MESSAGE: usize = 65535;

const TAG: usize = 16;

/// Открытого текста в одном сообщении.
pub const MAX_PLAINTEXT: usize = MAX_MESSAGE - TAG;

/// У `NNpsk0` оба сообщения handshake — эфемерный ключ и тег, 48 байт.
const MAX_HANDSHAKE: usize = 256;

/// PSK соединения ядра с модулем; `Debug` его не печатает.
#[derive(Clone)]
pub struct Psk([u8; 32]);

impl Psk {
    /// HKDF-SHA256 без salt; хвостовой перевод строки файла секрета в ключ не входит.
    pub fn derive(secret: &str) -> Self {
        let ikm = trim(secret);
        let mut okm = [0; 32];
        Hkdf::<Sha256>::new(None, ikm.as_bytes())
            .expand(INFO, &mut okm)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        Self(okm)
    }

    /// Секрет `key` из конфига: строка или файл.
    pub fn read(key: &SecretString) -> Result<Self, KeyError> {
        let secret = key
            .read()
            .map_err(|e| KeyError::Unreadable(format!("{e:#}")))?;
        if trim(&secret).is_empty() {
            return Err(KeyError::Empty);
        }
        Ok(Self::derive(&secret))
    }
}

impl fmt::Debug for Psk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Psk(***)")
    }
}

fn trim(secret: &str) -> &str {
    secret.trim_end_matches(['\n', '\r'])
}

#[data(error, not(serde, JsonSchema))]
pub enum KeyError {
    #[display("key is unreadable: {_0}")]
    Unreadable(String),
    #[display("key is empty")]
    Empty,
}

fn noise(e: snow::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("noise: {e}"))
}

fn state(psk: &Psk, initiator: bool) -> io::Result<HandshakeState> {
    let builder = Builder::new(PARAMS.parse().map_err(noise)?)
        .psk(0, &psk.0)
        .map_err(noise)?;
    if initiator {
        builder.build_initiator()
    } else {
        builder.build_responder()
    }
    .map_err(noise)
}

async fn send_handshake<S: AsyncWrite + Unpin>(
    io: &mut S,
    hs: &mut HandshakeState,
) -> io::Result<()> {
    let mut buf = [0; MAX_HANDSHAKE];
    let n = hs.write_message(&[], &mut buf).map_err(noise)?;
    let len = u16::try_from(n).map_err(io::Error::other)?;
    io.write_all(&len.to_be_bytes()).await?;
    io.write_all(&buf[..n]).await?;
    io.flush().await
}

async fn recv_handshake<S: AsyncRead + Unpin>(
    io: &mut S,
    hs: &mut HandshakeState,
) -> io::Result<()> {
    let mut len = [0; 2];
    io.read_exact(&mut len).await?;
    let len = usize::from(u16::from_be_bytes(len));
    if len > MAX_HANDSHAKE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "noise handshake message is too large",
        ));
    }
    let mut msg = [0; MAX_HANDSHAKE];
    io.read_exact(&mut msg[..len]).await?;
    let mut payload = [0; MAX_HANDSHAKE];
    hs.read_message(&msg[..len], &mut payload).map_err(noise)?;
    Ok(())
}

/// Сторона ядра: `-> psk, e`, `<- e, ee`. Чужой PSK модуль рвёт молча, поэтому
/// здесь он выглядит обрывом.
pub(crate) async fn initiate<S>(mut io: S, psk: &Psk) -> io::Result<NoiseStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut hs = state(psk, true)?;
    send_handshake(&mut io, &mut hs).await?;
    recv_handshake(&mut io, &mut hs).await.map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::new(
                e.kind(),
                "peer closed the noise handshake: wrong key or not an eva module",
            )
        } else {
            e
        }
    })?;
    Ok(NoiseStream::new(
        io,
        hs.into_transport_mode().map_err(noise)?,
    ))
}

/// Сторона модуля: первое сообщение не расшифруется без того же PSK; ошибка
/// роняет сокет без ответа.
pub(crate) async fn respond<S>(mut io: S, psk: &Psk) -> io::Result<NoiseStream<S>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut hs = state(psk, false)?;
    recv_handshake(&mut io, &mut hs).await.map_err(|e| {
        if e.kind() == io::ErrorKind::InvalidData {
            io::Error::new(e.kind(), format!("{e}: wrong key or not an eva kernel"))
        } else {
            e
        }
    })?;
    send_handshake(&mut io, &mut hs).await?;
    Ok(NoiseStream::new(
        io,
        hs.into_transport_mode().map_err(noise)?,
    ))
}

/// Зашифрованный поток байт поверх сокета. Запись копит не больше одного
/// сообщения открытого текста и запечатывает его при заполнении или `flush`.
pub(crate) struct NoiseStream<S> {
    io: S,
    noise: TransportState,

    /// Длина принимаемого сообщения; `rx_need == 0` — читается она.
    rx_head: [u8; 2],
    rx_head_got: usize,
    rx_cipher: Vec<u8>,
    rx_need: usize,
    rx_got: usize,
    rx_plain: Vec<u8>,
    rx_pos: usize,

    tx_plain: Vec<u8>,
    /// Запечатанное сообщение с длиной, ещё не ушедшее в сокет с `tx_pos`.
    tx_wire: Vec<u8>,
    tx_pos: usize,
}

impl<S> NoiseStream<S> {
    fn new(io: S, noise: TransportState) -> Self {
        Self {
            io,
            noise,
            rx_head: [0; 2],
            rx_head_got: 0,
            rx_cipher: Vec::new(),
            rx_need: 0,
            rx_got: 0,
            rx_plain: Vec::new(),
            rx_pos: 0,
            tx_plain: Vec::new(),
            tx_wire: Vec::new(),
            tx_pos: 0,
        }
    }

    fn seal(&mut self) -> io::Result<()> {
        self.tx_wire.resize(2 + self.tx_plain.len() + TAG, 0);
        let n = self
            .noise
            .write_message(&self.tx_plain, &mut self.tx_wire[2..])
            .map_err(noise)?;
        let len = u16::try_from(n).map_err(io::Error::other)?;
        self.tx_wire[..2].copy_from_slice(&len.to_be_bytes());
        self.tx_wire.truncate(2 + n);
        self.tx_plain.clear();
        Ok(())
    }
}

impl<S: AsyncWrite + Unpin> NoiseStream<S> {
    /// Запечатывает накопленное и дописывает всё в сокет.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            while self.tx_pos < self.tx_wire.len() {
                let n =
                    ready!(Pin::new(&mut self.io).poll_write(cx, &self.tx_wire[self.tx_pos..]))?;
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                self.tx_pos += n;
            }
            self.tx_wire.clear();
            self.tx_pos = 0;
            if self.tx_plain.is_empty() {
                return Poll::Ready(Ok(()));
            }
            self.seal()?;
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for NoiseStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        loop {
            if me.rx_pos < me.rx_plain.len() {
                let n = buf.remaining().min(me.rx_plain.len() - me.rx_pos);
                buf.put_slice(&me.rx_plain[me.rx_pos..me.rx_pos + n]);
                me.rx_pos += n;
                return Poll::Ready(Ok(()));
            }

            if me.rx_need == 0 {
                while me.rx_head_got < 2 {
                    let mut head = ReadBuf::new(&mut me.rx_head[me.rx_head_got..]);
                    ready!(Pin::new(&mut me.io).poll_read(cx, &mut head))?;
                    let n = head.filled().len();
                    if n == 0 {
                        // Конец только на границе сообщения; обрыв внутри — ошибка.
                        return Poll::Ready(if me.rx_head_got == 0 {
                            Ok(())
                        } else {
                            Err(io::ErrorKind::UnexpectedEof.into())
                        });
                    }
                    me.rx_head_got += n;
                }
                me.rx_head_got = 0;
                let need = usize::from(u16::from_be_bytes(me.rx_head));
                if need <= TAG {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "noise message without payload",
                    )));
                }
                me.rx_need = need;
                me.rx_got = 0;
                me.rx_cipher.resize(need, 0);
            }

            while me.rx_got < me.rx_need {
                let mut body = ReadBuf::new(&mut me.rx_cipher[me.rx_got..me.rx_need]);
                ready!(Pin::new(&mut me.io).poll_read(cx, &mut body))?;
                let n = body.filled().len();
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                }
                me.rx_got += n;
            }

            me.rx_plain.resize(me.rx_need, 0);
            let n = me
                .noise
                .read_message(&me.rx_cipher[..me.rx_need], &mut me.rx_plain)
                .map_err(noise)?;
            me.rx_plain.truncate(n);
            me.rx_pos = 0;
            me.rx_need = 0;
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for NoiseStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let me = self.get_mut();
        if me.tx_plain.len() >= MAX_PLAINTEXT {
            ready!(me.poll_drain(cx))?;
        }
        let n = buf.len().min(MAX_PLAINTEXT - me.tx_plain.len());
        me.tx_plain.extend_from_slice(&buf[..n]);
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        ready!(me.poll_drain(cx))?;
        Pin::new(&mut me.io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        ready!(me.poll_drain(cx))?;
        Pin::new(&mut me.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, DuplexStream, duplex};

    use super::*;

    fn hex(psk: &Psk) -> String {
        psk.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Та же HKDF с меткой рук: один секрет не должен давать им один PSK.
    fn hands(secret: &str) -> String {
        let mut okm = [0; 32];
        Hkdf::<Sha256>::new(None, secret.as_bytes())
            .expand(b"eva-hands psk v1", &mut okm)
            .unwrap();
        hex(&Psk(okm))
    }

    #[test]
    fn psk_is_hkdf_of_the_trimmed_secret() {
        // Вектор посчитан openssl kdf HKDF и python hmac независимо от крейта.
        let expected = "4d929ddbe37dae2163fb12da71cb916e023501d90ebde45a01221a6129ebd630";
        for secret in ["secret", "secret\n", "secret\r\n"] {
            assert_eq!(hex(&Psk::derive(secret)), expected, "{secret:?}");
        }
        assert_eq!(
            hands("secret"),
            "648afb8bb6b1c846f46100e5101dcc73ba37efe8a6f14ea03ec93c0ebbf76be1"
        );
        assert_ne!(hex(&Psk::derive("secret")), hands("secret"));
        assert_ne!(hex(&Psk::derive("secret ")), expected);
        assert!(!format!("{:?}", Psk::derive("secret")).contains("4d92"));
    }

    #[test]
    fn empty_or_missing_key_is_refused() {
        assert_eq!(
            Psk::read(&SecretString::Plaintext("\n".to_owned())).unwrap_err(),
            KeyError::Empty
        );
        assert!(matches!(
            Psk::read(&SecretString::File("/nonexistent/eva-key".into())).unwrap_err(),
            KeyError::Unreadable(_)
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "secret\n").unwrap();
        let psk = Psk::read(&SecretString::File(path)).unwrap();
        assert_eq!(hex(&psk), hex(&Psk::derive("secret")));
    }

    async fn pair(
        kernel: &str,
        module: &str,
    ) -> (
        io::Result<NoiseStream<DuplexStream>>,
        io::Result<NoiseStream<DuplexStream>>,
    ) {
        let (a, b) = duplex(1 << 16);
        let (kernel, module) = (Psk::derive(kernel), Psk::derive(module));
        tokio::join!(initiate(a, &kernel), respond(b, &module))
    }

    fn payload(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
            .collect()
    }

    #[tokio::test]
    async fn roundtrip_splits_into_messages() {
        let (kernel, module) = pair("k", "k").await;
        let (mut kernel, mut module) = (kernel.unwrap(), module.unwrap());

        // Больше трёх сообщений в каждую сторону разом.
        let there = payload(3 * MAX_PLAINTEXT + 17, 1);
        let back = payload(2 * MAX_PLAINTEXT + 1, 2);
        let ((), got_there) = tokio::join!(
            async {
                kernel.write_all(&there).await.unwrap();
                kernel.flush().await.unwrap();
            },
            async {
                let mut got = vec![0; there.len()];
                module.read_exact(&mut got).await.unwrap();
                got
            }
        );
        assert_eq!(got_there, there);

        let ((), got_back) = tokio::join!(
            async {
                module.write_all(&back).await.unwrap();
                module.shutdown().await.unwrap();
            },
            async {
                let mut got = Vec::new();
                kernel.read_to_end(&mut got).await.unwrap();
                got
            }
        );
        assert_eq!(got_back, back);
    }

    #[tokio::test]
    async fn wrong_key_fails_both_sides() {
        let (kernel, module) = pair("kernel key", "module key").await;
        let module = module.err().expect("the module must refuse a foreign psk");
        assert_eq!(module.kind(), io::ErrorKind::InvalidData);
        assert!(module.to_string().contains("wrong key"), "{module}");
        let kernel = kernel.err().expect("the kernel must not get a session");
        assert_eq!(kernel.kind(), io::ErrorKind::UnexpectedEof);
        assert!(kernel.to_string().contains("wrong key"), "{kernel}");
    }

    #[tokio::test]
    async fn tampered_message_is_refused() {
        let (a, b) = duplex(1 << 16);
        let (a, b) = (tokio::io::split(a), b);
        let (mut raw_rd, mut raw_wr) = a;
        let psk = Psk::derive("k");
        let responder = tokio::spawn(async move {
            let mut module = respond(b, &psk).await.unwrap();
            let mut buf = [0; 16];
            module.read(&mut buf).await
        });

        // Initiator руками: handshake честный, затем сообщение с битым тегом.
        let mut hs = state(&Psk::derive("k"), true).unwrap();
        send_handshake(&mut raw_wr, &mut hs).await.unwrap();
        recv_handshake(&mut raw_rd, &mut hs).await.unwrap();
        let mut transport = hs.into_transport_mode().unwrap();
        let mut msg = [0; 64];
        let n = transport.write_message(b"hello", &mut msg).unwrap();
        msg[n - 1] ^= 1;
        raw_wr.write_all(&(n as u16).to_be_bytes()).await.unwrap();
        raw_wr.write_all(&msg[..n]).await.unwrap();

        let error = responder.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn eof_inside_a_message_is_an_error() {
        let (kernel, module) = pair("k", "k").await;
        let (kernel, mut module) = (kernel.unwrap(), module.unwrap());
        let NoiseStream { mut io, .. } = kernel;
        io.write_all(&100_u16.to_be_bytes()).await.unwrap();
        io.write_all(&[0; 10]).await.unwrap();
        drop(io);

        let mut buf = [0; 16];
        let error = module.read(&mut buf).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}
