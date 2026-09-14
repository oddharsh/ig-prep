//! `ig-prep mcp` — the converter as a tool a model can call.
//!
//! The point of this mode is that the photographs never move. A model with
//! file access runs this server over stdio, names paths on the machine the
//! photographs are already on, and gets back the files it asked for plus the
//! numbers describing what happened. Nothing is uploaded, nothing is encoded
//! into the conversation, and the conversion is the same one `ig-prep <dir>`
//! performs, because both surfaces call the same `one()`.
//!
//! That last part is the design rather than an implementation detail. A tool
//! server that re-implements its own CLI grows a second set of defaults and
//! then disagrees with the documentation; here `Opts::defaults()` is the CLI's
//! defaults, and a caller who sets no arguments gets exactly `ig-prep <dir>`.
//!
//! WHAT IT DELIBERATELY DOES NOT DO: return image bytes. A converted 1440px
//! frame is a few hundred kilobytes, which is a megabyte of base64 in a tool
//! result, and a model that asked to convert forty photographs does not want
//! forty megabytes of them in its context. The files are on disk, where the
//! caller can already reach them, so the result carries paths and sizes.
//!
//! ## Transport
//!
//! Newline-delimited JSON-RPC on stdin and stdout, which is MCP's stdio
//! transport. Nothing else may write to stdout: a stray `println!` corrupts
//! the stream and the client reports a parse error rather than the print.
//! Diagnostics go to stderr. The one real hazard is the HEIF path, which
//! shells out to `sips`; `decode.rs` uses `.output()`, so the child's stdout
//! is captured rather than inherited, and that is load-bearing here in a way
//! it is not for the CLI.

use crate::encode::Chroma;
use crate::geometry::{Fit, Gravity};
use crate::{Agreement, Opts, Report};
use serde_json::{Map, Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const SERVER_NAME: &str = "ig-prep";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The revision this server declares, and the three legacy ones it still
/// answers. Both eras stay for the reason the spec gives: a legacy client has
/// no fall-forward mechanism, so pointed at a modern-only server it does not
/// negotiate down, it simply fails. Today essentially every stdio client in
/// the wild opens with `initialize`, which is the legacy handshake.
const MCP_PROTOCOL: &str = "2026-07-28";
const MCP_SUPPORTED: [&str; 4] = [MCP_PROTOCOL, "2025-06-18", "2025-03-26", "2024-11-05"];

/// Files converted in one call before the batch is cut short.
///
/// A cap is needed because a caller can name a directory and a directory can
/// hold a thousand 40-megapixel frames, which is tens of minutes of work under
/// a client that will have given up long before. What the cap must not do is
/// truncate quietly: a result listing 200 conversions of a 900-file directory
/// reads as a complete run, so the overflow is reported by count and the
/// caller is told to narrow the request.
const MAX_FILES: usize = 200;

// ── transport ─────────────────────────────────────────────────────────

pub fn serve(root: Option<PathBuf>) -> i32 {
    // Resolved once, at startup, so that a symlink swapped underneath us later
    // cannot widen the boundary mid-session.
    let root = match root {
        Some(r) => match r.canonicalize() {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("ig-prep: --root {}: {e}", r.display());
                return 1;
            }
        },
        None => None,
    };
    match &root {
        Some(r) => eprintln!("ig-prep mcp: confined to {}", r.display()),
        None => eprintln!(
            "ig-prep mcp: unconfined — any readable path may be converted. \
             Pass --root DIR to limit it."
        ),
    }

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ig-prep mcp: stdin: {e}");
                return 1;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(req) => handle(&req, root.as_deref()),
            // A parse failure has no id to answer under, which is what the
            // spec's null id is for.
            Err(e) => Some(error(
                Value::Null,
                -32700,
                &format!("parse error: {e}"),
                None,
            )),
        };
        if let Some(r) = response {
            // A write failure means the client is gone; there is nothing left
            // to report it to.
            if writeln!(stdout, "{r}").is_err() || stdout.flush().is_err() {
                return 0;
            }
        }
    }
    0
}

