//! Pairing: how two devices come to trust each other.
//!
//! One device (the host) shows a 6-digit code, also embedded in a QR ticket.
//! The other (the joiner) enters the code or scans the ticket. They then run
//! SPAKE2, a password-authenticated key exchange, with the code as password:
//!
//! - Someone watching the network learns nothing about the code.
//! - Someone actively interfering gets exactly one guess per code (a one in a
//!   million chance), after which the host throws the code away.
//! - The exchange is tied to both devices' public keys, taken from the
//!   encrypted connection it runs over. A device relaying between the two
//!   cannot complete it even though it sees every message.
//!
//! Message flow on one stream opened by the joiner:
//!
//! ```text
//! joiner                               host
//!   Start  { spake }        ------->
//!          <-------   Reply { spake }
//!   Confirm { mac, device } ------->          host checks the code here
//!          <-------   Accept { mac, device }  joiner checks the host here
//! ```
//!
//! The joiner proves knowledge of the code first, so a device that is not in
//! pairing mode never reveals anything checkable.

use std::{fmt, net::SocketAddr, str::FromStr, time::Duration};

use serde::{Deserialize, Serialize};
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    device::DeviceInfo,
    error::{Error, Result},
    identity::DeviceId,
    protocol::{decode, read_frame, write_frame},
};

/// Identifies the pairing protocol during the encrypted handshake. Unlike the
/// main protocol, connections here are accepted from devices we do not know.
pub const PAIR_ALPN: &[u8] = b"henos/pair/1";

/// How long a pairing code stays valid.
pub const CODE_TTL: Duration = Duration::from_secs(120);

/// Number of digits in a pairing code.
pub const CODE_DIGITS: usize = 6;

const MAX_PAIR_FRAME: usize = 4096;
const TICKET_PREFIX: &str = "henos://pair?";
const CONFIRM_CONTEXT: &str = "henos pairing v1 confirmation key";

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Start {
    spake: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Reply {
    spake: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Confirm {
    mac: [u8; 32],
    device: DeviceInfo,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Accept {
    mac: [u8; 32],
    device: DeviceInfo,
}

/// A fresh random pairing code.
pub fn generate_code() -> String {
    let n: u32 = rand::random_range(0..10u32.pow(CODE_DIGITS as u32));
    format!("{n:0width$}", width = CODE_DIGITS)
}

/// Cleans up a code as typed by a person: spaces and dashes are dropped.
pub fn normalize_code(input: &str) -> Result<String> {
    let code: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    if code.len() == CODE_DIGITS && code.bytes().all(|b| b.is_ascii_digit()) {
        Ok(code)
    } else {
        Err(Error::InvalidCode)
    }
}

/// Everything a joiner needs to pair: shown as a QR code or pasted as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingTicket {
    pub id: DeviceId,
    pub code: String,
    /// Where the host can be reached. May be empty, in which case the host
    /// has to be found through local discovery.
    pub addrs: Vec<SocketAddr>,
}

impl fmt::Display for PairingTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{TICKET_PREFIX}v=1&id={}&code={}", self.id, self.code)?;
        for addr in &self.addrs {
            write!(f, "&addr={addr}")?;
        }
        Ok(())
    }
}

impl FromStr for PairingTicket {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let invalid = |why: &str| Error::InvalidTicket(why.to_owned());
        let query = s
            .trim()
            .strip_prefix(TICKET_PREFIX)
            .ok_or_else(|| invalid("not a HENOS pairing ticket"))?;

        let (mut version, mut id, mut code, mut addrs) = (None, None, None, Vec::new());
        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key {
                "v" => version = Some(value),
                "id" => id = Some(value),
                "code" => code = Some(value),
                "addr" => {
                    // Addresses are hints; skip any we cannot read.
                    if let Ok(addr) = value.parse() {
                        addrs.push(addr);
                    }
                }
                // Keys added by newer versions are ignored.
                _ => {}
            }
        }
        if version != Some("1") {
            return Err(invalid("unsupported ticket version"));
        }
        let id = id
            .ok_or_else(|| invalid("missing device id"))?
            .parse()
            .map_err(|_| invalid("invalid device id"))?;
        let code = normalize_code(code.ok_or_else(|| invalid("missing code"))?)
            .map_err(|_| invalid("invalid code"))?;
        Ok(Self { id, code, addrs })
    }
}

/// Derives the two confirmation values from the SPAKE2 output. Each side
/// sends its own and checks the other's.
fn confirmations(shared_key: &[u8]) -> ([u8; 32], [u8; 32]) {
    let key = blake3::derive_key(CONFIRM_CONTEXT, shared_key);
    (
        *blake3::keyed_hash(&key, b"joiner").as_bytes(),
        *blake3::keyed_hash(&key, b"host").as_bytes(),
    )
}

/// Constant-time comparison.
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    blake3::Hash::from(*a) == blake3::Hash::from(*b)
}

fn spake_ids(joiner: &DeviceId, host: &DeviceId) -> (SpakeIdentity, SpakeIdentity) {
    (
        SpakeIdentity::new(joiner.as_bytes()),
        SpakeIdentity::new(host.as_bytes()),
    )
}

