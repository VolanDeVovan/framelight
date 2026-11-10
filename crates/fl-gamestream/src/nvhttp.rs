//! nvhttp routes on 47989 (plain) and 47984 (mutual TLS).

use crate::http::{Handler, Request, Response};
use crate::pairing::pair_xml;
use crate::{
    APP_ID, APP_VERSION, GFE_VERSION, HTTPS_PORT, HTTP_PORT, Inner, Launch, RTSP_PORT, SCM_HEVC,
};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Placeholder box art (a small solid PNG), used when the config has none.
const DEFAULT_APP_PNG: &[u8] = include_bytes!("../assets/app.png");

pub fn handler(inner: Arc<Inner>) -> Handler {
    Arc::new(move |req: &Request| route(&inner, req))
}

fn route(inner: &Arc<Inner>, req: &Request) -> Response {
    match req.path.as_str() {
        "/serverinfo" => serverinfo(inner, req),
        "/pair" => pair(inner, req),
        "/unpair" => {
            if let Some(u) = req.query.get("uniqueid") {
                inner.pairing.abandon(u, req.peer.ip());
            }
            Response::xml(ok_xml(""))
        }
        "/applist" | "/appasset" | "/launch" | "/resume" | "/cancel" => {
            let Some(cert) = paired_cert(inner, req) else {
                tracing::warn!(path = %req.path, peer = %req.peer, "rejected: client not paired");
                return if req.path == "/appasset" {
                    Response::status(403)
                } else {
                    Response::xml(error_xml(401, "The client is not paired with this host"))
                };
            };
            match req.path.as_str() {
                "/applist" => Response::xml(applist(inner)),
                "/appasset" => Response {
                    status: 200,
                    content_type: "image/png",
                    body: inner.cfg.app_image_png.clone().unwrap_or_else(|| DEFAULT_APP_PNG.to_vec()),
                },
                "/launch" => launch(inner, req, cert),
                "/resume" => resume(inner, req, &cert),
                _ => cancel(inner, &cert),
            }
        }
        _ => Response::status(404),
    }
}

/// The client cert of an HTTPS request, if it is pinned.
fn paired_cert(inner: &Inner, req: &Request) -> Option<Vec<u8>> {
    let cert = req.client_cert.as_ref()?;
    (req.https && inner.paired.is_paired(cert)).then(|| cert.clone())
}

fn serverinfo(inner: &Inner, req: &Request) -> Response {
    let cert = paired_cert(inner, req);
    let current_game = match (&*inner.launch.lock().unwrap(), &cert) {
        (Some(l), Some(c)) if &l.owner == c => l.appid,
        _ => 0,
    };
    let codecs = inner.cfg.codec_mode_support();
    let max_luma_hevc: u64 = if codecs & SCM_HEVC != 0 { 1_869_449_984 } else { 0 };
    let local_ip = local_ip(req);
    let xml = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<root status_code="200">
<hostname>{hostname}</hostname>
<appversion>{APP_VERSION}</appversion>
<GfeVersion>{GFE_VERSION}</GfeVersion>
<uniqueid>{uniqueid}</uniqueid>
<HttpsPort>{HTTPS_PORT}</HttpsPort>
<ExternalPort>{HTTP_PORT}</ExternalPort>
<MaxLumaPixelsHEVC>{max_luma_hevc}</MaxLumaPixelsHEVC>
<mac>00:00:00:00:00:00</mac>
<LocalIP>{local_ip}</LocalIP>
<ServerCodecModeSupport>{codecs}</ServerCodecModeSupport>
<SupportedDisplayMode>{modes}</SupportedDisplayMode>
<PairStatus>{paired}</PairStatus>
<currentgame>{current_game}</currentgame>
<state>{state}</state>
</root>
"#,
        hostname = xml_escape(&inner.cfg.name),
        modes = display_modes(),
        uniqueid = inner.id.uniqueid,
        paired = u8::from(cert.is_some()),
        state = if current_game != 0 { "SUNSHINE_SERVER_BUSY" } else { "SUNSHINE_SERVER_FREE" },
    );
    Response::xml(xml)
}

/// The list old GameStream hosts published. Any size works here (the picture is scaled
/// on the GPU), but moonlight-tv's launch code refuses a mode that is not listed when
/// its "unsupported settings" switch is off.
fn display_modes() -> String {
    let mut out = String::new();
    for (w, h) in [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)] {
        for fps in [30, 60, 90, 120] {
            out.push_str(&format!(
                "<DisplayMode><Width>{w}</Width><Height>{h}</Height><RefreshRate>{fps}</RefreshRate></DisplayMode>"
            ));
        }
    }
    out
}

fn local_ip(req: &Request) -> IpAddr {
    req.local.ip().to_canonical()
}

