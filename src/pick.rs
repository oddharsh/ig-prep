//! A crop chosen by hand, on this machine, so the app has nothing to resample.
//!
//! The Instagram app's crop measured as a resample: a rectangle 4098.2 source
//! rows tall squeezed into 4096, so the phase drifts across the frame and
//! blurs everything but a band in the middle (post DdhQL_qMTwr, 2026-09-20,
//! Laplacian energy ratio 0.71 against the upload, 0.86 in that band). A
//! window chosen here is integer, inside the band and already at the tier
//! width, so the upload reaches the servers as it left this tool.
//!
//! The picker is one page served from a loopback socket by this process, one
//! photograph at a time. std's TcpListener, a request line and a
//! Content-Length are the whole protocol: nothing is served by name, so there
//! is no path to traverse, and the socket never leaves 127.0.0.1. The page
//! posts the window in source pixels; the export goes through the same plan,
//! render and encode as every other conversion.

use crate::{Opts, encode, geometry, image};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

/// Longest edge of the preview the browser gets. Enough to place a window;
/// the export uses the frame this process already decoded.
const PREVIEW_EDGE: u32 = 1600;
/// Exports that may run at once. Each holds a decoded frame, so this bounds
/// memory as well as cores, like the converter's two workers.
const EXPORTS_IN_FLIGHT: usize = 2;
const HEADER_LIMIT: usize = 64 * 1024;
const BODY_LIMIT: usize = 64 * 1024;

pub fn run(files: &[PathBuf], opts: &Opts) -> i32 {
    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("ig-prep: cannot listen on loopback: {e}");
            return 1;
        }
    };
    let url = match listener.local_addr() {
        Ok(a) => format!("http://{a}/"),
        Err(e) => {
            eprintln!("ig-prep: {e}");
            return 1;
        }
    };
    println!("Choose the framing at {url}");
    open_browser(&url);
    match session(&listener, files, opts) {
        Ok(Outcome { failed, left }) => {
            if left > 0 {
                println!("{left} left unconverted");
            }
            i32::from(failed)
        }
        Err(e) => {
            eprintln!("ig-prep: {e}");
            1
        }
    }
}

struct Outcome {
    failed: bool,
    left: usize,
}

