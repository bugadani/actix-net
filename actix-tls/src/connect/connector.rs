use std::{
    collections::VecDeque,
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

use actix_rt::net::TcpStream;
use actix_service::{Service, ServiceFactory};
use futures_core::{future::LocalBoxFuture, ready};
use log::{error, trace};
use tokio_util::sync::ReusableBoxFuture;

use super::connect::{Address, Connect, ConnectAddrs, Connection};
use super::error::ConnectError;

/// TCP connector service factory
#[derive(Debug, Copy, Clone)]
pub struct TcpConnectorFactory;

impl TcpConnectorFactory {
    /// Create TCP connector service
    pub fn service(&self) -> TcpConnector {
        TcpConnector
    }
}

impl<T: Address> ServiceFactory<Connect<T>> for TcpConnectorFactory {
    type Response = Connection<T, TcpStream>;
    type Error = ConnectError;
    type Config = ();
    type Service = TcpConnector;
    type InitError = ();
    type Future = LocalBoxFuture<'static, Result<Self::Service, Self::InitError>>;

    fn new_service(&self, _: ()) -> Self::Future {
        let service = self.service();
        Box::pin(async move { Ok(service) })
    }
}

/// TCP connector service
#[derive(Debug, Copy, Clone)]
pub struct TcpConnector;

impl<T: Address> Service<Connect<T>> for TcpConnector {
    type Response = Connection<T, TcpStream>;
    type Error = ConnectError;
    type Future = TcpConnectorResponse<T>;

    actix_service::always_ready!();

    fn call(&self, req: Connect<T>) -> Self::Future {
        let port = req.port();
        let Connect { req, addr, .. } = req;

        TcpConnectorResponse::new(req, port, addr)
    }
}

/// TCP stream connector response future
pub enum TcpConnectorResponse<T> {
    Response {
        req: Option<T>,
        port: u16,
        addrs: Option<VecDeque<SocketAddr>>,
        stream: Option<ReusableBoxFuture<Result<TcpStream, io::Error>>>,
    },
    Error(Option<ConnectError>),
}

impl<T: Address> TcpConnectorResponse<T> {
    pub(crate) fn new(req: T, port: u16, addr: ConnectAddrs) -> TcpConnectorResponse<T> {
        if addr.is_none() {
            error!("TCP connector: unresolved connection address");
            return TcpConnectorResponse::Error(Some(ConnectError::Unresolved));
        }

        trace!(
            "TCP connector: connecting to {} on port {}",
            req.hostname(),
            port
        );

        match addr {
            ConnectAddrs::None => unreachable!("none variant already checked"),

            ConnectAddrs::One(addr) => TcpConnectorResponse::Response {
                req: Some(req),
                port,
                addrs: None,
                stream: Some(ReusableBoxFuture::new(TcpStream::connect(addr))),
            },

            // when resolver returns multiple socket addr for request they would be popped from
            // front end of queue and returns with the first successful tcp connection.
            ConnectAddrs::Multi(addrs) => TcpConnectorResponse::Response {
                req: Some(req),
                port,
                addrs: Some(addrs),
                stream: None,
            },
        }
    }
}

impl<T: Address> Future for TcpConnectorResponse<T> {
    type Output = Result<Connection<T, TcpStream>, ConnectError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut() {
            TcpConnectorResponse::Error(err) => Poll::Ready(Err(err.take().unwrap())),

            TcpConnectorResponse::Response {
                req,
                port,
                addrs,
                stream,
            } => loop {
                if let Some(new) = stream.as_mut() {
                    match ready!(new.poll(cx)) {
                        Ok(sock) => {
                            let req = req.take().unwrap();
                            trace!(
                                "TCP connector: successfully connected to {:?} - {:?}",
                                req.hostname(),
                                sock.peer_addr()
                            );
                            return Poll::Ready(Ok(Connection::new(sock, req)));
                        }

                        Err(err) => {
                            trace!(
                                "TCP connector: failed to connect to {:?} port: {}",
                                req.as_ref().unwrap().hostname(),
                                port,
                            );

                            if addrs.is_none() || addrs.as_ref().unwrap().is_empty() {
                                return Poll::Ready(Err(ConnectError::Io(err)));
                            }
                        }
                    }
                }

                // try to connect
                let addr = addrs.as_mut().unwrap().pop_front().unwrap();

                match stream {
                    Some(rbf) => rbf.set(TcpStream::connect(addr)),
                    None => *stream = Some(ReusableBoxFuture::new(TcpStream::connect(addr))),
                }
            },
        }
    }
}
