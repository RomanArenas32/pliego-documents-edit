//! Development test bench (`bridge` feature): serves the built UI over HTTP
//! and handles commands with the same backend, so the app can be tested in a browser.
//! Not included in the app.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::pdf::AppState;
use crate::{batch, pdf};

/// Replaces `window.__TAURI_INTERNALS__`: commands go over HTTP; file dialogs
/// return whatever was put in `window.__dialogQueue`.
const SHIM: &str = r#"<script>
(() => {
  const callbacks = new Map();
  let next = 1;
  window.__dialogQueue = [];
  window.__invokeLog = [];
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
  window.__TAURI_INTERNALS__ = {
    metadata: {
      currentWindow: { label: "main" },
      currentWebview: { windowLabel: "main", label: "main" },
    },
    transformCallback(cb) { const id = next++; callbacks.set(id, cb); return id; },
    unregisterCallback(id) { callbacks.delete(id); },
    convertFileSrc: (p) => p,
    async invoke(cmd, args = {}) {
      window.__invokeLog.push(cmd);
      if (cmd.startsWith("plugin:dialog|")) {
        if (cmd.endsWith("|ask") || cmd.endsWith("|confirm")) return true;
        // Dialogs with custom buttons: answers whatever is in the queue (e.g. the button text).
        if (cmd.endsWith("|message")) return window.__dialogQueue.length ? window.__dialogQueue.shift() : null;
        return window.__dialogQueue.length ? window.__dialogQueue.shift() : null;
      }
      if (cmd === "plugin:event|listen") return next++;
      if (cmd.startsWith("plugin:")) return null;
      const r = await fetch("/invoke/" + cmd, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(args ?? {}),
      });
      if (!r.ok) throw await r.text();
      const type = r.headers.get("content-type") || "";
      return type.includes("json") ? r.json() : r.arrayBuffer();
    },
  };
})();
</script>"#;

enum Reply {
    Json(Value),
    Bytes(Vec<u8>),
}

/// For each command: name, arguments (as the UI sends them) and the call.
macro_rules! dispatch {
    ($cmd:expr, $args:expr, { $( $name:ident ( $($arg:ident : $ty:ty),* ) => $call:expr ),* $(,)? }) => {
        match $cmd {
            $( stringify!($name) => {
                #[derive(serde::Deserialize)]
                #[allow(non_snake_case)]
                struct Args { $( $arg: $ty ),* }
                #[allow(unused_variables)]
                let Args { $($arg),* } = serde_json::from_value::<Args>($args).map_err(|e| e.to_string())?;
                Reply::Json(serde_json::to_value($call?).map_err(|e| e.to_string())?)
            } )*
            other => return Err(format!("comando desconocido: {other}")),
        }
    };
}