/// Serve the picker until every file is exported, skipped or the page quits.
///
/// Choosing a window takes longer than decoding or encoding a frame, so
/// neither waits on the other: the next photograph decodes on a thread
/// while the current one is on screen, and each export runs on a thread of
/// its own once the window is posted. Only the last export makes the page
/// wait, because nothing else is left to choose.
fn session(listener: &TcpListener, files: &[PathBuf], opts: &Opts) -> Result<Outcome, String> {
    let mut failed = false;
    let mut i = 0;
    let mut current: Option<Current> = None;
    let mut preload: Option<(usize, JoinHandle<Result<Current, String>>)> = None;
    let mut exports: VecDeque<Export> = VecDeque::new();
    while i < files.len() {
        if current.is_none() {
            let loaded = match preload.take() {
                Some((j, handle)) if j == i => handle
                    .join()
                    .map_err(|_| "decode thread panicked".to_string())
                    .and_then(|r| r),
                _ => Current::load(&files[i]),
            };
            match loaded {
                Ok(c) => current = Some(c),
                Err(e) => {
                    eprintln!("{}: {e}", files[i].display());
                    failed = true;
                    i += 1;
                    continue;
                }
            }
        }
        if preload.is_none() && i + 1 < files.len() {
            let path = files[i + 1].clone();
            preload = Some((i + 1, std::thread::spawn(move || Current::load(&path))));
        }
        let cur = current.as_ref().ok_or("no photograph loaded")?;
        let (mut stream, _) = listener.accept().map_err(|e| format!("accept: {e}"))?;
        let req = match Request::read(&mut stream) {
            Ok(r) => r,
            Err(e) => {
                let _ = respond(&mut stream, 400, "text/plain", e.as_bytes());
                continue;
            }
        };
        match (req.method.as_str(), req.path.as_str()) {
            ("GET", "/") => {
                let body = page(cur, &files[i], i, files.len(), opts.width);
                respond(
                    &mut stream,
                    200,
                    "text/html; charset=utf-8",
                    body.as_bytes(),
                )?;
            }
            ("GET", "/preview.jpg") => respond(&mut stream, 200, "image/jpeg", &cur.preview)?,
            ("POST", "/crop") => {
                let window = match parse_window(&req.body) {
                    Ok(w) => w,
                    Err(e) => {
                        let body = json!({"ok": false, "advanced": false, "next": true, "line": e});
                        respond(
                            &mut stream,
                            400,
                            "application/json",
                            body.to_string().as_bytes(),
                        )?;
                        continue;
                    }
                };
                let frame = current.take().ok_or("no photograph loaded")?;
                let path = files[i].clone();
                let opts = opts.clone();
                while exports.len() >= EXPORTS_IN_FLIGHT {
                    finish_one(&mut exports, &mut failed);
                }
                exports.push_back(Export {
                    path: path.clone(),
                    handle: std::thread::spawn(move || export(&frame, &path, window, &opts)),
                });
                i += 1;
                let last = i >= files.len();
                let (ok, line) = if last {
                    let done = finish_all(&mut exports, &mut failed);
                    (done.1 == 0, summary(done))
                } else {
                    (
                        true,
                        format!(
                            "{}: exporting in the background",
                            files[i - 1]
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                        ),
                    )
                };
                let body = json!({"ok": ok, "advanced": true, "next": !last, "line": line});
                respond(
                    &mut stream,
                    200,
                    "application/json",
                    body.to_string().as_bytes(),
                )?;
            }
            ("POST", "/skip") => {
                let line = format!("{}: skipped", files[i].display());
                println!("{line}");
                i += 1;
                current = None;
                let last = i >= files.len();
                let line = if last {
                    let done = finish_all(&mut exports, &mut failed);
                    format!("{line}; {}", summary(done))
                } else {
                    line
                };
                let body = json!({"ok": true, "advanced": true, "next": !last, "line": line});
                respond(
                    &mut stream,
                    200,
                    "application/json",
                    body.to_string().as_bytes(),
                )?;
            }
            ("POST", "/quit") => {
                let done = finish_all(&mut exports, &mut failed);
                let body = json!({"ok": true, "advanced": false, "next": false, "line": format!("stopped; {}", summary(done))});
                respond(
                    &mut stream,
                    200,
                    "application/json",
                    body.to_string().as_bytes(),
                )?;
                return Ok(Outcome {
                    failed,
                    left: files.len() - i,
                });
            }
            _ => respond(&mut stream, 404, "text/plain", b"not found")?,
        }
    }
    finish_all(&mut exports, &mut failed);
    Ok(Outcome { failed, left: 0 })
}

/// An export running on its own thread, holding the frame it needs.
struct Export {
    path: PathBuf,
    handle: JoinHandle<Result<String, String>>,
}

/// Wait for the oldest export and report it, in file order. Returns whether
/// it succeeded.
fn finish_one(exports: &mut VecDeque<Export>, failed: &mut bool) -> Option<bool> {
    let Export { path, handle } = exports.pop_front()?;
    match handle.join() {
        Ok(Ok(line)) => {
            println!("{line}");
            Some(true)
        }
        Ok(Err(e)) => {
            eprintln!("{}: {e}", path.display());
            *failed = true;
            Some(false)
        }
        Err(_) => {
            eprintln!("{}: export thread panicked", path.display());
            *failed = true;
            Some(false)
        }
    }
}

/// Wait for every export; the count of (exported, failed) in this batch.
fn finish_all(exports: &mut VecDeque<Export>, failed: &mut bool) -> (usize, usize) {
    let mut done = (0, 0);
    while let Some(ok) = finish_one(exports, failed) {
        if ok {
            done.0 += 1;
        } else {
            done.1 += 1;
        }
    }
    done
}

fn summary((exported, failed): (usize, usize)) -> String {
    match failed {
        0 => format!("{exported} exported"),
        n => format!("{exported} exported, {n} failed, see the terminal"),
    }
}

/// The photograph on screen: the decoded frame, kept for the export, and a
/// preview small enough to ship to a browser.
struct Current {
    img: image::Rgb,
    preview: Vec<u8>,
    pw: u32,
    ph: u32,
}

