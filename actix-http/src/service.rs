use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::{fmt, net, rc::Rc};

use actix_codec::{AsyncRead, AsyncWrite, Framed};
use actix_rt::net::TcpStream;
use actix_service::{pipeline_factory, IntoServiceFactory, Service, ServiceFactory};
use bytes::Bytes;
use futures_core::{ready, Future};
use h2::server::{self, Handshake};
use pin_project::pin_project;

use crate::body::MessageBody;
use crate::builder::HttpServiceBuilder;
use crate::config::{KeepAlive, ServiceConfig};
use crate::error::{DispatchError, Error};
use crate::request::Request;
use crate::response::Response;
use crate::{h1, h2::Dispatcher, ConnectCallback, OnConnectData, Protocol};

/// A `ServiceFactory` for HTTP/1.1 or HTTP/2 protocol.
pub struct HttpService<T, S, B, X = h1::ExpectHandler, U = h1::UpgradeHandler> {
    srv: S,
    cfg: ServiceConfig,
    expect: X,
    upgrade: Option<U>,
    on_connect_ext: Option<Rc<ConnectCallback<T>>>,
    _phantom: PhantomData<B>,
}

impl<T, S, B> HttpService<T, S, B>
where
    S: ServiceFactory<Request, Config = ()>,
    S::Error: Into<Error> + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service<Request>>::Future: 'static,
    B: MessageBody + 'static,
{
    /// Create builder for `HttpService` instance.
    pub fn build() -> HttpServiceBuilder<T, S> {
        HttpServiceBuilder::new()
    }
}

impl<T, S, B> HttpService<T, S, B>
where
    S: ServiceFactory<Request, Config = ()>,
    S::Error: Into<Error> + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service<Request>>::Future: 'static,
    B: MessageBody + 'static,
{
    /// Create new `HttpService` instance.
    pub fn new<F: IntoServiceFactory<S, Request>>(service: F) -> Self {
        let cfg = ServiceConfig::new(KeepAlive::Timeout(5), 5000, 0, false, None);

        HttpService {
            cfg,
            srv: service.into_factory(),
            expect: h1::ExpectHandler,
            upgrade: None,
            on_connect_ext: None,
            _phantom: PhantomData,
        }
    }

    /// Create new `HttpService` instance with config.
    pub(crate) fn with_config<F: IntoServiceFactory<S, Request>>(
        cfg: ServiceConfig,
        service: F,
    ) -> Self {
        HttpService {
            cfg,
            srv: service.into_factory(),
            expect: h1::ExpectHandler,
            upgrade: None,
            on_connect_ext: None,
            _phantom: PhantomData,
        }
    }
}

impl<T, S, B, X, U> HttpService<T, S, B, X, U>
where
    S: ServiceFactory<Request, Config = ()>,
    S::Error: Into<Error> + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service<Request>>::Future: 'static,
    B: MessageBody,
{
    /// Provide service for `EXPECT: 100-Continue` support.
    ///
    /// Service get called with request that contains `EXPECT` header.
    /// Service must return request in case of success, in that case
    /// request will be forwarded to main service.
    pub fn expect<X1>(self, expect: X1) -> HttpService<T, S, B, X1, U>
    where
        X1: ServiceFactory<Request, Config = (), Response = Request>,
        X1::Error: Into<Error>,
        X1::InitError: fmt::Debug,
        <X1::Service as Service<Request>>::Future: 'static,
    {
        HttpService {
            expect,
            cfg: self.cfg,
            srv: self.srv,
            upgrade: self.upgrade,
            on_connect_ext: self.on_connect_ext,
            _phantom: PhantomData,
        }
    }

    /// Provide service for custom `Connection: UPGRADE` support.
    ///
    /// If service is provided then normal requests handling get halted
    /// and this service get called with original request and framed object.
    pub fn upgrade<U1>(self, upgrade: Option<U1>) -> HttpService<T, S, B, X, U1>
    where
        U1: ServiceFactory<(Request, Framed<T, h1::Codec>), Config = (), Response = ()>,
        U1::Error: fmt::Display,
        U1::InitError: fmt::Debug,
        <U1::Service as Service<(Request, Framed<T, h1::Codec>)>>::Future: 'static,
    {
        HttpService {
            upgrade,
            cfg: self.cfg,
            srv: self.srv,
            expect: self.expect,
            on_connect_ext: self.on_connect_ext,
            _phantom: PhantomData,
        }
    }

    /// Set connect callback with mutable access to request data container.
    pub(crate) fn on_connect_ext(mut self, f: Option<Rc<ConnectCallback<T>>>) -> Self {
        self.on_connect_ext = f;
        self
    }
}