async fn read_msg<R, T>(recv: &mut R) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: serde::de::DeserializeOwned,
{
    let body = read_frame(recv, MAX_PAIR_FRAME)
        .await?
        .ok_or_else(|| Error::Protocol("connection closed during pairing".into()))?;
    decode(&body)
}

/// Runs the joiner's side. On success returns the host's self-description.
///
/// `our_id` and `host_id` must be the keys of the encrypted connection the
/// streams belong to.
pub(crate) async fn join<W, R>(
    send: &mut W,
    recv: &mut R,
    code: &str,
    our_id: &DeviceId,
    host_id: &DeviceId,
    our_device: &DeviceInfo,
) -> Result<DeviceInfo>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let (id_joiner, id_host) = spake_ids(our_id, host_id);
    let (state, outbound) =
        Spake2::<Ed25519Group>::start_a(&Password::new(code.as_bytes()), &id_joiner, &id_host);
    write_frame(send, &Start { spake: outbound }, MAX_PAIR_FRAME).await?;

    let reply: Reply = read_msg(recv).await?;
    let shared_key = state
        .finish(&reply.spake)
        .map_err(|_| Error::Pairing("the other device sent an invalid response".into()))?;
    let (mac_joiner, mac_host) = confirmations(&shared_key);

    let confirm = Confirm {
        mac: mac_joiner,
        device: our_device.clone(),
    };
    write_frame(send, &confirm, MAX_PAIR_FRAME).await?;

    let accept: Accept = read_msg(recv).await?;
    if !same(&accept.mac, &mac_host) {
        // The host accepted without knowing the code: not the device the
        // person is looking at.
        return Err(Error::Pairing(
            "the other device could not prove it knows the code".into(),
        ));
    }
    Ok(accept.device.sanitized())
}

/// Host side, step 1: reads the joiner's opening message.
pub(crate) async fn read_start<R>(recv: &mut R) -> Result<Start>
where
    R: AsyncRead + Unpin,
{
    read_msg(recv).await
}

/// Host side, step 2: checks that the joiner knows the code.
///
/// Returns the joiner's self-description and the message that completes
/// pairing. [`Error::WrongPairingCode`] means the code was guessed wrong and
/// must not be used again; any other error means the attempt broke off before
/// the code was tested.
pub(crate) async fn host_verify<W, R>(
    send: &mut W,
    recv: &mut R,
    start: Start,
    code: &str,
    our_id: &DeviceId,
    joiner_id: &DeviceId,
    our_device: &DeviceInfo,
) -> Result<(DeviceInfo, Accept)>
where
    W: AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
{
    let (id_joiner, id_host) = spake_ids(joiner_id, our_id);
    let (state, outbound) =
        Spake2::<Ed25519Group>::start_b(&Password::new(code.as_bytes()), &id_joiner, &id_host);
    write_frame(send, &Reply { spake: outbound }, MAX_PAIR_FRAME).await?;

    // A malformed message is treated like a wrong guess.
    let shared_key = state
        .finish(&start.spake)
        .map_err(|_| Error::WrongPairingCode)?;
    let (mac_joiner, mac_host) = confirmations(&shared_key);

    let confirm: Confirm = read_msg(recv).await?;
    if !same(&confirm.mac, &mac_joiner) {
        return Err(Error::WrongPairingCode);
    }
    let accept = Accept {
        mac: mac_host,
        device: our_device.clone(),
    };
    Ok((confirm.device.sanitized(), accept))
}