impl Current {
    fn load(path: &Path) -> Result<Self, String> {
        let img = crate::load_oriented(path)?;
        let (pw, ph) = preview_size(img.w, img.h);
        let small = image::resize(&img, pw, ph)?;
        let preview = encode::jpeg(&small, 90, encode::Chroma::Quartered, false)?;
        Ok(Self {
            img,
            preview,
            pw,
            ph,
        })
    }
}

fn preview_size(w: u32, h: u32) -> (u32, u32) {
    let f = (PREVIEW_EDGE as f64 / w.max(h).max(1) as f64).min(1.0);
    (
        (w as f64 * f).round().max(1.0) as u32,
        (h as f64 * f).round().max(1.0) as u32,
    )
}

fn parse_window(body: &[u8]) -> Result<(u32, u32, u32, u32), String> {
    let v: Value = serde_json::from_slice(body).map_err(|e| format!("crop request: {e}"))?;
    let n = |k: &str| {
        v.get(k)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| format!("crop request: {k} must be a whole number of pixels"))
    };
    Ok((n("x")?, n("y")?, n("w")?, n("h")?))
}

fn export(
    cur: &Current,
    path: &Path,
    window: (u32, u32, u32, u32),
    opts: &Opts,
) -> Result<String, String> {
    let plan = geometry::plan_window(cur.img.w, cur.img.h, window, opts.width)?;
    let final_img = crate::render(&cur.img, &plan, opts.pad)?;
    let bytes = encode::jpeg(&final_img, opts.quality, opts.chroma, opts.dither)?;
    let dest = opts.out_dir.join(format!(
        "{}.jpg",
        path.file_stem().unwrap_or_default().to_string_lossy()
    ));
    std::fs::write(&dest, &bytes).map_err(|e| format!("{}: {e}", dest.display()))?;
    Ok(format!(
        "{}: {}x{} -> {}x{} (window {}x{} at {},{})  {} {} KB",
        path.file_name().unwrap_or_default().to_string_lossy(),
        cur.img.w,
        cur.img.h,
        plan.scale.0,
        plan.scale.1,
        window.2,
        window.3,
        window.0,
        window.1,
        opts.chroma.label(),
        bytes.len() / 1024
    ))
}

// ── the wire ──────────────────────────────────────────────────────────

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

