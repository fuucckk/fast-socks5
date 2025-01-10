use crate::new_udp_header;
use crate::parse_udp_request;
use crate::read_exact;
use crate::ready;
use crate::util::stream::tcp_connect_with_timeout;
use crate::util::target_addr::{read_address, TargetAddr};
use crate::Socks5Command;
use crate::{consts, AuthenticationMethod, ReplyError, Result, SocksError};
use anyhow::Context;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::{SocketAddr, ToSocketAddrs as StdToSocketAddrs};
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as AsyncContext, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::net::{TcpListener, TcpStream, ToSocketAddrs as AsyncToSocketAddrs};
use tokio::try_join;
use tokio_stream::Stream;

#[derive(Clone)]
pub struct Config<A: Authentication = DenyAuthentication> {
    /// Timeout of the command request
    request_timeout: u64,
    /// Avoid useless roundtrips if we don't need the Authentication layer
    skip_auth: bool,
    /// Enable dns-resolving
    dns_resolve: bool,
    /// Enable command execution
    execute_command: bool,
    /// Enable UDP support
    allow_udp: bool,
    /// For some complex scenarios, we may want to either accept Username/Password configuration
    /// or IP Whitelisting, in case the client send only 1-2 auth methods (no auth) rather than 3 (with auth)
    allow_no_auth: bool,
    /// Contains the authentication trait to use the user against with
    auth: Option<Arc<A>>,
    /// Disables Nagle's algorithm for TCP
    nodelay: bool,
}

impl<A: Authentication> Default for Config<A> {
    fn default() -> Self {
        Config {
            request_timeout: 10,
            skip_auth: false,
            dns_resolve: true,
            execute_command: true,
            allow_udp: false,
            allow_no_auth: false,
            auth: None,
            nodelay: false,
        }
    }
}

/// Use this trait to handle a custom authentication on your end.
#[async_trait::async_trait]
pub trait Authentication: Send + Sync {
    type Item;

    async fn authenticate(&self, credentials: Option<(String, String)>) -> Option<Self::Item>;
}

/// Basic user/pass auth method provided.
pub struct SimpleUserPassword {
    pub username: String,
    pub password: String,
}

/// The struct returned when the user has successfully authenticated
pub struct AuthSucceeded {
    pub username: String,
}

/// This is an example to auth via simple credentials.
/// If the auth succeed, we return the username authenticated with, for further uses.
#[async_trait::async_trait]
impl Authentication for SimpleUserPassword {
    type Item = AuthSucceeded;

    async fn authenticate(&self, credentials: Option<(String, String)>) -> Option<Self::Item> {
        if let Some((username, password)) = credentials {
            // Client has supplied credentials
            if username == self.username && password == self.password {
                // Some() will allow the authentication and the credentials
                // will be forwarded to the socket
                Some(AuthSucceeded { username })
            } else {
                // Credentials incorrect, we deny the auth
                None
            }
        } else {
            // The client hasn't supplied any credentials, which only happens
            // when `Config::allow_no_auth()` is set as `true`
            None
        }
    }
}

/// This will simply return Option::None, which denies the authentication
#[derive(Copy, Clone, Default)]
pub struct DenyAuthentication {}

#[async_trait::async_trait]
impl Authentication for DenyAuthentication {
    type Item = ();

    async fn authenticate(&self, _credentials: Option<(String, String)>) -> Option<Self::Item> {
        None
    }
}

/// While this one will always allow the user in.
#[derive(Copy, Clone, Default)]
pub struct AcceptAuthentication {}

#[async_trait::async_trait]
impl Authentication for AcceptAuthentication {
    type Item = ();

    async fn authenticate(&self, _credentials: Option<(String, String)>) -> Option<Self::Item> {
        Some(())
    }
}

impl<A: Authentication> Config<A> {
    /// How much time it should wait until the request timeout.
    pub fn set_request_timeout(&mut self, n: u64) -> &mut Self {
        self.request_timeout = n;
        self
    }

    /// Skip the entire auth/handshake part, which means the server will directly wait for
    /// the command request.
    pub fn set_skip_auth(&mut self, value: bool) -> &mut Self {
        self.skip_auth = value;
        self.auth = None;
        self
    }

