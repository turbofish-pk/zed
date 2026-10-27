//! The sans-IO proxy handshake state machine.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};

use base64::Engine as _;

use crate::{Credentials, MAX_HTTP_RESPONSE_LENGTH, ProxyError, ProxyScheme, ProxySpec, Target};

const MAX_HTTP_RESPONSE_HEADERS: usize = 64;

const SOCKS4_VERSION: u8 = 0x04;
const SOCKS5_VERSION: u8 = 0x05;
const SOCKS_COMMAND_CONNECT: u8 = 0x01;
const SOCKS4_REPLY_GRANTED: u8 = 0x5A;
const SOCKS5_METHOD_NO_AUTHENTICATION: u8 = 0x00;
const SOCKS5_METHOD_USERNAME_PASSWORD: u8 = 0x02;
const SOCKS5_ADDRESS_IPV4: u8 = 0x01;
const SOCKS5_ADDRESS_DOMAIN: u8 = 0x03;
const SOCKS5_ADDRESS_IPV6: u8 = 0x04;

/// A proxy handshake in progress.
///
/// Sans-IO: the handshake never performs I/O itself. Drive it by calling
/// [`advance`](Self::advance) with bytes received from the proxy and acting
/// on the returned [`Step`]s until it reports [`Step::Done`].
pub struct Handshake {
    exchanges: VecDeque<Exchange>,
    input: Vec<u8>,
}

/// What the caller must do next to make progress on the handshake.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Write these bytes to the proxy.
    Send(Vec<u8>),
    /// Read more bytes from the proxy and pass them to the next `advance`.
    NeedMoreInput,
    /// The tunnel is established. `leftover` holds any bytes the proxy sent
    /// past the end of the handshake; they belong to the tunneled stream and
    /// must be consumed before reading from the transport again.
    Done { leftover: Vec<u8> },
}

enum Exchange {
    Send(Vec<u8>),
    Expect(Expectation),
}

enum Expectation {
    HttpResponseHead,
    Socks5MethodSelection { offered: u8 },
    Socks5AuthReply,
    Socks5ConnectReply,
    Socks4ConnectReply,
}

impl Handshake {
    /// Plans the handshake for tunneling to `target` through the proxy
    /// described by `spec`.
    ///
    /// Passing a [`Target::Domain`] delegates DNS resolution to the proxy
    /// regardless of scheme (SOCKS4 proxies are sent the 4a extension);
    /// resolve the host locally and pass a [`Target::Address`] when
    /// [`ProxySpec::remote_dns`] is false, to follow the proxy URL's intent.
    pub fn new(spec: &ProxySpec, target: &Target) -> Result<Self, ProxyError> {
        // The domain is spliced into protocol messages (the CONNECT request
        // line, NUL-terminated SOCKS4a fields), so control bytes would let a
        // hostile target string forge protocol structure. No valid host name
        // contains them; fail closed rather than trusting the caller to have
        // validated.
        if let Target::Domain(domain, _) = target
            && domain
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte == 0x7F)
        {
            return Err(ProxyError::DomainContainsControlBytes);
        }