impl Request {
    /// Read one request: a request line, headers up to the blank line, and
    /// as many body bytes as Content-Length promises. Anything else is a
    /// bad request; the browser is the only client and it does not need more.
    fn read(stream: &mut TcpStream) -> Result<Self, String> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let head_end = loop {
            if let Some(p) = find(&buf, b"\r\n\r\n") {
                break p;
            }
            if buf.len() > HEADER_LIMIT {
                return Err("request headers too long".into());
            }
            let n = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                return Err("connection closed before the request ended".into());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let mut first = lines.next().unwrap_or("").split(' ');
        let method = first.next().unwrap_or("").to_string();
        let path = first
            .next()
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("")
            .to_string();
        if method.is_empty() || !path.starts_with('/') {
            return Err("malformed request line".into());
        }
        let length = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if length > BODY_LIMIT {
            return Err("request body too long".into());
        }
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < length {
            let n = stream.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                return Err("connection closed before the body ended".into());
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(length);
        Ok(Self { method, path, body })
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn respond(stream: &mut TcpStream, status: u16, ctype: &str, body: &[u8]) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush())
        .map_err(|e| format!("respond: {e}"))
}

fn open_browser(url: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    // The URL is printed either way; a missing opener is not an error.
    let _ = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// ── the page ──────────────────────────────────────────────────────────

fn page(cur: &Current, path: &Path, index: usize, count: usize, target: u32) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let ratio = cur.img.w as f64 / cur.img.h as f64;
    let in_band = (geometry::MIN_PORTRAIT..=geometry::MAX_LANDSCAPE).contains(&ratio);
    PAGE.replace("%NAME%", &escape(&name))
        .replace("%INDEX%", &(index + 1).to_string())
        .replace("%COUNT%", &count.to_string())
        .replace("%SW%", &cur.img.w.to_string())
        .replace("%SH%", &cur.img.h.to_string())
        .replace("%PW%", &cur.pw.to_string())
        .replace("%PH%", &cur.ph.to_string())
        .replace("%TARGET%", &target.to_string())
        .replace("%MINP%", &geometry::MIN_PORTRAIT.to_string())
        .replace("%MAXL%", &geometry::MAX_LANDSCAPE.to_string())
        .replace("%INBAND%", if in_band { "true" } else { "false" })
        .replace("%SEQ%", &index.to_string())
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

const PAGE: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>ig-prep: %NAME%</title>
<style>
  :root { color-scheme: dark; }
  body { margin: 0; background: #151515; color: #ddd; font: 14px/1.4 system-ui, sans-serif; display: grid; grid-template-rows: auto 1fr auto; height: 100vh; }
  header, footer { padding: 10px 16px; display: flex; gap: 16px; align-items: center; flex-wrap: wrap; }
  header b { color: #fff; }
  main { display: grid; place-items: center; overflow: hidden; padding: 8px; }
  #stage { position: relative; user-select: none; }
  #p { display: block; max-width: calc(100vw - 32px); max-height: calc(100vh - 130px); width: auto; height: auto; }
  #w { position: absolute; box-sizing: border-box; border: 2px solid #fff; box-shadow: 0 0 0 9999px rgba(0,0,0,.6); cursor: grab; }
  #w:active { cursor: grabbing; }
  #w::before, #w::after { content: ""; position: absolute; inset: 0; pointer-events: none; opacity: .35; }
  #w::before { border-left: 1px solid #fff; border-right: 1px solid #fff; margin: 0 33.33%; }
  #w::after { border-top: 1px solid #fff; border-bottom: 1px solid #fff; margin: 33.33% 0; }
  label { display: inline-flex; gap: 4px; align-items: center; }
  button { font: inherit; padding: 6px 14px; border-radius: 6px; border: 1px solid #555; background: #2a2a2a; color: #eee; cursor: pointer; }
  button.primary { background: #3b82f6; border-color: #3b82f6; color: #fff; }
  button:disabled { opacity: .5; cursor: default; }
  #s { margin-left: auto; font-variant-numeric: tabular-nums; color: #bbb; }
  input[type=range] { width: 160px; }
</style></head>
<body>
<header>
  <span><b>%NAME%</b> &middot; %INDEX% of %COUNT% &middot; %SW%&times;%SH%</span>
  <span id="ratios"></span>
  <label>zoom <input id="z" type="range" min="1" max="1" step="0.001" value="1"></label>
  <span id="s"></span>
</header>
<main><div id="stage"><img id="p" src="/preview.jpg?%SEQ%" width="%PW%" height="%PH%" alt=""><div id="w" tabindex="0"></div></div></main>
<footer>
  <button id="export" class="primary">Export</button>
  <button id="skip">Skip</button>
  <button id="quit">Quit</button>
  <span>drag the window, arrow keys nudge (shift for 10), enter exports</span>
</footer>
<script>
const SW=%SW%, SH=%SH%, TARGET=%TARGET%, MINP=%MINP%, MAXL=%MAXL%, INBAND=%INBAND%;
const img=document.getElementById('p'), win=document.getElementById('w'), st=document.getElementById('s'), z=document.getElementById('z');
const portrait = SW < SH;
const choices = portrait ? [['3:4',0.75],['4:5',0.8],['1:1',1]] : [['1.91:1',1.91],['3:2',1.5],['4:5',0.8],['1:1',1]];
if (INBAND) choices.unshift(['full', SW/SH]);
let ratio = choices[0][1], full = INBAND;
let zoom = 1, cx = 0.5, cy = 0.5;
const rs = document.getElementById('ratios');
choices.forEach(([label, r], k) => {
  const l = document.createElement('label');
  const i = document.createElement('input'); i.type = 'radio'; i.name = 'r'; i.checked = k === 0;
  i.onchange = () => { ratio = r; full = label === 'full'; zoom = 1; z.value = 1; layout(); };
  l.append(i, label); rs.append(l);
});
function limits() {
  const wmax = Math.min(SW, Math.floor(SH * ratio));
  const wmin = Math.min(TARGET, wmax);
  return { wmax, wmin };
}
function geom() {
  if (full) return { x: 0, y: 0, w: SW, h: SH };
  const { wmax, wmin } = limits();
  let w = Math.max(wmin, Math.min(wmax, Math.round(wmax / zoom)));
  let h = ratio < 1 ? Math.floor(w / ratio) : Math.ceil(w / ratio);
  if (h > SH) { h = SH; w = ratio < 1 ? Math.ceil(h * ratio) : Math.floor(h * ratio); }
  let x = Math.round(cx * SW - w / 2), y = Math.round(cy * SH - h / 2);
  x = Math.max(0, Math.min(SW - w, x)); y = Math.max(0, Math.min(SH - h, y));
  // Write the clamped centre back, or a drag past the edge leaves the
  // centre stranded outside and the next nudges move nothing.
  cx = (x + w / 2) / SW; cy = (y + h / 2) / SH;
  return { x, y, w, h };
}
function layout() {
  const { wmax, wmin } = limits();
  z.max = Math.max(1, wmax / wmin).toFixed(3); z.disabled = full || z.max === '1.000';
  const g = geom(), s = img.clientWidth / SW;
  win.style.left = g.x * s + 'px'; win.style.top = g.y * s + 'px';
  win.style.width = g.w * s + 'px'; win.style.height = g.h * s + 'px';
  const ow = Math.min(TARGET, g.w), oh = Math.round(ow / (g.w / g.h));
  st.textContent = `window ${g.w}×${g.h} at ${g.x},${g.y} → export ${ow}×${oh}`;
}
let drag = null;
win.addEventListener('mousedown', e => { drag = { x: e.clientX, y: e.clientY, cx, cy }; win.focus(); e.preventDefault(); });
window.addEventListener('mousemove', e => {
  if (!drag) return;
  const s = img.clientWidth / SW;
  cx = drag.cx + (e.clientX - drag.x) / s / SW;
  cy = drag.cy + (e.clientY - drag.y) / s / SH;
  layout();
});
window.addEventListener('mouseup', () => { drag = null; });
window.addEventListener('keydown', e => {
  const step = (e.shiftKey ? 10 : 1) / (img.clientWidth / SW);
  if (e.key === 'ArrowLeft') cx -= step / SW; else if (e.key === 'ArrowRight') cx += step / SW;
  else if (e.key === 'ArrowUp') cy -= step / SH; else if (e.key === 'ArrowDown') cy += step / SH;
  else if (e.key === 'Enter') { send('/crop'); return; } else return;
  e.preventDefault(); layout();
});
z.addEventListener('input', () => { zoom = parseFloat(z.value); layout(); });
window.addEventListener('resize', layout);
img.addEventListener('load', layout);
function send(path) {
  for (const b of document.querySelectorAll('button')) b.disabled = true;
  st.textContent = path === '/crop' ? 'exporting…' : '…';
  fetch(path, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(geom()) })
    .then(r => r.json()).then(j => {
      st.textContent = j.line;
      if (j.advanced && j.next) setTimeout(() => location.reload(), 600);
      else if (j.advanced || path === '/quit') {
        // The tab was opened for this page, so the page may close it. A
        // browser that refuses leaves the line below as the fallback.
        st.textContent = j.line + ' \u00b7 done';
        setTimeout(() => { window.close(); st.textContent = j.line + ' \u00b7 done, you can close this tab'; }, 800);
      }
      else for (const b of document.querySelectorAll('button')) b.disabled = false;
    }).catch(e => { st.textContent = 'ig-prep has stopped: ' + e; });
}
document.getElementById('export').onclick = () => send('/crop');
document.getElementById('skip').onclick = () => send('/skip');
document.getElementById('quit').onclick = () => send('/quit');
layout();
</script>
</body></html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn request(bytes: &[u8]) -> Result<Request, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let bytes = bytes.to_vec();
        let writer = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(&bytes).unwrap();
            c
        });
        let (mut s, _) = listener.accept().unwrap();
        let r = Request::read(&mut s);
        drop(writer.join().unwrap());
        r
    }

    #[test]
    fn requests_are_read_line_query_and_body() {
        let r = request(b"POST /crop?x=1 HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello")
            .unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/crop"));
        assert_eq!(r.body, b"hello");
        let r = request(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str()), ("GET", "/"));
        assert!(r.body.is_empty());
        assert!(request(b"nonsense\r\n\r\n").is_err());
        assert!(request(b"POST / HTTP/1.1\r\nContent-Length: 999999\r\n\r\n").is_err());
    }