impl<S, B, X, U> HttpService<TcpStream, S, B, X, U>
where
    S: ServiceFactory<Request, Config = ()>,
    S::Error: Into<Error> + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service<Request>>::Future: 'static,
    B: MessageBody + 'static,
    X: ServiceFactory<Request, Config = (), Response = Request>,
    X::Error: Into<Error>,
    X::InitError: fmt::Debug,
    <X::Service as Service<Request>>::Future: 'static,
    U: ServiceFactory<
        (Request, Framed<TcpStream, h1::Codec>),
        Config = (),
        Response = (),
    >,
    U::Error: fmt::Display + Into<Error>,
    U::InitError: fmt::Debug,
    <U::Service as Service<(Request, Framed<TcpStream, h1::Codec>)>>::Future: 'static,
{
    /// Create simple tcp stream service
    pub fn tcp(
        self,
    ) -> impl ServiceFactory<
        TcpStream,
        Config = (),
        Response = (),
        Error = DispatchError,
        InitError = (),
    > {
        pipeline_factory(|io: TcpStream| async {
            let peer_addr = io.peer_addr().ok();
            Ok((io, Protocol::Http1, peer_addr))
        })
        .and_then(self)
    }
}

#[cfg(feature = "openssl")]
mod openssl {
    use super::*;
    use actix_service::ServiceFactoryExt;
    use actix_tls::accept::openssl::{Acceptor, SslAcceptor, SslError, SslStream};
    use actix_tls::accept::TlsError;

    impl<S, B, X, U> HttpService<SslStream<TcpStream>, S, B, X, U>
    where
        S: ServiceFactory<Request, Config = ()>,
        S::Error: Into<Error> + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
        <S::Service as Service<Request>>::Future: 'static,
        B: MessageBody + 'static,
        X: ServiceFactory<Request, Config = (), Response = Request>,
        X::Error: Into<Error>,
        X::InitError: fmt::Debug,
        <X::Service as Service<Request>>::Future: 'static,
        U: ServiceFactory<
            (Request, Framed<SslStream<TcpStream>, h1::Codec>),
            Config = (),
            Response = (),
        >,
        U::Error: fmt::Display + Into<Error>,
        U::InitError: fmt::Debug,
        <U::Service as Service<(Request, Framed<SslStream<TcpStream>, h1::Codec>)>>::Future: 'static,
    {
        /// Create openssl based service
        pub fn openssl(
            self,
            acceptor: SslAcceptor,
        ) -> impl ServiceFactory<
            TcpStream,
            Config = (),
            Response = (),
            Error = TlsError<SslError, DispatchError>,
            InitError = (),
        > {
            pipeline_factory(
                Acceptor::new(acceptor)
                    .map_err(TlsError::Tls)
                    .map_init_err(|_| panic!()),
            )
            .and_then(|io: SslStream<TcpStream>| async {
                let proto = if let Some(protos) = io.ssl().selected_alpn_protocol() {
                    if protos.windows(2).any(|window| window == b"h2") {
                        Protocol::Http2
                    } else {
                        Protocol::Http1
                    }
                } else {
                    Protocol::Http1
                };
                let peer_addr = io.get_ref().peer_addr().ok();
                Ok((io, proto, peer_addr))
            })
            .and_then(self.map_err(TlsError::Service))
        }
    }
}

#[cfg(feature = "rustls")]
mod rustls {
    use std::io;

    use actix_tls::accept::rustls::{Acceptor, ServerConfig, Session, TlsStream};
    use actix_tls::accept::TlsError;

    use super::*;
    use actix_service::ServiceFactoryExt;

    impl<S, B, X, U> HttpService<TlsStream<TcpStream>, S, B, X, U>
    where
        S: ServiceFactory<Request, Config = ()>,
        S::Error: Into<Error> + 'static,
        S::InitError: fmt::Debug,
        S::Response: Into<Response<B>> + 'static,
        <S::Service as Service<Request>>::Future: 'static,
        B: MessageBody + 'static,
        X: ServiceFactory<Request, Config = (), Response = Request>,
        X::Error: Into<Error>,
        X::InitError: fmt::Debug,
        <X::Service as Service<Request>>::Future: 'static,
        U: ServiceFactory<
            (Request, Framed<TlsStream<TcpStream>, h1::Codec>),
            Config = (),
            Response = (),
        >,
        U::Error: fmt::Display + Into<Error>,
        U::InitError: fmt::Debug,
        <U::Service as Service<(Request, Framed<TlsStream<TcpStream>, h1::Codec>)>>::Future: 'static,
    {
        /// Create openssl based service
        pub fn rustls(
            self,
            mut config: ServerConfig,
        ) -> impl ServiceFactory<
            TcpStream,
            Config = (),
            Response = (),
            Error = TlsError<io::Error, DispatchError>,
            InitError = (),
        > {
            let protos = vec!["h2".to_string().into(), "http/1.1".to_string().into()];
            config.set_protocols(&protos);

            pipeline_factory(
                Acceptor::new(config)
                    .map_err(TlsError::Tls)
                    .map_init_err(|_| panic!()),
            )
            .and_then(|io: TlsStream<TcpStream>| async {
                let proto = if let Some(protos) = io.get_ref().1.get_alpn_protocol() {
                    if protos.windows(2).any(|window| window == b"h2") {
                        Protocol::Http2
                    } else {
                        Protocol::Http1
                    }
                } else {
                    Protocol::Http1
                };
                let peer_addr = io.get_ref().0.peer_addr().ok();
                Ok((io, proto, peer_addr))
            })
            .and_then(self.map_err(TlsError::Service))
        }
    }
}