/// `None` for a notification, which by JSON-RPC carries no id and gets no
/// answer. Replying to one is a protocol error that some clients treat as a
/// stray message and others as a fault.
fn handle(req: &Value, root: Option<&Path>) -> Option<Value> {
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let id = req.get("id").cloned();
    let id = match id {
        Some(Value::Null) | None => return None,
        Some(i) => i,
    };
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    // The era is chosen by the shape of the request and by nothing else. A
    // `_meta` block is a modern client declaring itself; its absence is how a
    // legacy client presents itself, so absence cannot also be read as
    // malformed. `initialize` selects legacy outright.
    let meta = params.get("_meta");
    if let Some(m) = meta
        && let Some(v) = m.get("protocolVersion").and_then(Value::as_str)
        && !MCP_SUPPORTED.contains(&v)
    {
        return Some(error(
            id,
            -32022,
            &format!("unsupported protocol version {v}"),
            Some(json!({ "supported": MCP_SUPPORTED, "requested": v })),
        ));
    }

    match method {
        "initialize" => {
            let asked = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            // Echo the client's revision when we speak it. Otherwise answer
            // with the newest LEGACY one rather than with `MCP_PROTOCOL`,
            // since a client that opened with `initialize` is by definition
            // not speaking the revision that deleted `initialize`.
            let agreed = if MCP_SUPPORTED.contains(&asked) {
                asked
            } else {
                MCP_SUPPORTED[1]
            };
            Some(ok(
                id,
                json!({
                    "protocolVersion": agreed,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                    "instructions": INSTRUCTIONS,
                }),
            ))
        }
        // The modern revision replaces the handshake with this, and says
        // servers MUST implement it.
        "server/discover" => Some(ok(
            id,
            json!({
                "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                "capabilities": { "tools": { "listChanged": false } },
                "protocolVersions": MCP_SUPPORTED,
                "instructions": INSTRUCTIONS,
            }),
        )),
        // Removed by 2026-07-28 and kept because legacy clients send it and it
        // costs nothing.
        "ping" => Some(ok(id, json!({}))),
        "tools/list" => Some(ok(
            id,
            json!({
                "tools": tools(),
                // A tool list changes only when the binary does, so a client
                // may hold it for as long as it holds the process.
                "ttlMs": 3_600_000,
                "cacheScope": "session",
            }),
        )),
        "tools/call" => Some(call(id, &params, root)),
        _ => Some(error(id, -32601, &format!("unknown method {method}"), None)),
    }
}

const INSTRUCTIONS: &str = "\
Prepares photographs for Instagram's compression, on this machine. Name paths \
to files or directories; converted JPEGs are written to disk and the result \
reports paths and sizes rather than image bytes. Start with ig_plan to see \
what a conversion would do, then ig_convert to do it. The default delivers the \
whole frame at up to 1440px wide so the framing choice stays in the Instagram \
app. Matching width does not guarantee that Instagram skips resampling. \
Outputs are colour-managed SDR sRGB JPEGs; detected PQ/HLG input requires a \
reviewed SDR export first.";

// ── results ───────────────────────────────────────────────────────────

fn envelope(mut result: Map<String, Value>) -> Value {
    // Emitted unconditionally, which is safe in both directions: a JSON-RPC
    // client ignores unknown result fields, and the modern spec tells a modern
    // client to read a missing `resultType` as complete anyway. One code path
    // beats two.
    result.insert("resultType".into(), json!("complete"));
    result.insert(
        "_meta".into(),
        json!({
            "io.modelcontextprotocol/serverInfo": {
                "name": SERVER_NAME, "version": SERVER_VERSION
            }
        }),
    );
    Value::Object(result)
}

fn ok(id: Value, result: Value) -> Value {
    let map = match result {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".into(), other);
            m
        }
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": envelope(map) })
}

fn error(id: Value, code: i32, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({ "code": code, "message": message });
    if let Some(d) = data {
        e["data"] = d;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": e })
}

/// A tool that failed at its own job rather than at the protocol. `isError`
/// keeps the failure inside the result, which is what lets a model read it and
/// try something else instead of the whole call unwinding.
fn tool_error(id: Value, message: String) -> Value {
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    )
}

fn tool_ok(id: Value, text: String, structured: Value) -> Value {
    ok(
        id,
        json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": structured,
            "isError": false,
        }),
    )
}

// ── tools ─────────────────────────────────────────────────────────────

/// Shared by all three tools: what to operate on.
fn paths_schema(verb: &str) -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "minItems": 1,
        "description": format!(
            "Files or directories on this machine to {verb}. A directory is \
             read one level deep for jpg, jpeg, heif, heic, hif, jxl, png and \
             avif."
        ),
    })
}

