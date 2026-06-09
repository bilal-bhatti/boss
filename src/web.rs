//! A tiny embedded status page: one read-only HTML page showing the devices the
//! bridge currently manages plus the Matter commissioning QR / pairing code.
//!
//! A hand-rolled HTTP/1.1 responder over the same `async-io` reactor the Matter
//! runtime already uses, so it's just another task in the main `select`
//! borrowing `&Bridge`. The page itself is an `askama` template
//! (`templates/index.html`) — compile-time checked and auto-escaping — so no
//! markup lives in this file and device-supplied strings are escaped for us. The
//! QR is rendered as inline SVG from rs-matter's own QR encoder.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};

use askama::Template;
use async_io::Async;
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};

use rs_matter::error::Error;
use rs_matter::pairing::qr::Qr;

use crate::bridge::{Bridge, DeviceView};

/// Pre-rendered commissioning info shown on the page (constant for the life of
/// the process, so it's computed once at startup).
pub struct Commissioning {
    /// The onboarding QR as inline SVG.
    pub qr_svg: String,
    /// The 11-digit manual pairing code, pretty-printed.
    pub manual_code: String,
    /// The raw `MT:...` QR payload (shown as copyable text).
    pub qr_text: String,
}

/// The status page, bound to `templates/index.html`.
#[derive(Template)]
#[template(path = "index.html")]
struct Page<'a> {
    devices: &'a [DeviceView],
    online: usize,
    qr_svg: &'a str,
    manual_code: &'a str,
    qr_text: &'a str,
}

/// Serve the status page on `0.0.0.0:port` until the process exits.
///
/// Auxiliary, not load-bearing: a bind failure is logged loudly but then parks
/// forever rather than taking the bridge down (unlike mDNS, the page is not
/// required for the Matter node to function).
pub async fn run(bridge: &Bridge, port: u16, comm: &Commissioning) -> Result<(), Error> {
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let listener = match Async::<TcpListener>::bind(addr) {
        Ok(l) => l,
        Err(e) => {
            log::error!("status page disabled: cannot bind {addr}: {e}");
            return core::future::pending().await;
        }
    };
    log::info!("status page on http://0.0.0.0:{port}/");

    loop {
        match listener.accept().await {
            Ok((mut stream, peer)) => {
                if let Err(e) = serve(&mut stream, bridge, comm).await {
                    log::debug!("status page: client {peer} error: {e}");
                }
            }
            Err(e) => log::warn!("status page: accept failed: {e}"),
        }
    }
}

async fn serve(
    stream: &mut Async<TcpStream>,
    bridge: &Bridge,
    comm: &Commissioning,
) -> std::io::Result<()> {
    // We serve the same page for any request; read once to consume the request
    // headers (best effort) so the client doesn't see a reset before our reply.
    let mut scratch = [0u8; 1024];
    let _ = stream.read(&mut scratch).await?;

    let body = match render(bridge, comm) {
        Ok(html) => html,
        Err(e) => {
            log::error!("status page: template render failed: {e}");
            "<!doctype html><title>boss</title><p>internal error".to_string()
        }
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

fn render(bridge: &Bridge, comm: &Commissioning) -> Result<String, askama::Error> {
    let devices = bridge.device_views();
    let online = devices.iter().filter(|d| d.reachable).count();
    Page {
        devices: &devices,
        online,
        qr_svg: &comm.qr_svg,
        manual_code: &comm.manual_code,
        qr_text: &comm.qr_text,
    }
    .render()
}

/// Render the Matter onboarding QR as inline SVG (1 unit per module, scaled by
/// CSS). A 4-module quiet zone is included so scanners read it reliably.
pub fn qr_svg(qr: &Qr) -> String {
    const BORDER: i32 = 4;
    let size = qr.size() as i32;
    let dim = size + BORDER * 2;

    // Writing to a String is infallible, so the `write!` results are ignored.
    let mut modules = String::new();
    for y in 0..size {
        for x in 0..size {
            if qr.get_module(x, y) {
                let _ = write!(
                    modules,
                    "<rect x=\"{}\" y=\"{}\" width=\"1\" height=\"1\"/>",
                    x + BORDER,
                    y + BORDER
                );
            }
        }
    }

    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {dim} {dim}\" \
class=\"qr\" shape-rendering=\"crispEdges\" role=\"img\" aria-label=\"Matter pairing QR code\">\
<rect width=\"{dim}\" height=\"{dim}\" fill=\"#fff\"/><g fill=\"#0b1220\">{modules}</g></svg>"
    )
}