impl<T, S, B, X, U> ServiceFactory<(T, Protocol, Option<net::SocketAddr>)>
    for HttpService<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: ServiceFactory<Request, Config = ()>,
    S::Error: Into<Error> + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service<Request>>::Future: 'static,
    B: MessageBody + 'static,
    X: ServiceFactory<Request, Config = (), Response = Request>,
    X::Error: Into<Error>,
    X::InitError: fmt::Debug,
    <X::Service as Service<Request>>::Future: 'static,
    U: ServiceFactory<(Request, Framed<T, h1::Codec>), Config = (), Response = ()>,
    U::Error: fmt::Display + Into<Error>,
    U::InitError: fmt::Debug,
    <U::Service as Service<(Request, Framed<T, h1::Codec>)>>::Future: 'static,
{
    type Response = ();
    type Error = DispatchError;
    type Config = ();
    type Service = HttpServiceHandler<T, S::Service, B, X::Service, U::Service>;
    type InitError = ();
    type Future = HttpServiceResponse<T, S, B, X, U>;

    fn new_service(&self, _: ()) -> Self::Future {
        HttpServiceResponse {
            fut: self.srv.new_service(()),
            fut_ex: Some(self.expect.new_service(())),
            fut_upg: self.upgrade.as_ref().map(|f| f.new_service(())),
            expect: None,
            upgrade: None,
            on_connect_ext: self.on_connect_ext.clone(),
            cfg: self.cfg.clone(),
            _phantom: PhantomData,
        }
    }
}

#[doc(hidden)]
#[pin_project]
pub struct HttpServiceResponse<T, S, B, X, U>
where
    S: ServiceFactory<Request>,
    X: ServiceFactory<Request>,
    U: ServiceFactory<(Request, Framed<T, h1::Codec>)>,
{
    #[pin]
    fut: S::Future,
    #[pin]
    fut_ex: Option<X::Future>,
    #[pin]
    fut_upg: Option<U::Future>,
    expect: Option<X::Service>,
    upgrade: Option<U::Service>,
    on_connect_ext: Option<Rc<ConnectCallback<T>>>,
    cfg: ServiceConfig,
    _phantom: PhantomData<B>,
}

impl<T, S, B, X, U> Future for HttpServiceResponse<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: ServiceFactory<Request>,
    S::Error: Into<Error> + 'static,
    S::InitError: fmt::Debug,
    S::Response: Into<Response<B>> + 'static,
    <S::Service as Service<Request>>::Future: 'static,
    B: MessageBody + 'static,
    X: ServiceFactory<Request, Response = Request>,
    X::Error: Into<Error>,
    X::InitError: fmt::Debug,
    <X::Service as Service<Request>>::Future: 'static,
    U: ServiceFactory<(Request, Framed<T, h1::Codec>), Response = ()>,
    U::Error: fmt::Display,
    U::InitError: fmt::Debug,
    <U::Service as Service<(Request, Framed<T, h1::Codec>)>>::Future: 'static,
{
    type Output =
        Result<HttpServiceHandler<T, S::Service, B, X::Service, U::Service>, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();

        if let Some(fut) = this.fut_ex.as_pin_mut() {
            let expect = ready!(fut
                .poll(cx)
                .map_err(|e| log::error!("Init http service error: {:?}", e)))?;
            this = self.as_mut().project();
            *this.expect = Some(expect);
            this.fut_ex.set(None);
        }

        if let Some(fut) = this.fut_upg.as_pin_mut() {
            let upgrade = ready!(fut
                .poll(cx)
                .map_err(|e| log::error!("Init http service error: {:?}", e)))?;
            this = self.as_mut().project();
            *this.upgrade = Some(upgrade);
            this.fut_upg.set(None);
        }

        let result = ready!(this
            .fut
            .poll(cx)
            .map_err(|e| log::error!("Init http service error: {:?}", e)));

        Poll::Ready(result.map(|service| {
            let this = self.as_mut().project();
            HttpServiceHandler::new(
                this.cfg.clone(),
                service,
                this.expect.take().unwrap(),
                this.upgrade.take(),
                this.on_connect_ext.clone(),
            )
        }))
    }
}