/// Host side, step 3: tells the joiner that pairing succeeded.
pub(crate) async fn host_accept<W>(send: &mut W, accept: Accept) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_frame(send, &accept, MAX_PAIR_FRAME).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{device::Platform, identity::Identity};
    use tokio::io::{duplex, split};

    fn info(name: &str) -> DeviceInfo {
        DeviceInfo {
            name: name.to_owned(),
            platform: Platform::current(),
        }
    }

    /// Runs both sides over an in-memory pipe. `ids` overrides who each side
    /// believes it is talking to (joiner as seen by host, host as seen by
    /// joiner), to simulate a relay in the middle.
    async fn run(
        host_code: &str,
        joiner_code: &str,
        ids: Option<(DeviceId, DeviceId)>,
    ) -> (Result<DeviceInfo>, Result<DeviceInfo>) {
        let host_id = Identity::generate().id();
        let joiner_id = Identity::generate().id();
        // What each side believes the other's key to be.
        let (host_sees_joiner_as, joiner_sees_host_as) = ids.unwrap_or((joiner_id, host_id));

        let (host_io, joiner_io) = duplex(8192);
        let (mut host_recv, mut host_send) = split(host_io);
        let (mut joiner_recv, mut joiner_send) = split(joiner_io);

        // `move`: when the host finishes, even with an error, its end of the
        // pipe is dropped, so the joiner sees the stream close instead of
        // waiting forever.
        let host = async move {
            let start = read_start(&mut host_recv).await?;
            let (device, accept) = host_verify(
                &mut host_send,
                &mut host_recv,
                start,
                host_code,
                &host_id,
                &host_sees_joiner_as,
                &info("Host"),
            )
            .await?;
            host_accept(&mut host_send, accept).await?;
            Ok::<_, Error>(device)
        };
        let joiner_info = info("Joiner");
        let joiner = join(
            &mut joiner_send,
            &mut joiner_recv,
            joiner_code,
            &joiner_id,
            &joiner_sees_host_as,
            &joiner_info,
        );
        tokio::join!(host, joiner)
    }

    #[tokio::test]
    async fn right_code_pairs_and_exchanges_names() {
        let (host, joiner) = run("123456", "123456", None).await;
        assert_eq!(host.unwrap().name, "Joiner");
        assert_eq!(joiner.unwrap().name, "Host");
    }

    #[tokio::test]
    async fn wrong_code_is_detected_by_the_host() {
        let (host, joiner) = run("123456", "654321", None).await;
        assert!(matches!(host, Err(Error::WrongPairingCode)));
        assert!(joiner.is_err());
    }

    #[tokio::test]
    async fn relay_in_the_middle_cannot_pair_even_with_the_right_code() {
        // Both sides use the right code, but each is actually connected to a
        // relay with its own key, so they disagree about who is talking.
        let relay = Identity::generate().id();
        let (host, joiner) = run("123456", "123456", Some((relay, relay))).await;
        assert!(matches!(host, Err(Error::WrongPairingCode)));
        assert!(joiner.is_err());
    }

    #[tokio::test]
    async fn host_that_does_not_know_the_code_is_rejected_by_the_joiner() {
        let host_id = Identity::generate().id();
        let joiner_id = Identity::generate().id();
        let (host_io, joiner_io) = duplex(8192);
        let (mut host_recv, mut host_send) = split(host_io);
        let (mut joiner_recv, mut joiner_send) = split(joiner_io);

        // A fake host that accepts whatever it is sent.
        let fake_host = async {
            let _start: Start = read_msg(&mut host_recv).await.unwrap();
            let (_, outbound) = Spake2::<Ed25519Group>::start_b(
                &Password::new(b"000000"),
                &SpakeIdentity::new(joiner_id.as_bytes()),
                &SpakeIdentity::new(host_id.as_bytes()),
            );
            write_frame(&mut host_send, &Reply { spake: outbound }, MAX_PAIR_FRAME)
                .await
                .unwrap();
            let _confirm: Confirm = read_msg(&mut host_recv).await.unwrap();
            let accept = Accept {
                mac: [7u8; 32],
                device: info("Evil"),
            };
            host_accept(&mut host_send, accept).await.unwrap();
        };
        let joiner_info = info("Joiner");
        let joiner = join(
            &mut joiner_send,
            &mut joiner_recv,
            "123456",
            &joiner_id,
            &host_id,
            &joiner_info,
        );
        let (_, joined) = tokio::join!(fake_host, joiner);
        assert!(matches!(joined, Err(Error::Pairing(_))));
    }

    #[test]
    fn codes_are_six_digits() {
        for _ in 0..200 {
            let code = generate_code();
            assert_eq!(code.len(), CODE_DIGITS);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
            assert_eq!(normalize_code(&code).unwrap(), code);
        }
    }

    #[test]
    fn typed_codes_are_cleaned_up() {
        assert_eq!(normalize_code(" 123 456 ").unwrap(), "123456");
        assert_eq!(normalize_code("123-456").unwrap(), "123456");
        assert!(matches!(normalize_code("12345"), Err(Error::InvalidCode)));
        assert!(matches!(normalize_code("1234567"), Err(Error::InvalidCode)));
        assert!(matches!(normalize_code("12345a"), Err(Error::InvalidCode)));
        assert!(matches!(
            normalize_code("１２３４５６"),
            Err(Error::InvalidCode)
        ));
    }

    #[test]
    fn ticket_round_trips() {
        let ticket = PairingTicket {
            id: Identity::generate().id(),
            code: "004271".into(),
            addrs: vec![
                "192.168.1.20:51234".parse().unwrap(),
                "[fe80::1]:51234".parse().unwrap(),
            ],
        };
        let text = ticket.to_string();
        assert!(text.starts_with("henos://pair?"));
        assert_eq!(text.parse::<PairingTicket>().unwrap(), ticket);
    }

    #[test]
    fn bad_tickets_are_refused() {
        let id = Identity::generate().id();
        for bad in [
            "https://example.com".to_owned(),
            "henos://pair?".to_owned(),
            format!("henos://pair?v=2&id={id}&code=123456"),
            "henos://pair?v=1&code=123456".to_owned(),
            "henos://pair?v=1&id=xyz&code=123456".to_owned(),
            format!("henos://pair?v=1&id={id}&code=12"),
        ] {
            assert!(
                matches!(bad.parse::<PairingTicket>(), Err(Error::InvalidTicket(_))),
                "accepted {bad}"
            );
        }
        // Unknown keys and unreadable addresses are tolerated.
        let ok = format!("henos://pair?v=1&id={id}&code=123456&addr=nonsense&future=1");
        assert!(ok.parse::<PairingTicket>().unwrap().addrs.is_empty());
    }
}