    /// Enable authentication
    /// 'static lifetime for Authentication avoid us to use `dyn Authentication`
    /// and set the Arc before calling the function.
    pub fn with_authentication<T: Authentication + 'static>(self, authentication: T) -> Config<T> {
        Config {
            request_timeout: self.request_timeout,
            skip_auth: self.skip_auth,
            dns_resolve: self.dns_resolve,
            execute_command: self.execute_command,
            allow_udp: self.allow_udp,
            allow_no_auth: self.allow_no_auth,
            auth: Some(Arc::new(authentication)),
            nodelay: self.nodelay,
        }
    }

    /// For some complex scenarios, we may want to either accept Username/Password configuration
    /// or IP Whitelisting, in case the client send only 2 auth methods rather than 3 (with auth)
    pub fn set_allow_no_auth(&mut self, value: bool) -> &mut Self {
        self.allow_no_auth = value;
        self
    }

    /// Set whether or not to execute commands
    pub fn set_execute_command(&mut self, value: bool) -> &mut Self {
        self.execute_command = value;
        self
    }

    /// Will the server perform dns resolve
    pub fn set_dns_resolve(&mut self, value: bool) -> &mut Self {
        self.dns_resolve = value;
        self
    }

    /// Set whether or not to allow udp traffic
    pub fn set_udp_support(&mut self, value: bool) -> &mut Self {
        self.allow_udp = value;
        self
    }
}

/// Wrapper of TcpListener
/// Useful if you don't use any existing TcpListener's streams.
pub struct Socks5Server<A: Authentication = DenyAuthentication> {
    listener: TcpListener,
    config: Arc<Config<A>>,
}

impl<A: Authentication + Default> Socks5Server<A> {
    pub async fn bind<S: AsyncToSocketAddrs>(addr: S) -> io::Result<Self> {
        let listener = TcpListener::bind(&addr).await?;
        let config = Arc::new(Config::default());

        Ok(Socks5Server { listener, config })
    }
}

impl<A: Authentication> Socks5Server<A> {
    /// Set a custom config
    pub fn with_config<T: Authentication>(self, config: Config<T>) -> Socks5Server<T> {
        Socks5Server {
            listener: self.listener,
            config: Arc::new(config),
        }
    }

    /// Can loop on `incoming().next()` to iterate over incoming connections.
    pub fn incoming(&self) -> Incoming<'_, A> {
        Incoming(self, None)
    }
}

/// `Incoming` implements [`futures_core::stream::Stream`].
///
/// [`futures_core::stream::Stream`]: https://docs.rs/futures/0.3.30/futures/stream/trait.Stream.html
pub struct Incoming<'a, A: Authentication>(
    &'a Socks5Server<A>,
    Option<Pin<Box<dyn Future<Output = io::Result<(TcpStream, SocketAddr)>> + Send + Sync + 'a>>>,
);

/// Iterator for each incoming stream connection
/// this wrapper will convert async_std TcpStream into Socks5Socket.
impl<'a, A: Authentication> Stream for Incoming<'a, A> {
    type Item = Result<Socks5Socket<TcpStream, A>>;

    /// this code is mainly borrowed from [`Incoming::poll_next()` of `TcpListener`][tcpListenerLink]
    ///
    /// [tcpListenerLink]: https://docs.rs/async-std/1.8.0/async_std/net/struct.TcpListener.html#method.incoming
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut AsyncContext<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.1.is_none() {
                self.1 = Some(Box::pin(self.0.listener.accept()));
            }

            if let Some(f) = &mut self.1 {
                // early returns if pending
                let (socket, peer_addr) = ready!(f.as_mut().poll(cx))?;
                self.1 = None;

                let local_addr = socket.local_addr()?;
                debug!(
                    "incoming connection from peer {} @ {}",
                    &peer_addr, &local_addr
                );

                // Wrap the TcpStream into Socks5Socket
                let socket = Socks5Socket::new(socket, self.0.config.clone());

                return Poll::Ready(Some(Ok(socket)));
            }
        }
    }
}

/// Wrap TcpStream and contains Socks5 protocol implementation.
pub struct Socks5Socket<T: AsyncRead + AsyncWrite + Unpin, A: Authentication> {
    inner: T,
    config: Arc<Config<A>>,
    auth: AuthenticationMethod,
    target_addr: Option<TargetAddr>,
    cmd: Option<Socks5Command>,
    /// Socket address which will be used in the reply message.
    reply_ip: Option<IpAddr>,
    /// If the client has been authenticated, that's where we store his credentials
    /// to be accessed from the socket
    credentials: Option<A::Item>,
}