fn pair(inner: &Arc<Inner>, req: &Request) -> Response {
    let q = &req.query;
    let uniqueid = q.get("uniqueid").cloned().unwrap_or_default();
    let ip = req.peer.ip();
    let id = &inner.id;
    let result = if q.get("phrase").map(String::as_str) == Some("getservercert") {
        match (q.get("salt"), q.get("clientcert")) {
            (Some(salt), Some(cert)) => inner.pairing.getservercert(id, &uniqueid, salt, cert, ip),
            _ => Ok(pair_xml(false, "")),
        }
    } else if q.get("phrase").map(String::as_str) == Some("pairchallenge") {
        // Final step over TLS with the newly pinned cert.
        Ok(pair_xml(paired_cert(inner, req).is_some(), ""))
    } else if let Some(v) = q.get("clientchallenge") {
        inner.pairing.clientchallenge(id, &uniqueid, v, ip)
    } else if let Some(v) = q.get("serverchallengeresp") {
        inner.pairing.serverchallengeresp(id, &uniqueid, v, ip)
    } else if let Some(v) = q.get("clientpairingsecret") {
        inner.pairing.clientpairingsecret(&uniqueid, v, ip, &inner.paired)
    } else {
        Ok(pair_xml(false, ""))
    };
    Response::xml(result.unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), uniqueid, "pairing step failed");
        pair_xml(false, "")
    }))
}

fn applist(inner: &Inner) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">\n<App>\n<IsHdrSupported>0</IsHdrSupported>\n<AppTitle>{}</AppTitle>\n<ID>{APP_ID}</ID>\n</App>\n</root>\n",
        xml_escape(&inner.cfg.app_name)
    )
}

fn parse_rikey(q: &std::collections::HashMap<String, String>) -> Option<([u8; 16], u32)> {
    let key = hex::decode(q.get("rikey")?).ok()?;
    let key: [u8; 16] = key.get(..16)?.try_into().ok()?;
    // moonlight-common-c prints it signed, other clients unsigned: same low 32 bits.
    let id = q.get("rikeyid").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0) as u32;
    Some((key, id))
}

fn new_ping() -> [u8; 16] {
    let hexed = hex::encode_upper(crate::crypto::random_bytes::<8>());
    hexed.as_bytes().try_into().expect("16 hex chars")
}

fn launch(inner: &Arc<Inner>, req: &Request, owner: Vec<u8>) -> Response {
    let q = &req.query;
    let appid = q.get("appid").and_then(|s| s.parse().ok()).unwrap_or(APP_ID);
    if appid != APP_ID {
        return Response::xml(error_xml(404, "No such app on this host"));
    }
    let Some((key, rikeyid)) = parse_rikey(q) else {
        return Response::xml(error_xml(400, "Missing or malformed rikey"));
    };
    let (width, height, fps) = q
        .get("mode")
        .and_then(|m| {
            let mut it = m.split('x');
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .unwrap_or((1920, 1080, 60));
    // One session at a time: a new launch replaces whatever runs, telling its client.
    inner.stop_stream("replaced by a new launch", true);
    let l = Launch {
        generation: inner.generation.fetch_add(1, Ordering::SeqCst) + 1,
        appid,
        key,
        rikeyid,
        width,
        height,
        fps,
        owner,
        peer_ip: req.peer.ip().to_canonical(),
        ping: new_ping(),
        connect_data: u32::from_be_bytes(crate::crypto::random_bytes()),
        negotiated: None,
    };
    tracing::info!(client = %l.peer_ip, width, height, fps, "launch");
    *inner.launch.lock().unwrap() = Some(l);
    Response::xml(session_xml(req, "gamesession"))
}

fn resume(inner: &Arc<Inner>, req: &Request, cert: &[u8]) -> Response {
    let owner_ok = inner.launch.lock().unwrap().as_ref().map(|l| l.owner == cert);
    match owner_ok {
        None => return Response::xml(error_xml(400, "Nothing to resume; start the app again")),
        Some(false) => return Response::xml(error_xml(403, "The session belongs to another device")),
        Some(true) => {}
    }
    inner.stop_stream("client resumes", false);
    let mut guard = inner.launch.lock().unwrap();
    let Some(l) = guard.as_mut() else {
        return Response::xml(error_xml(400, "Nothing to resume; start the app again"));
    };
    if req.query.contains_key("rikey") {
        match parse_rikey(&req.query) {
            Some((key, id)) => {
                l.key = key;
                l.rikeyid = id;
            }
            None => return Response::xml(error_xml(400, "Malformed rikey")),
        }
    }
    l.peer_ip = req.peer.ip().to_canonical();
    l.ping = new_ping();
    l.connect_data = u32::from_be_bytes(crate::crypto::random_bytes());
    l.negotiated = None;
    tracing::info!(client = %l.peer_ip, "resume");
    drop(guard);
    Response::xml(session_xml(req, "resume"))
}

fn cancel(inner: &Arc<Inner>, cert: &[u8]) -> Response {
    let owned = inner.launch.lock().unwrap().as_ref().map(|l| l.owner == cert);
    if owned == Some(false) {
        return Response::xml(error_xml(403, "The session belongs to another device"));
    }
    inner.stop_stream("client quit the app", true);
    *inner.launch.lock().unwrap() = None;
    tracing::info!("cancel — session closed");
    Response::xml(ok_xml("<cancel>1</cancel>"))
}

fn session_xml(req: &Request, tag: &str) -> String {
    let ip = local_ip(req);
    let host = match ip {
        IpAddr::V6(v6) => format!("[{v6}]"),
        v4 => v4.to_string(),
    };
    ok_xml(&format!("<sessionUrl0>rtsp://{host}:{RTSP_PORT}</sessionUrl0><{tag}>1</{tag}>"))
}

fn ok_xml(inner: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">{inner}</root>\n")
}

fn error_xml(code: u16, msg: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"{code}\" status_message=\"{}\"></root>\n",
        xml_escape(msg)
    )
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