fn geometry_props(map: &mut Map<String, Value>) {
    map.insert(
        "fit".into(),
        json!({
            "type": "string",
            "enum": ["full", "crop", "pad"],
            "default": "full",
            "description": "full delivers the whole frame at the target width and leaves the \
                            framing choice to the Instagram app. crop takes the nearest allowed \
                            ratio (3:4 to 1.91:1) here instead. pad keeps the whole frame and \
                            fills the rest within the requested canvas width.",
        }),
    );
    map.insert(
        "gravity".into(),
        json!({
            "type": "string",
            "enum": ["center", "top", "bottom"],
            "default": "center",
            "description": "Where fit=crop takes its window. Ignored by the other two.",
        }),
    );
    map.insert(
        "width".into(),
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": 16384,
            "default": 1440,
            "description": "Target delivery width in pixels. 1440 is a deliberate guess rather \
                            than a measured fact. Compare 1080 and 1440 on your upload path. \
                            Padding counts towards this limit. Source pixels never enlarge.",
        }),
    );
}

fn tools() -> Value {
    let mut plan_props = Map::new();
    plan_props.insert("paths".into(), paths_schema("inspect"));
    geometry_props(&mut plan_props);

    let mut convert_props = plan_props.clone();
    convert_props.insert(
        "out_dir".into(),
        json!({
            "type": "string",
            "default": "ig",
            "description": "Directory to write JPEGs into, created if missing. Each output is \
                            named after its source stem.",
        }),
    );
    convert_props.insert("dither".into(), json!({"type":"boolean", "default":false, "description":"Experimental deterministic dither at final 8-bit quantization. Compare returned Instagram images before enabling by default."}));
    convert_props.insert(
        "quality".into(),
        json!({
            "type": "integer", "minimum": 1, "maximum": 100, "default": 95,
            "description": "JPEG quality. High by default because Instagram re-encodes whatever \
                            it receives, and every pre-compression pass compounds with that one.",
        }),
    );
    convert_props.insert(
        "chroma".into(),
        json!({
            "type": "string",
            "enum": ["444", "422", "420"],
            "default": "444",
            "description": "Output chroma resolution. 444 hands Instagram's own encoder \
                            full-resolution chroma; 422 halves it horizontally, and 420 halves \
                            both axes. Compare returned images before choosing a setting.",
        }),
    );
    convert_props.insert(
        "pad_color".into(),
        json!({
            "type": "string",
            "default": "ffffff",
            "description": "Six-digit hex fill for fit=pad. Ignored otherwise.",
        }),
    );

    json!([
        {
            "name": "ig_plan",
            "title": "Plan an Instagram conversion",
            "description": "Report what converting these photographs would do — the display \
                            dimensions after rotation, the size each would be delivered at, \
                            and whether the frame falls outside the band Instagram shows \
                            uncropped. Writes nothing. Run this first on a directory, both to \
                            see the shape of the batch and to find files that will not decode.",
            "inputSchema": { "type": "object", "properties": plan_props, "required": ["paths"] },
            "outputSchema": plan_output_schema(),
            "annotations": {
                "title": "Plan an Instagram conversion",
                "readOnlyHint": true, "destructiveHint": false,
                "idempotentHint": true, "openWorldHint": false,
            },
        },
        {
            "name": "ig_convert",
            "title": "Convert photographs for Instagram",
            "description": "Convert photographs and write sRGB JPEGs to disk. Reads JPEG, \
                            HEIF/HEIC/HIF, JPEG XL and PNG. Rotation is read from metadata and \
                            baked into the pixels, the downscale runs in linear light with \
                            Lanczos3, and the result reports output paths and byte counts \
                            rather than image data. HEIF and JPEG XL need `sips` (macOS) or \
                            `djxl` on PATH; a file that cannot be decoded is reported by name \
                            rather than skipped silently.",
            "inputSchema": { "type": "object", "properties": convert_props, "required": ["paths"] },
            "outputSchema": convert_output_schema(),
            "annotations": {
                "title": "Convert photographs for Instagram",
                // Writes new files, so not read-only. Not destructive: it only
                // ever creates, and it creates under out_dir rather than beside
                // the source. Idempotent because the same input and options
                // produce the same bytes at the same path.
                "readOnlyHint": false, "destructiveHint": false,
                "idempotentHint": true, "openWorldHint": false,
            },
        },
        {
            "name": "ig_check_rotation",
            "title": "Check for contradictory rotation metadata",
            "description": "Report photographs whose two statements of their own rotation \
                            disagree. A HEIF can say how to turn itself twice, once as a \
                            container transform and once as EXIF Orientation; saying it twice \
                            is normal and every Fujifilm HIF does it. Saying two DIFFERENT \
                            things is the fault, because then no viewer is right and the \
                            photograph has no defined orientation. Writes nothing.",
            "inputSchema": {
                "type": "object",
                "properties": { "paths": paths_schema("check") },
                "required": ["paths"],
            },
            "outputSchema": check_output_schema(),
            "annotations": {
                "title": "Check for contradictory rotation metadata",
                "readOnlyHint": true, "destructiveHint": false,
                "idempotentHint": true, "openWorldHint": false,
            },
        },
    ])
}