pub mod states {
    pub struct Opened;
    pub struct AuthMethodsRead;
    pub struct AuthMethodChosen;
    pub struct Authenticated;
    pub struct CommandRead;
}

pub struct Socks5ServerProtocol<T, S> {
    inner: T,
    _state: PhantomData<S>,
}

impl<T, S> Socks5ServerProtocol<T, S> {
    fn new(inner: T) -> Self {
        Socks5ServerProtocol {
            inner,
            _state: PhantomData,
        }
    }
}

impl<T> Socks5ServerProtocol<T, states::Opened> {
    pub fn start(inner: T) -> Self {
        Self::new(inner)
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin, A: Authentication> Socks5Socket<T, A> {
    pub fn new(socket: T, config: Arc<Config<A>>) -> Self {
        Socks5Socket {
            inner: socket,
            config,
            auth: AuthenticationMethod::None,
            target_addr: None,
            cmd: None,
            reply_ip: None,
            credentials: None,
        }
    }

    /// Set the bind IP address in Socks5Reply.
    ///
    /// Only the inner socket owner knows the correct reply bind addr, so leave this field to be
    /// populated. For those strict clients, users can use this function to set the correct IP
    /// address.
    ///
    /// Most popular SOCKS5 clients [1] [2] ignore BND.ADDR and BND.PORT the reply of command
    /// CONNECT, but this field could be useful in some other command, such as UDP ASSOCIATE.
    ///
    /// [1]: https://github.com/chromium/chromium/blob/bd2c7a8b65ec42d806277dd30f138a673dec233a/net/socket/socks5_client_socket.cc#L481
    /// [2]: https://github.com/curl/curl/blob/d15692ebbad5e9cfb871b0f7f51a73e43762cee2/lib/socks.c#L978
    pub fn set_reply_ip(&mut self, addr: IpAddr) {
        self.reply_ip = Some(addr);
    }

    /// Process clients SOCKS requests
    /// This is the entry point where a whole request is processed.
    pub async fn upgrade_to_socks5(mut self) -> Result<Socks5Socket<T, A>> {
        trace!("upgrading to socks5...");

        // Handshake
        let proto = if !self.config.skip_auth {
            let (proto, methods) = Socks5ServerProtocol::start(self.inner)
                .get_methods()
                .await?;

            let (proto, auth_method) = proto
                .can_accept_method(methods, self.config.as_ref())
                .await?;

            if self.config.auth.is_some() {
                let (proto, credentials) = proto
                    .authenticate(auth_method, self.config.as_ref())
                    .await?;
                self.credentials = Some(credentials);
                proto
            } else {
                Socks5ServerProtocol::new(proto.inner)
            }
        } else {
            debug!("skipping auth");
            Socks5ServerProtocol::new(self.inner)
        };

        let (proto, cmd, target_addr) = proto.read_command().await?;
        self.cmd = Some(match cmd {
            /* XXX: this is redundant, just to do it early before dns resolve? */
            Socks5Command::UDPAssociate if !self.config.allow_udp => {
                proto.reply_error(&ReplyError::CommandNotSupported).await?;
                return Err(ReplyError::CommandNotSupported.into());
            }
            Socks5Command::TCPBind => {
                proto.reply_error(&ReplyError::CommandNotSupported).await?;
                return Err(ReplyError::CommandNotSupported.into());
            }
            c => c,
        });
        self.target_addr = Some(target_addr);
        self.inner = proto.inner;

        if self.config.dns_resolve {
            self.resolve_dns().await?;
        } else {
            debug!("Domain won't be resolved because `dns_resolve`'s config has been turned off.")
        }

        if self.config.execute_command {
            /* we've just set it to Some above.
             * also, not gonna be used externally since we execute it here */
            let cmd = self.cmd.take().unwrap();
            let proto = Socks5ServerProtocol::<T, states::CommandRead>::new(self.inner);

            match cmd {
                Socks5Command::TCPBind => {
                    proto.reply_error(&ReplyError::CommandNotSupported).await?;
                    return Err(ReplyError::CommandNotSupported.into());
                }
                Socks5Command::TCPConnect => {
                    let addr = self
                        .target_addr
                        .as_ref()
                        .context("target_addr empty")?
                        .to_socket_addrs()?
                        .next()
                        .context("unreachable")?;

                    // TCP connect with timeout, to avoid memory leak for connection that takes forever
                    let outbound =
                        tcp_connect_with_timeout(addr, self.config.request_timeout).await?;

                    // Disable Nagle's algorithm if config specifies to do so.
                    outbound.set_nodelay(self.config.nodelay)?;

                    debug!("Connected to remote destination");

                    let mut inner = proto
                        .reply_success(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0))
                        .await?;

                    transfer(&mut inner, outbound).await?;
                    self.inner = inner;
                }
                Socks5Command::UDPAssociate => {
                    if self.config.allow_udp {
                        // The DST.ADDR and DST.PORT fields contain the address and port that
                        // the client expects to use to send UDP datagrams on for the
                        // association. The server MAY use this information to limit access
                        // to the association.
                        // @see Page 6, https://datatracker.ietf.org/doc/html/rfc1928.
                        //
                        // We do NOT limit the access from the client currently in this implementation.
                        let _not_used = self.target_addr.as_ref();

                        // Listen with UDP6 socket, so the client can connect to it with either
                        // IPv4 or IPv6.
                        let peer_sock = UdpSocket::bind("[::]:0").await?;

                        // Respect the pre-populated reply IP address.
                        self.inner = proto
                            .reply_success(SocketAddr::new(
                                self.reply_ip.context("invalid reply ip")?,
                                peer_sock.local_addr()?.port(),
                            ))
                            .await?;

                        transfer_udp(peer_sock).await?;
                    } else {
                        proto.reply_error(&ReplyError::CommandNotSupported).await?;
                        return Err(ReplyError::CommandNotSupported.into());
                    }
                }
            };
        }

        Ok(self)
    }

    /// Consumes the `Socks5Socket`, returning the wrapped stream.
    pub fn into_inner(self) -> T {
        self.inner
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Socks5ServerProtocol<T, states::Opened> {
    /// Read the authentication method provided by the client.
    /// A client send a list of methods that he supports, he could send
    ///
    ///   - 0: Non auth
    ///   - 2: Auth with username/password
    ///
    /// Altogether, then the server choose to use of of these,
    /// or deny the handshake (thus the connection).
    ///
    /// # Examples
    /// ```text
    ///                    {SOCKS Version, methods-length}
    ///     eg. (non-auth) {5, 2}
    ///     eg. (auth)     {5, 3}
    /// ```
    ///
    async fn get_methods(
        mut self,
    ) -> Result<(Socks5ServerProtocol<T, states::AuthMethodsRead>, Vec<u8>)> {
        trace!("Socks5Socket: get_methods()");
        // read the first 2 bytes which contains the SOCKS version and the methods len()
        let [version, methods_len] =
            read_exact!(self.inner, [0u8; 2]).context("Can't read methods")?;
        debug!(
            "Handshake headers: [version: {version}, methods len: {len}]",
            version = version,
            len = methods_len,
        );

        if version != consts::SOCKS5_VERSION {
            return Err(SocksError::UnsupportedSocksVersion(version));
        }

        // {METHODS available from the client}
        // eg. (non-auth) {0, 1}
        // eg. (auth)     {0, 1, 2}
        let methods = read_exact!(self.inner, vec![0u8; methods_len as usize])
            .context("Can't get methods.")?;
        debug!("methods supported sent by the client: {:?}", &methods);

        // Return methods available
        Ok((Socks5ServerProtocol::new(self.inner), methods))
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Socks5ServerProtocol<T, states::AuthMethodsRead> {
    /// Decide to whether or not, accept the authentication method.
    /// Don't forget that the methods list sent by the client, contains one or more methods.
    ///
    /// # Request
    ///
    ///  Client send an array of 3 entries: [0, 1, 2]
    /// ```text
    ///                          {SOCKS Version,  Authentication chosen}
    ///     eg. (non-auth)       {5, 0}
    ///     eg. (GSSAPI)         {5, 1}
    ///     eg. (auth)           {5, 2}
    /// ```
    ///
    /// # Response
    /// ```text
    ///     eg. (accept non-auth) {5, 0x00}
    ///     eg. (non-acceptable)  {5, 0xff}
    /// ```
    ///
    async fn can_accept_method<A: Authentication>(
        mut self,
        client_methods: Vec<u8>,
        config: &Config<A>,
    ) -> Result<(Socks5ServerProtocol<T, states::AuthMethodChosen>, u8)> {
        let method_supported;

        if let Some(_auth) = config.auth.as_ref() {
            if client_methods.contains(&consts::SOCKS5_AUTH_METHOD_PASSWORD) {
                // can auth with password
                method_supported = consts::SOCKS5_AUTH_METHOD_PASSWORD;
            } else {
                // client hasn't provided a password
                if config.allow_no_auth {
                    // but we allow no auth, for ip whitelisting
                    method_supported = consts::SOCKS5_AUTH_METHOD_NONE;
                } else {
                    // we don't allow no auth, so we deny the entry
                    debug!("Don't support this auth method, reply with (0xff)");
                    self.inner
                        .write_all(&[
                            consts::SOCKS5_VERSION,
                            consts::SOCKS5_AUTH_METHOD_NOT_ACCEPTABLE,
                        ])
                        .await
                        .context("Can't reply with method not acceptable.")?;

                    return Err(SocksError::AuthMethodUnacceptable(client_methods));
                }
            }
        } else {
            method_supported = consts::SOCKS5_AUTH_METHOD_NONE;
        }

        debug!(
            "Reply with method {} ({})",
            AuthenticationMethod::from_u8(method_supported).context("Method not supported")?,
            method_supported
        );
        self.inner
            .write(&[consts::SOCKS5_VERSION, method_supported])
            .await
            .context("Can't reply with method auth-none")?;
        Ok((Socks5ServerProtocol::new(self.inner), method_supported))
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Socks5ServerProtocol<T, states::AuthMethodChosen> {
    async fn read_username_password(socket: &mut T) -> Result<(String, String)> {
        trace!("Socks5Socket: authenticate()");
        let [version, user_len] = read_exact!(socket, [0u8; 2]).context("Can't read user len")?;
        debug!(
            "Auth: [version: {version}, user len: {len}]",
            version = version,
            len = user_len,
        );

        if user_len < 1 {
            return Err(SocksError::AuthenticationFailed(format!(
                "Username malformed ({} chars)",
                user_len
            )));
        }

        let username =
            read_exact!(socket, vec![0u8; user_len as usize]).context("Can't get username.")?;
        debug!("username bytes: {:?}", &username);

        let [pass_len] = read_exact!(socket, [0u8; 1]).context("Can't read pass len")?;
        debug!("Auth: [pass len: {len}]", len = pass_len,);

        if pass_len < 1 {
            return Err(SocksError::AuthenticationFailed(format!(
                "Password malformed ({} chars)",
                pass_len
            )));
        }

        let password =
            read_exact!(socket, vec![0u8; pass_len as usize]).context("Can't get password.")?;
        debug!("password bytes: {:?}", &password);

        let username = String::from_utf8(username).context("Failed to convert username")?;
        let password = String::from_utf8(password).context("Failed to convert password")?;

        Ok((username, password))
    }

    /// Only called if
    ///  - this server has `Authentication` trait implemented.
    ///  - and the client supports authentication via username/password
    ///  - or the client doesn't send authentication, but we let the trait decides if the `allow_no_auth()` set as `true`
    async fn authenticate<A: Authentication>(
        mut self,
        auth_method: u8,
        config: &Config<A>,
    ) -> Result<(Socks5ServerProtocol<T, states::Authenticated>, A::Item)> {
        let credentials = if auth_method == consts::SOCKS5_AUTH_METHOD_PASSWORD {
            let credentials = Self::read_username_password(&mut self.inner).await?;
            Some(credentials)
        } else {
            // the client hasn't provided any credentials, the function auth.authenticate()
            // will then check None, according to other parameters provided by the trait
            // such as IP, etc.
            None
        };

        let auth = config.auth.as_ref().context("No auth module")?;

        if let Some(credentials) = auth.authenticate(credentials).await {
            if auth_method == consts::SOCKS5_AUTH_METHOD_PASSWORD {
                // only the password way expect to write a response at this moment
                self.inner
                    .write_all(&[1, consts::SOCKS5_REPLY_SUCCEEDED])
                    .await
                    .context("Can't reply auth success")?;
            }

            info!("User logged successfully.");

            return Ok((Socks5ServerProtocol::new(self.inner), credentials));
        } else {
            self.inner
                .write_all(&[1, consts::SOCKS5_AUTH_METHOD_NOT_ACCEPTABLE])
                .await
                .context("Can't reply with auth method not acceptable.")?;

            return Err(SocksError::AuthenticationRejected(format!(
                "Authentication, rejected."
            )));
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Socks5ServerProtocol<T, states::CommandRead> {
    /// Reply success to the client according to the RFC.
    /// This consumes the wrapper as after this message actual proxying should begin.
    async fn reply_success(mut self, sock_addr: SocketAddr) -> Result<T> {
        self.inner
            .write(&new_reply(&ReplyError::Succeeded, sock_addr))
            .await
            .context("Can't write successful reply")?;

        self.inner.flush().await.context("Can't flush the reply!")?;

        debug!("Wrote success");
        Ok(self.inner)
    }

    /// Reply error to the client with the reply code according to the RFC.
    async fn reply_error(mut self, error: &ReplyError) -> Result<()> {
        let reply = new_reply(error, "0.0.0.0:0".parse().unwrap());
        debug!("reply error to be written: {:?}", &reply);

        self.inner
            .write(&reply)
            .await
            .context("Can't write the reply!")?;

        self.inner.flush().await.context("Can't flush the reply!")?;

        Ok(())
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Socks5ServerProtocol<T, states::Authenticated> {
    /// Decide to whether or not, accept the authentication method.
    /// Don't forget that the methods list sent by the client, contains one or more methods.
    ///
    /// # Request
    /// ```text
    ///          +----+-----+-------+------+----------+----------+
    ///          |VER | CMD |  RSV  | ATYP | DST.ADDR | DST.PORT |
    ///          +----+-----+-------+------+----------+----------+
    ///          | 1  |  1  |   1   |  1   | Variable |    2     |
    ///          +----+-----+-------+------+----------+----------+
    /// ```
    ///
    /// It the request is correct, it should returns a ['SocketAddr'].
    ///
    async fn read_command(
        mut self,
    ) -> Result<(
        Socks5ServerProtocol<T, states::CommandRead>,
        Socks5Command,
        TargetAddr,
    )> {
        let [version, cmd, rsv, address_type] =
            read_exact!(self.inner, [0u8; 4]).context("Malformed request")?;
        debug!(
            "Request: [version: {version}, command: {cmd}, rev: {rsv}, address_type: {address_type}]",
            version = version,
            cmd = cmd,
            rsv = rsv,
            address_type = address_type,
        );

        if version != consts::SOCKS5_VERSION {
            return Err(SocksError::UnsupportedSocksVersion(version));
        }

        let cmd = Socks5Command::from_u8(cmd).ok_or(ReplyError::CommandNotSupported)?;

        // Guess address type
        let target_addr = read_address(&mut self.inner, address_type)
            .await
            .map_err(|e| {
                // print explicit error
                error!("{:#}", e);
                // then convert it to a reply
                ReplyError::AddressTypeNotSupported
            })?;

        debug!("Request target is {}", target_addr);

        Ok((Socks5ServerProtocol::new(self.inner), cmd, target_addr))
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin, A: Authentication> Socks5Socket<T, A> {
    /// This function is public, it can be call manually on your own-willing
    /// if config flag has been turned off: `Config::dns_resolve == false`.
    pub async fn resolve_dns(&mut self) -> Result<()> {
        trace!("resolving dns");
        if let Some(target_addr) = self.target_addr.take() {
            // decide whether we have to resolve DNS or not
            self.target_addr = match target_addr {
                TargetAddr::Domain(_, _) => Some(target_addr.resolve_dns().await?),
                TargetAddr::Ip(_) => Some(target_addr),
            };
        }

        Ok(())
    }

    pub fn target_addr(&self) -> Option<&TargetAddr> {
        self.target_addr.as_ref()
    }

    pub fn auth(&self) -> &AuthenticationMethod {
        &self.auth
    }

    pub fn cmd(&self) -> &Option<Socks5Command> {
        &self.cmd
    }

    /// Borrow the credentials of the user has authenticated with
    pub fn get_credentials(&self) -> Option<&<<A as Authentication>::Item as Deref>::Target>
    where
        <A as Authentication>::Item: Deref,
    {
        self.credentials.as_deref()
    }

    /// Get the credentials of the user has authenticated with
    pub fn take_credentials(&mut self) -> Option<A::Item> {
        self.credentials.take()
    }
}

/// Copy data between two peers
/// Using 2 different generators, because they could be different structs with same traits.
async fn transfer<I, O>(mut inbound: I, mut outbound: O) -> Result<()>
where
    I: AsyncRead + AsyncWrite + Unpin,
    O: AsyncRead + AsyncWrite + Unpin,
{
    match tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await {
        Ok(res) => info!("transfer closed ({}, {})", res.0, res.1),
        Err(err) => error!("transfer error: {:?}", err),
    };

    Ok(())
}

async fn handle_udp_request(inbound: &UdpSocket, outbound: &UdpSocket) -> Result<()> {
    let mut buf = vec![0u8; 0x10000];
    loop {
        let (size, client_addr) = inbound.recv_from(&mut buf).await?;
        debug!("Server recieve udp from {}", client_addr);
        inbound.connect(client_addr).await?;

        let (frag, target_addr, data) = parse_udp_request(&buf[..size]).await?;

        if frag != 0 {
            debug!("Discard UDP frag packets sliently.");
            return Ok(());
        }

        debug!("Server forward to packet to {}", target_addr);
        let mut target_addr = target_addr
            .resolve_dns()
            .await?
            .to_socket_addrs()?
            .next()
            .context("unreachable")?;

        target_addr.set_ip(match target_addr.ip() {
            std::net::IpAddr::V4(v4) => std::net::IpAddr::V6(v4.to_ipv6_mapped()),
            v6 @ std::net::IpAddr::V6(_) => v6,
        });
        outbound.send_to(data, target_addr).await?;
    }
}

async fn handle_udp_response(inbound: &UdpSocket, outbound: &UdpSocket) -> Result<()> {
    let mut buf = vec![0u8; 0x10000];
    loop {
        let (size, remote_addr) = outbound.recv_from(&mut buf).await?;
        debug!("Recieve packet from {}", remote_addr);

        let mut data = new_udp_header(remote_addr)?;
        data.extend_from_slice(&buf[..size]);
        inbound.send(&data).await?;
    }
}

async fn transfer_udp(inbound: UdpSocket) -> Result<()> {
    let outbound = UdpSocket::bind("[::]:0").await?;

    let req_fut = handle_udp_request(&inbound, &outbound);
    let res_fut = handle_udp_response(&inbound, &outbound);
    match try_join!(req_fut, res_fut) {
        Ok(_) => {}
        Err(error) => return Err(error),
    }

    Ok(())
}

// Fixes the issue "cannot borrow data in dereference of `Pin<&mut >` as mutable"
//
// cf. https://users.rust-lang.org/t/take-in-impl-future-cannot-borrow-data-in-a-dereference-of-pin/52042
impl<T, A: Authentication> Unpin for Socks5Socket<T, A> where T: AsyncRead + AsyncWrite + Unpin {}

/// Allow us to read directly from the struct
impl<T, A: Authentication> AsyncRead for Socks5Socket<T, A>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buf)
    }
}

/// Allow us to write directly into the struct
impl<T, A: Authentication> AsyncWrite for Socks5Socket<T, A>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(context, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

/// Generate reply code according to the RFC.
fn new_reply(error: &ReplyError, sock_addr: SocketAddr) -> Vec<u8> {
    let (addr_type, mut ip_oct, mut port) = match sock_addr {
        SocketAddr::V4(sock) => (
            consts::SOCKS5_ADDR_TYPE_IPV4,
            sock.ip().octets().to_vec(),
            sock.port().to_be_bytes().to_vec(),
        ),
        SocketAddr::V6(sock) => (
            consts::SOCKS5_ADDR_TYPE_IPV6,
            sock.ip().octets().to_vec(),
            sock.port().to_be_bytes().to_vec(),
        ),
    };

    let mut reply = vec![
        consts::SOCKS5_VERSION,
        error.as_u8(), // transform the error into byte code
        0x00,          // reserved
        addr_type,     // address type (ipv4, v6, domain)
    ];
    reply.append(&mut ip_oct);
    reply.append(&mut port);

    reply
}

#[cfg(test)]
mod test {
    use crate::server::Socks5Server;
    use tokio_test::block_on;

    use super::AcceptAuthentication;

    #[test]
    fn test_bind() {
        let f = async {
            let _server = Socks5Server::<AcceptAuthentication>::bind("127.0.0.1:1080")
                .await
                .unwrap();
        };

        block_on(f);
    }
}
