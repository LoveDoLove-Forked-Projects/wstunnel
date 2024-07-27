use anyhow::Context;
use std::future::Future;

use bytes::Bytes;
use log::{debug, error};
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;

use base64::Engine;
use futures_util::{future, stream, Stream};
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioTimer;
use parking_lot::Mutex;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tracing::log::info;
use url::Host;

#[allow(clippy::type_complexity)]
pub struct HttpProxyListener {
    listener: Pin<Box<dyn Stream<Item = anyhow::Result<(TcpStream, (Host, u16))>> + Send>>,
}

impl Stream for HttpProxyListener {
    type Item = anyhow::Result<(TcpStream, (Host, u16))>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        unsafe { self.map_unchecked_mut(|x| &mut x.listener) }.poll_next(cx)
    }
}

fn server_client(
    credentials: &Option<String>,
    dest: &Mutex<(Host, u16)>,
    req: Request<Incoming>,
) -> impl Future<Output = Result<Response<Empty<Bytes>>, &'static str>> {
    const PROXY_AUTHORIZATION_PREFIX: &str = "Basic ";
    let ok_response = |forward_to: (Host, u16)| -> Result<Response<Empty<Bytes>>, _> {
        *dest.lock() = forward_to;
        Ok(Response::builder().status(200).body(Empty::new()).unwrap())
    };
    fn err_response() -> Result<Response<Empty<Bytes>>, &'static str> {
        info!("Un-authorized connection to http proxy");
        Err("Un-authorized")
    }

    if req.method() != hyper::Method::CONNECT {
        return future::ready(err_response());
    }

    debug!("HTTP Proxy CONNECT request to {}", req.uri());
    let forward_to = (
        Host::parse(req.uri().host().unwrap_or_default()).unwrap_or(Host::Ipv4(Ipv4Addr::new(0, 0, 0, 0))),
        req.uri().port_u16().unwrap_or(443),
    );

    let Some(token) = credentials else {
        return future::ready(ok_response(forward_to));
    };

    let Some(auth) = req.headers().get(hyper::header::PROXY_AUTHORIZATION) else {
        return future::ready(err_response());
    };

    let auth = auth.to_str().unwrap_or_default().trim();
    if auth.starts_with(PROXY_AUTHORIZATION_PREFIX) && &auth[PROXY_AUTHORIZATION_PREFIX.len()..] == token {
        return future::ready(ok_response(forward_to));
    }

    future::ready(err_response())
}

pub async fn run_server(
    bind: SocketAddr,
    timeout: Option<Duration>,
    credentials: Option<(String, String)>,
) -> Result<HttpProxyListener, anyhow::Error> {
    info!(
        "Starting http proxy server listening cnx on {} with credentials {:?}",
        bind, credentials
    );

    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("Cannot create TCP server {:?}", bind))?;

    let http1 = {
        let mut builder = http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(timeout)
            .keep_alive(false);
        builder
    };
    let auth_header =
        credentials.map(|(user, pass)| base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", user, pass)));

    let listener = stream::unfold((listener, http1, auth_header), |(listener, http1, auth_header)| async {
        loop {
            let (mut stream, _) = match listener.accept().await {
                Ok(v) => v,
                Err(err) => {
                    error!("Error while accepting connection {:?}", err);
                    continue;
                }
            };

            let forward_to = Mutex::new((Host::Ipv4(Ipv4Addr::new(0, 0, 0, 0)), 0));
            let conn_fut = http1.serve_connection(
                hyper_util::rt::TokioIo::new(&mut stream),
                service_fn(|req| server_client(&auth_header, &forward_to, req)),
            );
            match conn_fut.await {
                Ok(_) => return Some((Ok((stream, forward_to.into_inner())), (listener, http1, auth_header))),
                Err(err) => {
                    info!("Error while serving connection: {}", err);
                    continue;
                }
            }
        }
    });

    Ok(HttpProxyListener {
        listener: Box::pin(listener),
    })
}

//#[cfg(test)]
//mod tests {
//    use super::*;
//    use tracing::level_filters::LevelFilter;
//
//    #[tokio::test]
//    async fn test_run_server() {
//        tracing_subscriber::fmt()
//            .with_ansi(true)
//            .with_max_level(LevelFilter::TRACE)
//            .init();
//        let x = run_server("127.0.0.1:1212".parse().unwrap(), None, None).await;
//    }
//}