fn file_props(converted: bool) -> Value {
    let mut p = json!({
        "source": { "type": "string" },
        "from": {
            "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2,
            "description": "Display width and height after rotation. Absent if it would not decode.",
        },
        "to": {
            "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2,
            "description": "Delivered width and height.",
        },
        "outside_band": {
            "type": "boolean",
            "description": "True when the source ratio falls outside 3:4 to 1.91:1, so \
                            Instagram will crop it unless fit=crop or fit=pad handled it here.",
        },
        "error": { "type": "string", "description": "Present only if this file failed." },
    });
    if converted {
        p["output"] = json!({ "type": "string", "description": "Path of the written JPEG." });
        p["bytes"] = json!({ "type": "integer" });
        p["chroma"] = json!({ "type": "string" });
    }
    p
}

fn plan_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "files": { "type": "array", "items": { "type": "object", "properties": file_props(false) } },
            "planned": { "type": "integer" },
            "failed": { "type": "integer" },
            "not_shown": {
                "type": "integer",
                "description": "Files matched but not reported, because the batch hit its cap.",
            },
        },
        "required": ["files", "planned", "failed"],
    })
}

fn convert_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "files": { "type": "array", "items": { "type": "object", "properties": file_props(true) } },
            "written": { "type": "integer" },
            "failed": { "type": "integer" },
            "out_dir": { "type": "string" },
            "not_converted": {
                "type": "integer",
                "description": "Files matched but not converted, because the batch hit its cap.",
            },
        },
        "required": ["files", "written", "failed", "out_dir"],
    })
}

fn check_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "source": { "type": "string" },
                        "agreement": {
                            "type": "string",
                            "enum": ["both", "disagree", "one", "neither", "unreadable"],
                        },
                        "container": { "type": "string" },
                        "exif": { "type": "string" },
                        "error": { "type": "string" },
                    },
                },
            },
            "disagree": { "type": "integer" },
            "checked": { "type": "integer" },
            "not_checked": { "type": "integer" },
        },
        "required": ["files", "disagree", "checked"],
    })
}

// ── calling them ──────────────────────────────────────────────────────

fn call(id: Value, params: &Value, root: Option<&Path>) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // The NAME is checked before the arguments, because an unknown tool is a
    // protocol fault rather than a bad request, and reporting a path problem
    // for a tool that does not exist sends the caller to fix the wrong thing.
    if !["ig_plan", "ig_convert", "ig_check_rotation"].contains(&name) {
        return error(id, -32602, &format!("unknown tool {name}"), None);
    }

    let (files, overflow) = match inputs(&args, root) {
        Ok(v) => v,
        Err(e) => return tool_error(id, e),
    };

    match name {
        "ig_plan" => {
            let mut opts = match geometry_opts(&args) {
                Ok(o) => o,
                Err(e) => return tool_error(id, e),
            };
            opts.dry = true;
            let reports = crate::run_all(&files, &opts);
            let failed = reports.iter().filter(|r| r.error.is_some()).count();
            let mut text = lines(&reports);
            if overflow > 0 {
                text.push_str(&overflow_note(overflow, "planned"));
            }
            let mut out = json!({
                "files": reports.iter().map(report_json_plan).collect::<Vec<_>>(),
                "planned": reports.len() - failed,
                "failed": failed,
            });
            if overflow > 0 {
                out["not_shown"] = json!(overflow);
            }
            tool_ok(id, text, out)
        }
        "ig_convert" => {
            let opts = match convert_opts(&args, root) {
                Ok(o) => o,
                Err(e) => return tool_error(id, e),
            };
            if let Err(e) = std::fs::create_dir_all(&opts.out_dir) {
                return tool_error(id, format!("{}: {e}", opts.out_dir.display()));
            }
            let reports = crate::run_all(&files, &opts);
            let failed = reports.iter().filter(|r| r.error.is_some()).count();
            let mut text = lines(&reports);
            if overflow > 0 {
                text.push_str(&overflow_note(overflow, "converted"));
            }
            let mut out = json!({
                "files": reports.iter().map(report_json_full).collect::<Vec<_>>(),
                "written": reports.len() - failed,
                "failed": failed,
                "out_dir": opts.out_dir.display().to_string(),
            });
            if overflow > 0 {
                out["not_converted"] = json!(overflow);
            }
            tool_ok(id, text, out)
        }
        "ig_check_rotation" => {
            let checks: Vec<_> = files.iter().map(|p| crate::check_one(p)).collect();
            let disagree = checks
                .iter()
                .filter(|c| matches!(c.agreement, Agreement::Disagree))
                .count();
            let mut text = String::new();
            for c in &checks {
                match c.agreement {
                    Agreement::Disagree => text.push_str(&format!(
                        "{}: DISAGREE  container says {}, EXIF says {}\n",
                        c.name,
                        c.container.clone().unwrap_or_default(),
                        c.exif.clone().unwrap_or_default()
                    )),
                    Agreement::Unreadable => text.push_str(&format!(
                        "{}: {}\n",
                        c.name,
                        c.error.clone().unwrap_or_default()
                    )),
                    _ => {}
                }
            }
            text.push_str(&format!(
                "{} files checked, {disagree} disagree.",
                checks.len()
            ));
            if disagree > 0 {
                text.push_str(&format!("\n\n{}", crate::DISAGREE_NOTE));
            }
            if overflow > 0 {
                text.push_str(&overflow_note(overflow, "checked"));
            }
            let mut out = json!({
                "files": checks.iter().map(|c| {
                    let mut m = json!({
                        "source": c.source.display().to_string(),
                        "agreement": c.agreement.label(),
                    });
                    if let Some(v) = &c.container { m["container"] = json!(v); }
                    if let Some(v) = &c.exif { m["exif"] = json!(v); }
                    if let Some(v) = &c.error { m["error"] = json!(v); }
                    m
                }).collect::<Vec<_>>(),
                "disagree": disagree,
                "checked": checks.len(),
            });
            if overflow > 0 {
                out["not_checked"] = json!(overflow);
            }
            tool_ok(id, text, out)
        }
        // Unreachable: the guard above already refused anything else. Kept so
        // adding a tool to that list without adding an arm fails loudly here
        // rather than silently answering nothing.
        other => error(
            id,
            -32603,
            &format!("tool {other} is listed but unimplemented"),
            None,
        ),
    }
}