        let mut exchanges = VecDeque::new();
        match spec.scheme {
            ProxyScheme::Http { .. } => {
                exchanges.push_back(Exchange::Send(http_connect_request(
                    target,
                    spec.credentials.as_ref(),
                )));
                exchanges.push_back(Exchange::Expect(Expectation::HttpResponseHead));
            }
            ProxyScheme::Socks4 { .. } => {
                exchanges.push_back(Exchange::Send(socks4_connect_request(
                    target,
                    spec.credentials.as_ref(),
                )?));
                exchanges.push_back(Exchange::Expect(Expectation::Socks4ConnectReply));
            }
            ProxyScheme::Socks5 { .. } => {
                let method = if spec.credentials.is_some() {
                    SOCKS5_METHOD_USERNAME_PASSWORD
                } else {
                    SOCKS5_METHOD_NO_AUTHENTICATION
                };
                exchanges.push_back(Exchange::Send(vec![SOCKS5_VERSION, 1, method]));
                exchanges.push_back(Exchange::Expect(Expectation::Socks5MethodSelection {
                    offered: method,
                }));
                if let Some(credentials) = &spec.credentials {
                    exchanges.push_back(Exchange::Send(socks5_auth_request(credentials)?));
                    exchanges.push_back(Exchange::Expect(Expectation::Socks5AuthReply));
                }
                exchanges.push_back(Exchange::Send(socks5_connect_request(target)?));
                exchanges.push_back(Exchange::Expect(Expectation::Socks5ConnectReply));
            }
        }
        Ok(Self {
            exchanges,
            input: Vec::new(),
        })
    }

    /// Advances the state machine with bytes newly received from the proxy
    /// (empty on the first call, and whenever there is nothing new to feed).
    ///
    /// Callers must not feed the same bytes twice: partial input is buffered
    /// internally until it can be parsed.
    pub fn advance(&mut self, received: &[u8]) -> Result<Step, ProxyError> {
        self.input.extend_from_slice(received);
        loop {
            match self.exchanges.pop_front() {
                None => {
                    return Ok(Step::Done {
                        leftover: std::mem::take(&mut self.input),
                    });
                }
                Some(Exchange::Send(bytes)) => return Ok(Step::Send(bytes)),
                Some(Exchange::Expect(expectation)) => {
                    match try_parse(&expectation, &self.input)? {
                        Some(consumed) => {
                            self.input.drain(..consumed);
                        }
                        None => {
                            self.exchanges.push_front(Exchange::Expect(expectation));
                            return Ok(Step::NeedMoreInput);
                        }
                    }
                }
            }
        }
    }
}

/// Attempts to parse one expected proxy message from the buffered input.
/// Returns the number of bytes consumed, or `None` when more input is needed.
fn try_parse(expectation: &Expectation, input: &[u8]) -> Result<Option<usize>, ProxyError> {
    match expectation {
        Expectation::HttpResponseHead => parse_http_response_head(input),
        Expectation::Socks5MethodSelection { offered } => {
            let Some(reply) = input.get(..2) else {
                return Ok(None);
            };
            if reply[0] != SOCKS5_VERSION {
                return Err(ProxyError::UnexpectedSocksVersion(reply[0]));
            }
            if reply[1] != *offered {
                return Err(ProxyError::AuthMethodRejected { selected: reply[1] });
            }
            Ok(Some(2))
        }
        Expectation::Socks5AuthReply => {
            let Some(reply) = input.get(..2) else {
                return Ok(None);
            };
            // RFC 1929: any non-zero status is failure.
            if reply[1] != 0x00 {
                return Err(ProxyError::AuthenticationFailed);
            }
            Ok(Some(2))
        }
        Expectation::Socks5ConnectReply => {
            let Some(header) = input.get(..4) else {
                return Ok(None);
            };
            if header[0] != SOCKS5_VERSION {
                return Err(ProxyError::UnexpectedSocksVersion(header[0]));
            }
            if header[1] != 0x00 {
                return Err(ProxyError::Socks5ConnectRefused(header[1]));
            }
            let address_length = match header[3] {
                SOCKS5_ADDRESS_IPV4 => 4,
                SOCKS5_ADDRESS_IPV6 => 16,
                SOCKS5_ADDRESS_DOMAIN => match input.get(4) {
                    Some(length) => 1 + *length as usize,
                    None => return Ok(None),
                },
                other => return Err(ProxyError::UnknownAddressType(other)),
            };
            // The reply's bound address and port only matter for BIND, so
            // they are consumed and discarded.
            let total = 4 + address_length + 2;
            if input.len() < total {
                return Ok(None);
            }
            Ok(Some(total))
        }
        Expectation::Socks4ConnectReply => {
            if input.len() < 8 {
                return Ok(None);
            }
            if input[1] != SOCKS4_REPLY_GRANTED {
                return Err(ProxyError::Socks4ConnectRefused(input[1]));
            }
            Ok(Some(8))
        }
    }
}

fn parse_http_response_head(input: &[u8]) -> Result<Option<usize>, ProxyError> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HTTP_RESPONSE_HEADERS];
    let mut response = httparse::Response::new(&mut headers);
    match response.parse(input) {
        Ok(httparse::Status::Complete(consumed)) => {
            // The cap applies to complete heads too: a driver that reads in
            // large chunks could otherwise hand a well-formed multi-megabyte
            // head to a single `advance` and have it accepted.
            if consumed > MAX_HTTP_RESPONSE_LENGTH {
                return Err(ProxyError::HttpResponseTooLarge);
            }
            let code = response.code.ok_or_else(|| {
                ProxyError::MalformedHttpResponse("missing status code".to_string())
            })?;
            if !(200..300).contains(&code) {
                return Err(ProxyError::HttpConnectRefused(code));
            }
            Ok(Some(consumed))
        }
        Ok(httparse::Status::Partial) => {
            if input.len() > MAX_HTTP_RESPONSE_LENGTH {
                Err(ProxyError::HttpResponseTooLarge)
            } else {
                Ok(None)
            }
        }
        Err(error) => Err(ProxyError::MalformedHttpResponse(error.to_string())),
    }
}