fn call(state: &AppState, cmd: &str, args: Value) -> Result<Reply, String> {
    use pdfium_render::prelude::{PdfPageAnnotationIndex as Annot, PdfPageIndex as Page, PdfPageObjectIndex as Obj};
    use pdf::{RelPoint as P, TextLine};

    Ok(match cmd {
        "render_page" => {
            #[derive(serde::Deserialize)]
            struct Args { index: Page, width: u32 }
            let a: Args = serde_json::from_value(args).map_err(|e| e.to_string())?;
            Reply::Bytes(state.render_page(a.index, a.width)?)
        }
        "batch_preview" => {
            #[derive(serde::Deserialize)]
            struct Args { fields: Vec<batch::Field>, row: Vec<String>, page: Page, width: u32 }
            let a: Args = serde_json::from_value(args).map_err(|e| e.to_string())?;
            Reply::Bytes(batch::preview_png(state, &a.fields, &a.row, a.page, a.width)?)
        }
        _ => dispatch!(cmd, args, {
            open_pdf(path: String, password: Option<String>) => state.open_pdf(path, password),
            save_pdf(path: Option<String>) => state.save_pdf(path),
            undo() => state.undo(),
            redo() => state.redo(),
            rotate_page(index: Page, clockwise: bool) => state.rotate_page(index, clockwise),
            delete_page(index: Page) => state.delete_page(index),
            reorder_pages(order: Vec<Page>) => state.reorder_pages(order),
            insert_pdf(path: String, at: Page) => state.insert_pdf(path, at),
            insert_blank_page(at: Page, like: Page) => state.insert_blank_page(at, like),
            add_text(index: Page, lines: Vec<TextLine>, size: f32, color: String) =>
                state.add_text(index, lines, size, color),
            add_highlight(index: Page, from: P, to: P, color: String) =>
                state.add_highlight(index, from, to, color),
            add_rect(index: Page, from: P, to: P, width: f32, color: String) =>
                state.add_rect(index, from, to, width, color),
            add_ink(index: Page, points: Vec<P>, width: f32, color: String) =>
                state.add_ink(index, points, width, color),
            erase_at(index: Page, at: P) => state.erase_at(index, at),
            update_text(index: Page, annotation: Annot, lines: Vec<TextLine>, size: f32, color: String) =>
                state.update_text(index, annotation, lines, size, color),
            annotation_at(index: Page, at: P) => state.annotation_at(index, at),
            delete_annotation(index: Page, annotation: Annot) => state.delete_annotation(index, annotation),
            text_object_at(index: Page, at: P) => state.text_object_at(index, at),
            move_text_object(index: Page, objects: Vec<Obj>, from: P, to: P) =>
                state.move_text_object(index, objects, from, to),
            recolor_text_object(index: Page, objects: Vec<Obj>, color: String) =>
                state.recolor_text_object(index, objects, color),
            edit_text_object(index: Page, objects: Vec<Obj>, text: String, color: Option<String>) =>
                state.edit_text_object(index, objects, text, color),
            search_text(query: String) => state.search_text(&query),
            form_fields(index: Page) => state.form_fields(index),
            set_form_field(index: Page, annotation: Annot, edit: crate::forms::FieldEdit) =>
                state.set_form_field(index, annotation, edit),
            add_markup(index: Page, rects: Vec<crate::pdf::RelRect>, kind: crate::markup::MarkupKind, color: String) =>
                state.add_markup(index, rects, kind, color),
            add_note(index: Page, at: crate::pdf::RelPoint, text: String, color: String) =>
                state.add_note(index, at, text, color),
            set_note(index: Page, annotation: Annot, text: String) => state.set_note(index, annotation, text),
            extract_pages(pages: Vec<Page>, path: String) => state.extract_pages(pages, path),
            split_pdf(every: Page, dir: String) => state.split_pdf(every, dir),
            export_images(pages: Vec<Page>, dir: String, format: crate::tools::ImageFormat, dpi: u32) =>
                state.export_images(pages, dir, format, dpi),
            compress_pdf(path: String, level: crate::tools::CompressLevel) => state.compress_pdf(path, level),
            protect_pdf(path: String, password: String) => state.protect_pdf(path, password),
            images_to_pdf(paths: Vec<String>, output: String) => crate::tools::images_to_pdf(paths, output),
            redact(areas: Vec<crate::redact::RedactArea>) => state.redact(areas),
            export_word(path: String) => state.export_word(path),
            take_pending_files() => Ok::<Vec<String>, String>(Vec::new()),
            add_image(index: Page, source: crate::images::ImageSource, center: P, width: f32) =>
                state.add_image(index, source, center, width),
            set_image_rect(index: Page, annotation: Annot, from: P, to: P) =>
                state.set_image_rect(index, annotation, from, to),
            text_in_rect(index: Page, from: P, to: P) => state.text_in_rect(index, from, to),
            read_table_file(path: String) => batch::read_table(Path::new(&path)),
            batch_generate(fields: Vec<batch::Field>, rows: Vec<Vec<String>>, output: batch::Output) =>
                batch::template_bytes(state).and_then(|t| batch::generate(&t, &fields, &rows, output, |_, _| {})),
            batch_cancel() => Ok::<(), String>(batch::cancel()),
        }),
    })
}

pub fn run(dist: &Path, port: u16) {
    pdf::init_pdfium(None).expect("pdfium");
    let state = Arc::new(AppState { doc: std::sync::Mutex::new(None) });
    let server = tiny_http::Server::http(("127.0.0.1", port)).expect("no se pudo abrir el puerto");
    println!("Banco de pruebas en http://127.0.0.1:{port}");

    for mut request in server.incoming_requests() {
        let state = state.clone();
        let dist = dist.to_path_buf();
        std::thread::spawn(move || {
            let url = request.url().split('?').next().unwrap_or("/").to_string();
            let header = |v: &str| tiny_http::Header::from_bytes("Content-Type", v).unwrap();
            let response = if let Some(cmd) = url.strip_prefix("/invoke/") {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let args = serde_json::from_str(&body).unwrap_or(Value::Null);
                match call(&state, cmd, args) {
                    Ok(Reply::Json(v)) => tiny_http::Response::from_data(v.to_string().into_bytes())
                        .with_header(header("application/json")),
                    Ok(Reply::Bytes(b)) => tiny_http::Response::from_data(b).with_header(header("image/png")),
                    Err(e) => {
                        eprintln!("{cmd}: {e}");
                        tiny_http::Response::from_data(e.into_bytes()).with_status_code(400)
                    }
                }
            } else {
                let rel = if url == "/" { "index.html" } else { url.trim_start_matches('/') };
                match std::fs::read(dist.join(rel)) {
                    Ok(mut bytes) => {
                        let kind = match rel.rsplit('.').next() {
                            Some("html") => "text/html; charset=utf-8",
                            Some("js") => "text/javascript",
                            Some("css") => "text/css",
                            Some("svg") => "image/svg+xml",
                            _ => "application/octet-stream",
                        };
                        if rel == "index.html" {
                            let html = String::from_utf8_lossy(&bytes).replacen("<head>", &format!("<head>{SHIM}"), 1);
                            bytes = html.into_bytes();
                        }
                        tiny_http::Response::from_data(bytes).with_header(header(kind))
                    }
                    Err(_) => tiny_http::Response::from_data(b"no encontrado".to_vec()).with_status_code(404),
                }
            };
            let _ = request.respond(response);
        });
    }
}