    #[test]
    fn windows_are_whole_pixels_or_nothing() {
        assert_eq!(
            parse_window(br#"{"x":1,"y":2,"w":30,"h":40}"#).unwrap(),
            (1, 2, 30, 40)
        );
        assert!(parse_window(br#"{"x":1.5,"y":2,"w":30,"h":40}"#).is_err());
        assert!(parse_window(br#"{"x":-1,"y":2,"w":30,"h":40}"#).is_err());
        assert!(parse_window(br#"{"x":1,"y":2,"w":30}"#).is_err());
        assert!(parse_window(b"not json").is_err());
    }

    #[test]
    fn the_page_carries_the_frame_and_escapes_the_name() {
        let cur = Current {
            img: image::Rgb::new(2, 3, vec![0.5; 18]),
            preview: Vec::new(),
            pw: 2,
            ph: 3,
        };
        let html = page(&cur, Path::new("a<b>&\"c.jpg"), 2, 5, 3072);
        assert!(html.contains("const SW=2, SH=3, TARGET=3072"));
        assert!(html.contains("INBAND=false"));
        assert!(html.contains("a&lt;b&gt;&amp;&quot;c.jpg"));
        assert!(html.contains("3 of 5"));
        assert!(
            html.contains("window.close()"),
            "the tab closes itself when done"
        );
        assert!(
            html.split('%')
                .skip(1)
                .all(|s| !s.starts_with(|c: char| c.is_ascii_uppercase())),
            "every placeholder is filled"
        );
        assert_eq!(preview_size(7728, 5152), (1600, 1067));
        assert_eq!(preview_size(800, 600), (800, 600));
    }

    /// A whole session over the wire: page, preview, one export, done.
    #[test]
    fn a_session_exports_the_posted_window_and_then_ends() {
        let dir = tempfile::tempdir().unwrap();
        // A PNG source: the converter's own progressive 4:4:4 output is not
        // readable by the zune-jpeg it decodes sources with (see simulate.rs).
        let src = dir.path().join("tall.png");
        let mut px = Vec::new();
        for y in 0..144u32 {
            for x in 0..96u32 {
                px.extend([(x * 255 / 95) as u8, (y * 255 / 143) as u8, 80]);
            }
        }
        let mut info = png::Info::with_size(96, 144);
        info.color_type = png::ColorType::Rgb;
        info.bit_depth = png::BitDepth::Eight;
        let file = std::fs::File::create(&src).unwrap();
        png::Encoder::with_info(std::io::BufWriter::new(file), info)
            .unwrap()
            .write_header()
            .unwrap()
            .write_image_data(&px)
            .unwrap();
        let mut opts = Opts::defaults();
        opts.out_dir = dir.path().join("out");
        std::fs::create_dir(&opts.out_dir).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let files = vec![src.clone()];
        let out_dir = opts.out_dir.clone();
        let server = std::thread::spawn(move || session(&listener, &files, &opts).unwrap());

        let talk = |req: String| -> (u16, Vec<u8>) {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(req.as_bytes()).unwrap();
            let mut resp = Vec::new();
            c.read_to_end(&mut resp).unwrap();
            let p = find(&resp, b"\r\n\r\n").unwrap();
            let status = String::from_utf8_lossy(&resp[9..12]).parse().unwrap();
            (status, resp[p + 4..].to_vec())
        };
        let (status, html) = talk("GET / HTTP/1.1\r\nHost: x\r\n\r\n".into());
        assert_eq!(status, 200);
        let html = String::from_utf8(html).unwrap();
        assert!(html.contains("tall.png") && html.contains("const SW=96, SH=144"));
        let (status, jpeg) = talk("GET /preview.jpg?0 HTTP/1.1\r\nHost: x\r\n\r\n".into());
        assert_eq!(status, 200);
        assert_eq!(&jpeg[..2], &[0xff, 0xd8]);
        let (status, _) = talk("GET /nothing HTTP/1.1\r\nHost: x\r\n\r\n".into());
        assert_eq!(status, 404);
        let bad = r#"{"x":0,"y":0,"w":96,"h":144}"#;
        let (status, body) = talk(format!(
            "POST /crop HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{bad}",
            bad.len()
        ));
        assert_eq!(
            status, 200,
            "a refused window is reported, not a protocol error"
        );
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["advanced"], true);
        assert_eq!(v["next"], false);
        assert_eq!(v["ok"], false, "{v}");
        let outcome = server.join().unwrap();
        assert!(outcome.failed);
        assert!(!out_dir.join("tall.jpg").exists());

        // Again, with a window inside the band: the export lands.
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let files = vec![src];
        let mut opts = Opts::defaults();
        opts.out_dir = out_dir.clone();
        let server = std::thread::spawn(move || session(&listener, &files, &opts).unwrap());
        let good = r#"{"x":0,"y":8,"w":96,"h":128}"#;
        let mut c = TcpStream::connect(addr).unwrap();
        c.write_all(
            format!(
                "POST /crop HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{good}",
                good.len()
            )
            .as_bytes(),
        )
        .unwrap();
        let mut resp = Vec::new();
        c.read_to_end(&mut resp).unwrap();
        let v: Value =
            serde_json::from_slice(&resp[find(&resp, b"\r\n\r\n").unwrap() + 4..]).unwrap();
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["next"], false);
        let outcome = server.join().unwrap();
        assert!(!outcome.failed && outcome.left == 0);
        let (w, h) = dims(&std::fs::read(out_dir.join("tall.jpg")).unwrap());
        assert_eq!(
            (w, h),
            (96, 128),
            "3:4 window at native size, never enlarged"
        );
    }

    /// Two photographs: the first export returns at once and runs behind the
    /// second choice; the last export makes the page wait for everything.
    #[test]
    fn exports_run_in_the_background_until_the_last_one() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for (name, tint) in [("one.png", 60u8), ("two.png", 180u8)] {
            let src = dir.path().join(name);
            let mut px = Vec::new();
            for y in 0..144u32 {
                for x in 0..96u32 {
                    px.extend([(x * 255 / 95) as u8, tint, (y * 255 / 143) as u8]);
                }
            }
            let mut info = png::Info::with_size(96, 144);
            info.color_type = png::ColorType::Rgb;
            info.bit_depth = png::BitDepth::Eight;
            png::Encoder::with_info(
                std::io::BufWriter::new(std::fs::File::create(&src).unwrap()),
                info,
            )
            .unwrap()
            .write_header()
            .unwrap()
            .write_image_data(&px)
            .unwrap();
            files.push(src);
        }
        let out_dir = dir.path().join("out");
        std::fs::create_dir(&out_dir).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut opts = Opts::defaults();
        opts.out_dir = out_dir.clone();
        let server = std::thread::spawn(move || session(&listener, &files, &opts).unwrap());
        let talk = |req: String| -> Value {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(req.as_bytes()).unwrap();
            let mut resp = Vec::new();
            c.read_to_end(&mut resp).unwrap();
            let body = &resp[find(&resp, b"\r\n\r\n").unwrap() + 4..];
            serde_json::from_slice(body)
                .unwrap_or_else(|_| json!({"html": String::from_utf8_lossy(body)}))
        };
        let window = r#"{"x":0,"y":8,"w":96,"h":128}"#;
        let post = format!(
            "POST /crop HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{window}",
            window.len()
        );
        let first = talk(post.clone());
        assert_eq!(first["next"], true);
        assert!(
            first["line"]
                .as_str()
                .unwrap()
                .contains("exporting in the background"),
            "{first}"
        );
        let page = talk("GET / HTTP/1.1\r\nHost: x\r\n\r\n".into());
        assert!(
            page["html"].as_str().unwrap().contains("2 of 2"),
            "the second photograph is on screen"
        );
        let last = talk(post);
        assert_eq!(last["next"], false);
        assert_eq!(last["line"], "2 exported", "{last}");
        let outcome = server.join().unwrap();
        assert!(!outcome.failed && outcome.left == 0);
        for name in ["one.jpg", "two.jpg"] {
            assert_eq!(dims(&std::fs::read(out_dir.join(name)).unwrap()), (96, 128));
        }
    }

    fn dims(bytes: &[u8]) -> (u16, u16) {
        let mut d = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
        d.read_info().unwrap();
        let info = d.info().unwrap();
        (info.width, info.height)
    }
}