fn http_connect_request(target: &Target, credentials: Option<&Credentials>) -> Vec<u8> {
    let host = match target {
        Target::Domain(domain, port) => format!("{domain}:{port}"),
        // `SocketAddr`'s `Display` brackets IPv6 addresses, matching the
        // `host:port` form CONNECT requires.
        Target::Address(address) => address.to_string(),
    };
    let mut request =
        format!("CONNECT {host} HTTP/1.1\r\nHost: {host}\r\nProxy-Connection: Keep-Alive\r\n");
    if let Some(Credentials { username, password }) = credentials {
        let encoded = base64::prelude::BASE64_STANDARD.encode(format!("{username}:{password}"));
        request.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
    }
    request.push_str("\r\n");
    request.into_bytes()
}

fn socks5_auth_request(credentials: &Credentials) -> Result<Vec<u8>, ProxyError> {
    let username = credentials.username.as_bytes();
    let password = credentials.password.as_bytes();
    if username.len() > 255 || password.len() > 255 {
        return Err(ProxyError::CredentialsTooLong);
    }
    let mut request = Vec::with_capacity(3 + username.len() + password.len());
    // RFC 1929 subnegotiation version.
    request.push(0x01);
    request.push(username.len() as u8);
    request.extend_from_slice(username);
    request.push(password.len() as u8);
    request.extend_from_slice(password);
    Ok(request)
}

fn socks5_connect_request(target: &Target) -> Result<Vec<u8>, ProxyError> {
    let mut request = vec![SOCKS5_VERSION, SOCKS_COMMAND_CONNECT, 0x00];
    let port = match target {
        Target::Domain(domain, port) => {
            let domain = domain.as_bytes();
            if domain.len() > 255 {
                return Err(ProxyError::DomainTooLong);
            }
            request.push(SOCKS5_ADDRESS_DOMAIN);
            request.push(domain.len() as u8);
            request.extend_from_slice(domain);
            *port
        }
        Target::Address(address) => {
            match address.ip() {
                IpAddr::V4(ip) => {
                    request.push(SOCKS5_ADDRESS_IPV4);
                    request.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    request.push(SOCKS5_ADDRESS_IPV6);
                    request.extend_from_slice(&ip.octets());
                }
            }
            address.port()
        }
    };
    request.extend_from_slice(&port.to_be_bytes());
    Ok(request)
}

fn socks4_connect_request(
    target: &Target,
    credentials: Option<&Credentials>,
) -> Result<Vec<u8>, ProxyError> {
    let user_id = credentials
        .map(|credentials| credentials.username.as_str())
        .unwrap_or("");
    // The user id field is NUL-terminated on the wire, so an embedded NUL
    // would truncate it and shift the fields that follow.
    if user_id.as_bytes().contains(&0x00) {
        return Err(ProxyError::Socks4UserIdContainsNul);
    }
    let mut request = vec![SOCKS4_VERSION, SOCKS_COMMAND_CONNECT];
    match target {
        Target::Address(SocketAddr::V4(address)) => {
            request.extend_from_slice(&address.port().to_be_bytes());
            request.extend_from_slice(&address.ip().octets());
            request.extend_from_slice(user_id.as_bytes());
            request.push(0x00);
        }
        Target::Address(SocketAddr::V6(_)) => return Err(ProxyError::Socks4Ipv4Only),
        Target::Domain(domain, port) => {
            // SOCKS4a: the invalid destination address 0.0.0.1 signals that
            // a host name follows the user id.
            request.extend_from_slice(&port.to_be_bytes());
            request.extend_from_slice(&[0, 0, 0, 1]);
            request.extend_from_slice(user_id.as_bytes());
            request.push(0x00);
            request.extend_from_slice(domain.as_bytes());
            request.push(0x00);
        }
    }
    Ok(request)
}