fn lines(reports: &[Report]) -> String {
    let mut s = String::new();
    for r in reports {
        s.push_str(&r.line());
        s.push('\n');
    }
    s
}

fn overflow_note(n: usize, verb: &str) -> String {
    format!(
        "\n{n} further file(s) matched and were not {verb}: a single call handles at most \
         {MAX_FILES}. Name a narrower set of paths to reach the rest."
    )
}

/// An undefined field is OMITTED rather than sent as null, so a reader can
/// tell "this file did not decode" from "this file decoded to nothing".
fn report_json_plan(r: &Report) -> Value {
    let mut m = json!({ "source": r.source.display().to_string() });
    if let Some((w, h)) = r.from {
        m["from"] = json!([w, h]);
    }
    if let Some((w, h)) = r.to {
        m["to"] = json!([w, h]);
        m["outside_band"] = json!(r.outside_band);
    }
    if let Some(e) = &r.error {
        m["error"] = json!(e);
    }
    m
}

fn report_json_full(r: &Report) -> Value {
    let mut m = report_json_plan(r);
    if let Some(o) = &r.output {
        m["output"] = json!(o.display().to_string());
        m["bytes"] = json!(r.bytes.unwrap_or(0));
        m["chroma"] = json!(r.chroma);
    }
    m
}

// ── arguments ─────────────────────────────────────────────────────────

fn geometry_opts(args: &Value) -> Result<Opts, String> {
    let mut o = Opts::defaults();
    match args.get("fit").and_then(Value::as_str) {
        None | Some("full") => o.fit = Fit::Full,
        Some("crop") => o.fit = Fit::Crop,
        Some("pad") => o.fit = Fit::Pad,
        Some(x) => return Err(format!("fit must be full, crop or pad, got {x:?}")),
    }
    match args.get("gravity").and_then(Value::as_str) {
        None | Some("center") => o.gravity = Gravity::Center,
        Some("top") => o.gravity = Gravity::Top,
        Some("bottom") => o.gravity = Gravity::Bottom,
        Some(x) => return Err(format!("gravity must be center, top or bottom, got {x:?}")),
    }
    if let Some(w) = args.get("width") {
        let w = w
            .as_u64()
            .filter(|w| (1..=16384).contains(w))
            .ok_or("width must be a whole number of pixels between 1 and 16384")?;
        o.width = w as u32;
    }
    Ok(o)
}

