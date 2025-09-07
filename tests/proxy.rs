use std::convert::Infallible;
use std::io::{ErrorKind};
use std::net::{SocketAddr, IpAddr, Ipv4Addr};
use std::time::Duration;

use cmux_proxy::ProxyConfig;
use hyper::body::to_bytes;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server, StatusCode, Client};
use hyper::client::HttpConnector;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::timeout;

async fn start_upstream_http() -> SocketAddr {
    let make_svc = make_service_fn(|_conn| async move {
        Ok::<_, Infallible>(service_fn(|req: Request<Body>| async move {
            let body = format!("ok:{}:{}", req.method(), req.uri().path());
            Ok::<_, Infallible>(Response::new(Body::from(body)))
        }))
    });
    let addr: SocketAddr = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into();
    let server = Server::bind(&addr).serve(make_svc);
    let local = server.local_addr();
    tokio::spawn(server);
    local
}

async fn start_upstream_ws_like_upgrade_echo() -> SocketAddr {
    use hyper::header::{CONNECTION, UPGRADE};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let make_svc = make_service_fn(|_conn| async move {
        Ok::<_, Infallible>(service_fn(|mut req: Request<Body>| async move {
            let is_upgrade = req
                .headers()
                .get(CONNECTION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_ascii_lowercase().contains("upgrade"))
                .unwrap_or(false)
                && req.headers().contains_key(UPGRADE);

            if is_upgrade {
                let resp = Response::builder()
                    .status(StatusCode::SWITCHING_PROTOCOLS)
                    .header(CONNECTION, "upgrade")
                    .header(UPGRADE, "websocket")
                    .body(Body::empty())
                    .unwrap();

                tokio::spawn(async move {
                    match hyper::upgrade::on(&mut req).await {
                        Ok(mut upgraded) => {
                            let mut buf = [0u8; 1024];
                            loop {
                                match upgraded.read(&mut buf).await {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        if upgraded.write_all(&buf[..n]).await.is_err() { break; }
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                        Err(_) => {}
                    }
                });

                return Ok::<_, Infallible>(resp);
            }

            Ok::<_, Infallible>(Response::builder().status(400).body(Body::from("no upgrade")).unwrap())
        }))
    });

    let addr: SocketAddr = (IpAddr::V4(Ipv4Addr::LOCALHOST), 0).into();
    let server = Server::bind(&addr).serve(make_svc);
    let local = server.local_addr();
    tokio::spawn(server);
    local
}

async fn start_upstream_tcp_echo() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.unwrap();
    let local = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = vec![0u8; 1024];
            loop {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if stream.write_all(&buf[..n]).await.is_err() { break; }
                    }
                    Err(e) => {
                        if e.kind() != ErrorKind::WouldBlock { break; }
                    }
                }
            }
        }
    });
    (local, handle)
}

async fn start_proxy(listen: SocketAddr, upstream_host: &str) -> (SocketAddr, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let cfg = ProxyConfig { listen, upstream_host: upstream_host.to_string() };
    let (tx, rx) = oneshot::channel::<()>();
    let (bound, handle) = cmux_proxy::spawn_proxy(cfg, async move { let _ = rx.await; });
    (bound, tx, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_http_proxy_routes_by_header() {
    let upstream_addr = start_upstream_http().await;
    let (proxy_addr, shutdown, handle) = start_proxy(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), "127.0.0.1").await;

    // Build client
    let client: Client<HttpConnector, Body> = Client::new();
    let url = format!("http://{}:{}/hello", proxy_addr.ip(), proxy_addr.port());
    let req = Request::builder()
        .method("GET")
        .uri(url)
        .header("X-Cmux-Port-Internal", upstream_addr.port().to_string())
        .body(Body::empty())
        .unwrap();

    let resp = timeout(Duration::from_secs(5), client.request(req)).await.expect("resp timeout").unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body()).await.unwrap();
    let s = String::from_utf8(body.to_vec()).unwrap();
    assert!(s.contains("ok:GET:/hello"), "unexpected body: {}", s);

    // Missing header -> 400
    let url2 = format!("http://{}:{}/missing", proxy_addr.ip(), proxy_addr.port());
    let req2 = Request::builder().method("GET").uri(url2).body(Body::empty()).unwrap();
    let resp2 = timeout(Duration::from_secs(5), client.request(req2)).await.expect("resp2 timeout").unwrap();
    assert_eq!(resp2.status(), StatusCode::BAD_REQUEST);

    // shutdown
    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_websocket_proxy_upgrade() {
    let ws_addr = start_upstream_ws_like_upgrade_echo().await;
    let (proxy_addr, shutdown, handle) = start_proxy(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), "127.0.0.1").await;

    // Raw HTTP upgrade handshake to proxy
    let mut stream = TcpStream::connect(proxy_addr).await.unwrap();
    let req = format!(
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: upgrade\r\nUpgrade: websocket\r\nX-Cmux-Port-Internal: {}\r\n\r\n",
        proxy_addr.port(), ws_addr.port()
    );
    stream.write_all(req.as_bytes()).await.unwrap();

    // Read 101 response
    let mut resp_buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = timeout(Duration::from_secs(5), stream.read(&mut tmp)).await.expect("read timeout").unwrap();
        assert!(n > 0);
        resp_buf.extend_from_slice(&tmp[..n]);
        if resp_buf.windows(4).any(|w| w == b"\r\n\r\n") { break; }
    }
    let resp_text = String::from_utf8_lossy(&resp_buf);
    assert!(resp_text.starts_with("HTTP/1.1 101"), "resp: {}", resp_text);

    // Echo over upgraded connection
    let payload = b"hello-upgrade\n";
    stream.write_all(payload).await.unwrap();
    let mut recv = vec![0u8; payload.len()];
    timeout(Duration::from_secs(5), stream.read_exact(&mut recv)).await.expect("upgrade echo timeout").unwrap();
    assert_eq!(&recv, payload);

    let _ = shutdown.send(());
    let _ = handle.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_connect_tcp_tunnel() {
    let (echo_addr, _echo_handle) = start_upstream_tcp_echo().await;
    let (proxy_addr, shutdown, handle) = start_proxy(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), "127.0.0.1").await;

    // Connect to proxy and issue CONNECT request with header
    let mut stream = TcpStream::connect(proxy_addr).await.unwrap();
    let req = format!(
        "CONNECT foo HTTP/1.1\r\nHost: foo\r\nX-Cmux-Port-Internal: {}\r\n\r\n",
        echo_addr.port()
    );
    stream.write_all(req.as_bytes()).await.unwrap();

    // Read response headers
    let mut resp_buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = timeout(Duration::from_secs(5), stream.read(&mut tmp)).await.expect("read timeout").unwrap();
        assert!(n > 0);
        resp_buf.extend_from_slice(&tmp[..n]);
        if resp_buf.windows(4).any(|w| w == b"\r\n\r\n") { break; }
        if resp_buf.len() > 8192 { panic!("response too large"); }
    }
    let resp_text = String::from_utf8_lossy(&resp_buf);
    assert!(resp_text.starts_with("HTTP/1.1 200"), "resp: {}", resp_text);

    // Tunnel is established now. Send and receive echo
    let payload = b"ping-123\n";
    stream.write_all(payload).await.unwrap();

    let mut recv = vec![0u8; payload.len()];
    timeout(Duration::from_secs(5), stream.read_exact(&mut recv)).await.expect("echo timeout").unwrap();
    assert_eq!(&recv, payload);

    let _ = shutdown.send(());
    let _ = handle.await;
}