/// `Service` implementation for HTTP transport
pub struct HttpServiceHandler<T, S, B, X, U>
where
    S: Service<Request>,
    X: Service<Request>,
    U: Service<(Request, Framed<T, h1::Codec>)>,
{
    flow: Rc<HttpFlow<S, X, U>>,
    cfg: ServiceConfig,
    on_connect_ext: Option<Rc<ConnectCallback<T>>>,
    _phantom: PhantomData<B>,
}

/// A collection of services that describe an HTTP request flow.
pub(super) struct HttpFlow<S, X, U> {
    pub(super) service: S,
    pub(super) expect: X,
    pub(super) upgrade: Option<U>,
}

impl<S, X, U> HttpFlow<S, X, U> {
    pub(super) fn new(service: S, expect: X, upgrade: Option<U>) -> Rc<Self> {
        Rc::new(Self {
            service,
            expect,
            upgrade,
        })
    }
}

impl<T, S, B, X, U> HttpServiceHandler<T, S, B, X, U>
where
    S: Service<Request>,
    S::Error: Into<Error> + 'static,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
    X: Service<Request, Response = Request>,
    X::Error: Into<Error>,
    U: Service<(Request, Framed<T, h1::Codec>), Response = ()>,
    U::Error: fmt::Display,
{
    fn new(
        cfg: ServiceConfig,
        service: S,
        expect: X,
        upgrade: Option<U>,
        on_connect_ext: Option<Rc<ConnectCallback<T>>>,
    ) -> HttpServiceHandler<T, S, B, X, U> {
        HttpServiceHandler {
            cfg,
            on_connect_ext,
            flow: HttpFlow::new(service, expect, upgrade),
            _phantom: PhantomData,
        }
    }
}

impl<T, S, B, X, U> Service<(T, Protocol, Option<net::SocketAddr>)>
    for HttpServiceHandler<T, S, B, X, U>
where
    T: AsyncRead + AsyncWrite + Unpin,
    S: Service<Request>,
    S::Error: Into<Error> + 'static,
    S::Future: 'static,
    S::Response: Into<Response<B>> + 'static,
    B: MessageBody + 'static,
    X: Service<Request, Response = Request>,
    X::Error: Into<Error>,
    U: Service<(Request, Framed<T, h1::Codec>), Response = ()>,
    U::Error: fmt::Display + Into<Error>,
{
    type Response = ();
    type Error = DispatchError;
    type Future = impl Future<Output = Result<(), DispatchError>>;

    fn poll_ready(&self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let ready = self
            .flow
            .expect
            .poll_ready(cx)
            .map_err(|e| {
                let e = e.into();
                log::error!("Http service readiness error: {:?}", e);
                DispatchError::Service(e)
            })?
            .is_ready();

        let ready = self
            .flow
            .service
            .poll_ready(cx)
            .map_err(|e| {
                let e = e.into();
                log::error!("Http service readiness error: {:?}", e);
                DispatchError::Service(e)
            })?
            .is_ready()
            && ready;

        let ready = if let Some(ref upg) = self.flow.upgrade {
            upg.poll_ready(cx)
                .map_err(|e| {
                    let e = e.into();
                    log::error!("Http service readiness error: {:?}", e);
                    DispatchError::Service(e)
                })?
                .is_ready()
                && ready
        } else {
            ready
        };

        if ready {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }

    fn call(
        &self,
        (io, proto, peer_addr): (T, Protocol, Option<net::SocketAddr>),
    ) -> Self::Future {
        let on_connect_data =
            OnConnectData::from_io(&io, self.on_connect_ext.as_deref());
        let config = self.cfg.clone();
        let flow = self.flow.clone();

        async move {
            match proto {
                Protocol::Http2 => {
                    let connection = server::handshake(io).await.map_err(|e| {
                        trace!("H2 handshake error: {}", e);
                        e
                    })?;
                    Dispatcher::new(
                        flow,
                        connection,
                        on_connect_data,
                        config,
                        None,
                        peer_addr,
                    )
                    .await
                }
                Protocol::Http1 => {
                    crate::h1::h1_dispatch(io, flow, on_connect_data, peer_addr, config)
                        .await
                }
                proto => unimplemented!("Unsupported HTTP version: {:?}.", proto),
            }
        }
    }
}