fn convert_opts(args: &Value, root: Option<&Path>) -> Result<Opts, String> {
    let mut o = geometry_opts(args)?;
    o.dry = false;
    if let Some(dither) = args.get("dither") {
        o.dither = dither.as_bool().ok_or("dither must be a boolean")?;
    }
    if let Some(q) = args.get("quality") {
        let q = q
            .as_u64()
            .filter(|q| (1..=100).contains(q))
            .ok_or("quality must be a whole number between 1 and 100")?;
        o.quality = q as u8;
    }
    match args.get("chroma") {
        None => {}
        Some(v) => match v.as_str() {
            Some("444") => o.chroma = Chroma::Full,
            Some("422") => o.chroma = Chroma::Halved,
            Some("420") => o.chroma = Chroma::Quartered,
            other => return Err(format!("chroma must be 444, 422 or 420, got {other:?}")),
        },
    }
    if let Some(c) = args.get("pad_color").and_then(Value::as_str) {
        let h = c.trim_start_matches('#');
        if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("pad_color must be six hex digits, got {c:?}"));
        }
        o.set_pad_color(c);
    }
    let dir = args.get("out_dir").and_then(Value::as_str).unwrap_or("ig");
    o.out_dir = under_root(Path::new(dir), root, false)?;
    Ok(o)
}

fn inputs(args: &Value, root: Option<&Path>) -> Result<(Vec<PathBuf>, usize), String> {
    let list = args
        .get("paths")
        .and_then(Value::as_array)
        .ok_or("paths is required and must be an array of strings")?;
    if list.is_empty() {
        return Err("paths must name at least one file or directory".into());
    }
    let mut given = Vec::new();
    for v in list {
        let s = v.as_str().ok_or("every entry in paths must be a string")?;
        given.push(under_root(Path::new(s), root, true)?);
    }
    let mut files = crate::expand(&given);
    if files.is_empty() {
        return Err("no readable images at those paths".into());
    }
    // Re-checked after expansion rather than trusted: a directory inside the
    // root can hold a symlink pointing outside it, and the entry that opened
    // the directory says nothing about that.
    for f in &files {
        under_root(f, root, true)?;
    }
    let overflow = files.len().saturating_sub(MAX_FILES);
    files.truncate(MAX_FILES);
    Ok((files, overflow))
}

/// Resolve a caller-supplied path, and refuse it if `--root` was given and it
/// lands outside.
///
/// Symlinks are resolved before the comparison, because comparing the string a
/// caller wrote against the root would let `root/link-to-elsewhere` pass. For
/// a path that does not exist yet, which is the `out_dir` case, the deepest
/// EXISTING ancestor is resolved and the remaining components are appended
/// after `.` and `..` have been folded away; a lexical check alone would let a
/// symlinked parent escape, and requiring existence would refuse to create an
/// output directory at all.
fn under_root(p: &Path, root: Option<&Path>, must_exist: bool) -> Result<PathBuf, String> {
    let resolved = resolve(p, must_exist)?;
    match root {
        Some(r) if !resolved.starts_with(r) => Err(format!(
            "{} is outside the root this server was started with ({})",
            resolved.display(),
            r.display()
        )),
        _ => Ok(resolved),
    }
}

fn resolve(p: &Path, must_exist: bool) -> Result<PathBuf, String> {
    if let Ok(c) = p.canonicalize() {
        return Ok(c);
    }
    if must_exist {
        return Err(format!("{}: no such file or directory", p.display()));
    }
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_err(|e| e.to_string())?.join(p)
    };
    // Fold `.` and `..` away first, so the split below cannot be fooled by a
    // tail that climbs back out.
    let mut folded = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                folded.pop();
            }
            other => folded.push(other),
        }
    }
    // Walk up to something real, resolve that, and put the tail back.
    let mut tail = Vec::new();
    let mut probe = folded.clone();
    loop {
        if let Ok(c) = probe.canonicalize() {
            let mut out = c;
            for t in tail.iter().rev() {
                out.push(t);
            }
            return Ok(out);
        }
        match probe.file_name() {
            Some(n) => {
                tail.push(n.to_os_string());
                probe.pop();
            }
            None => return Ok(folded),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call_tool(name: &str, args: Value, root: Option<&Path>) -> Value {
        call(json!(1), &json!({ "name": name, "arguments": args }), root)
    }

    fn text_of(v: &Value) -> String {
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("")
            .into()
    }

    fn is_tool_error(v: &Value) -> bool {
        v["result"]["isError"].as_bool().unwrap_or(false)
    }

    /// A real directory pair, since the boundary is about paths that EXIST.
    /// Built rather than borrowed from the filesystem: an earlier version
    /// reached for `/etc`, which passes on Unix and fails on Windows for a
    /// reason that has nothing to do with the boundary being tested.
    fn sandbox(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ig-prep-{tag}-{}", std::process::id()));
        let (root, outside) = (base.join("root"), base.join("outside"));
        std::fs::create_dir_all(root.join("in")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("in").join("a.jpg"), b"not really a jpeg").unwrap();
        std::fs::write(outside.join("b.jpg"), b"also not a jpeg").unwrap();
        (root, outside)
    }

    // ── the protocol ──────────────────────────────────────────────

    #[test]
    fn a_notification_gets_no_reply() {
        let n = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(&n, None).is_none());
        // An explicit null id is the same case.
        let n = json!({ "jsonrpc": "2.0", "id": null, "method": "ping" });
        assert!(handle(&n, None).is_none());
    }

    #[test]
    fn initialize_echoes_a_revision_it_speaks() {
        for asked in ["2025-06-18", "2025-03-26", "2024-11-05"] {
            let r = handle(
                &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                        "params":{"protocolVersion": asked}}),
                None,
            )
            .unwrap();
            assert_eq!(r["result"]["protocolVersion"], asked);
        }
    }

    /// A client that opened with `initialize` is by definition not speaking the
    /// revision that deleted `initialize`, so an unknown version must not be
    /// answered with 2026-07-28.
    #[test]
    fn initialize_never_answers_with_the_modern_revision() {
        let r = handle(
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize",
                    "params":{"protocolVersion":"1999-01-01"}}),
            None,
        )
        .unwrap();
        assert_ne!(r["result"]["protocolVersion"], MCP_PROTOCOL);
        assert_eq!(r["result"]["protocolVersion"], MCP_SUPPORTED[1]);
    }

    #[test]
    fn an_unsupported_meta_version_is_refused_by_code() {
        let r = handle(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/list",
                    "params":{"_meta":{"protocolVersion":"1999-01-01"}}}),
            None,
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32022);
        // The client can only retry if it is told what would work.
        assert!(r["error"]["data"]["supported"].is_array());
        assert_eq!(r["error"]["data"]["requested"], "1999-01-01");
    }

    /// An absent `_meta` is how a legacy client presents itself, so it cannot
    /// also be read as malformed.
    #[test]
    fn no_meta_is_legacy_rather_than_an_error() {
        let r = handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}), None).unwrap();
        assert!(r["error"].is_null());
        assert!(r["result"]["tools"].is_array());
    }

    #[test]
    fn discover_reports_every_revision() {
        let r = handle(
            &json!({"jsonrpc":"2.0","id":1,"method":"server/discover"}),
            None,
        )
        .unwrap();
        let vs = r["result"]["protocolVersions"].as_array().unwrap();
        assert_eq!(vs.len(), MCP_SUPPORTED.len());
        assert_eq!(vs[0], MCP_PROTOCOL);
    }

    #[test]
    fn every_result_carries_the_completion_marker() {
        for m in ["initialize", "server/discover", "ping", "tools/list"] {
            let r = handle(&json!({"jsonrpc":"2.0","id":1,"method":m}), None).unwrap();
            assert_eq!(r["result"]["resultType"], "complete", "{m}");
            assert!(!r["result"]["_meta"]["io.modelcontextprotocol/serverInfo"].is_null());
        }
    }

    // ── the catalogue ─────────────────────────────────────────────

    /// The write tool must not advertise itself as a read. A model choosing
    /// what it may run unattended reads these and nothing else.
    #[test]
    fn only_the_converter_declares_itself_a_write() {
        let tools = tools();
        for t in tools.as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            let read_only = t["annotations"]["readOnlyHint"].as_bool().unwrap();
            assert_eq!(read_only, name != "ig_convert", "{name}");
            // Nothing here reaches the network or a model.
            assert_eq!(t["annotations"]["openWorldHint"], false, "{name}");
            assert_eq!(t["annotations"]["destructiveHint"], false, "{name}");
            assert!(
                t["inputSchema"]["properties"]["paths"].is_object(),
                "{name}"
            );
            assert_eq!(t["inputSchema"]["required"][0], "paths", "{name}");
        }
        assert_eq!(tools.as_array().unwrap().len(), 3);
    }

    // ── arguments ─────────────────────────────────────────────────

    #[test]
    fn defaults_match_the_command_line() {
        let o = geometry_opts(&json!({})).unwrap();
        assert_eq!(o.width, crate::geometry::TARGET_WIDTH);
        assert_eq!(o.fit, Fit::Full);
        assert_eq!(o.gravity, Gravity::Center);
        let c = convert_opts(&json!({}), None).unwrap();
        assert_eq!(c.quality, 95);
        assert!(!c.dither);
        assert!(convert_opts(&json!({"dither":true}), None).unwrap().dither);
        assert!(convert_opts(&json!({"dither":"yes"}), None).is_err());
        assert!(c.chroma == Chroma::Full);
    }

    /// A misspelled enum is refused rather than silently defaulted, because
    /// silently defaulting hands back a conversion nobody asked for and
    /// reports success.
    #[test]
    fn bad_values_are_refused_rather_than_defaulted() {
        assert!(geometry_opts(&json!({ "fit": "cropp" })).is_err());
        assert!(geometry_opts(&json!({ "gravity": "middle" })).is_err());
        assert!(geometry_opts(&json!({ "width": 0 })).is_err());
        assert!(geometry_opts(&json!({ "width": -5 })).is_err());
        assert!(geometry_opts(&json!({ "width": "1440" })).is_err());
        assert!(convert_opts(&json!({ "quality": 0 }), None).is_err());
        assert!(convert_opts(&json!({ "quality": 101 }), None).is_err());
        assert!(convert_opts(&json!({ "chroma": "411" }), None).is_err());
        assert!(convert_opts(&json!({ "pad_color": "xyzxyz" }), None).is_err());
        assert!(convert_opts(&json!({ "pad_color": "fff" }), None).is_err());
    }

    #[test]
    fn paths_must_be_a_non_empty_array_of_strings() {
        assert!(inputs(&json!({}), None).is_err());
        assert!(inputs(&json!({ "paths": [] }), None).is_err());
        assert!(inputs(&json!({ "paths": "one.jpg" }), None).is_err());
        assert!(inputs(&json!({ "paths": [7] }), None).is_err());
    }

    // ── the boundary ──────────────────────────────────────────────

    #[test]
    fn root_refuses_a_path_outside_it() {
        let (root, outside) = sandbox("root");
        let inside = root.join("in");

        assert!(under_root(&inside, Some(&root), true).is_ok());
        // The classic escape, and the one a lexical check alone would miss if
        // it ran after the join rather than before.
        assert!(under_root(&inside.join("../.."), Some(&root), true).is_err());
        assert!(under_root(&outside, Some(&root), true).is_err());
        // An output directory that does not exist yet still has to land inside.
        assert!(under_root(&root.join("out"), Some(&root), false).is_ok());
        assert!(under_root(&root.join("../out"), Some(&root), false).is_err());
        // With no root, a real path outside resolves fine.
        assert!(under_root(&outside, None, true).is_ok());

        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    /// A symlink inside the root that points out of it must not widen the
    /// boundary, because the comparison is what stops it and a comparison
    /// against the string the caller wrote would pass.
    #[test]
    #[cfg(unix)]
    fn a_symlink_cannot_widen_the_root() {
        let dir = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ig-prep-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("escape");
        std::os::unix::fs::symlink("/etc", &link).ok();
        assert!(under_root(&link, Some(&dir), true).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_confined_call_reports_the_refusal_as_a_tool_error() {
        let (root, outside) = sandbox("call");
        // A tool error rather than a JSON-RPC error: the model should be able
        // to read it and choose another path, rather than have the call unwind.
        let target = outside.join("b.jpg").display().to_string();
        let r = call_tool("ig_plan", json!({ "paths": [target] }), Some(&root));
        assert!(is_tool_error(&r));
        assert!(text_of(&r).contains("outside the root"));
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    /// Every name the catalogue advertises has to reach a dispatch arm. The
    /// guard and the `match` list the same three names, and this is what keeps
    /// them together when a fourth is added.
    #[test]
    fn every_advertised_tool_dispatches() {
        let (root, _outside) = sandbox("dispatch");
        let inside = root.join("in").display().to_string();
        let out = root.join("out").display().to_string();
        for t in tools().as_array().unwrap() {
            let name = t["name"].as_str().unwrap();
            let r = call_tool(
                name,
                json!({ "paths": [inside], "out_dir": out }),
                Some(&root),
            );
            assert!(
                r["error"].is_null(),
                "{name} is advertised but did not dispatch: {}",
                r["error"]
            );
        }
        std::fs::remove_dir_all(root.parent().unwrap()).ok();
    }

    #[test]
    fn an_unknown_tool_is_a_protocol_error() {
        // Refused on the NAME, before its arguments are resolved, so a caller
        // is told the thing that is actually wrong. The path here does not
        // exist on any platform, which is the point.
        let r = call_tool(
            "ig_nope",
            json!({ "paths": ["/nonexistent/anywhere.jpg"] }),
            None,
        );
        assert_eq!(r["error"]["code"], -32602);
        assert!(r["error"]["message"].as_str().unwrap().contains("ig_nope"));
    }
}
