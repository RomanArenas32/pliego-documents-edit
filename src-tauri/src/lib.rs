mod batch;
#[cfg(feature = "bridge")]
pub mod bridge;
mod fonts;
mod forms;
mod history;
mod images;
mod markup;
mod pdf;
mod redact;
mod text;
mod tools;
mod word;

use std::sync::Mutex;

use tauri::{AppHandle, Emitter, Manager};

/// PDFs the system asked to open (double click, "Open with…") that the UI hasn't opened yet.
#[derive(Default)]
struct PendingFiles(Mutex<Vec<String>>);

fn request_open(app: &AppHandle, paths: Vec<String>) {
    let paths: Vec<String> = paths.into_iter().filter(|p| p.to_lowercase().ends_with(".pdf")).collect();
    if paths.is_empty() {
        return;
    }
    if let Ok(mut pending) = app.state::<PendingFiles>().0.lock() {
        pending.extend(paths);
    }
    let _ = app.emit("open-files", ());
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// PDF paths among the program arguments (Windows and Linux open files this way).
fn pdf_args(args: impl IntoIterator<Item = String>, cwd: Option<&std::path::Path>) -> Vec<String> {
    args.into_iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .map(|a| match cwd {
            Some(dir) if std::path::Path::new(&a).is_relative() => dir.join(&a).to_string_lossy().into_owned(),
            _ => a,
        })
        .collect()
}

#[tauri::command]
fn take_pending_files(state: tauri::State<'_, PendingFiles>) -> Vec<String> {
    state.0.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    // If the app is already open, opening another PDF sends it to that window instead of a new copy.
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
        request_open(app, pdf_args(args, Some(std::path::Path::new(&cwd))));
    }));
    builder
        .plugin(tauri_plugin_dialog::init())
        .manage(PendingFiles::default())
        .manage(pdf::AppState {
            doc: Mutex::new(None),
        })
        .setup(|app| {
            let resource_dir = app.path().resource_dir().ok();
            pdf::init_pdfium(resource_dir)?;
            std::thread::spawn(history::clean_old);
            fonts::warm_up();
            request_open(app.handle(), pdf_args(std::env::args(), None));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            take_pending_files,
            pdf::open_pdf,
            pdf::save_pdf,
            pdf::render_page,
            pdf::undo,
            pdf::redo,
            pdf::rotate_page,
            pdf::delete_page,
            pdf::reorder_pages,
            pdf::insert_pdf,
            pdf::insert_blank_page,
            pdf::add_text,
            pdf::add_highlight,
            pdf::add_rect,
            pdf::add_ink,
            pdf::erase_at,
            pdf::update_text,
            pdf::annotation_at,
            pdf::delete_annotation,
            pdf::text_object_at,
            pdf::move_text_object,
            pdf::recolor_text_object,
            pdf::edit_text_object,
            forms::form_fields,
            forms::set_form_field,
            images::add_image,
            tools::extract_pages,
            tools::split_pdf,
            tools::export_images,
            tools::compress_pdf,
            tools::protect_pdf,
            tools::images_to_pdf_command,
            tools::print_window,
            word::export_word,
            markup::add_markup,
            redact::redact,
            markup::add_note,
            markup::set_note,
            images::set_image_rect,
            text::search_text,
            text::text_in_rect,
            batch::read_table_file,
            batch::batch_preview,
            batch::batch_generate,
            batch::batch_cancel,
        ])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|_app, _event| {
            // On macOS files arrive as an event (double click or dropping on the icon).
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            if let tauri::RunEvent::Opened { urls } = _event {
                let paths = urls.iter().filter_map(|u| u.to_file_path().ok()).map(|p| p.to_string_lossy().into_owned());
                request_open(_app, paths.collect());
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_paths_from_arguments() {
        // Absolute paths for the current system ("C:\..." on Windows, "/..." elsewhere).
        let cwd = std::env::temp_dir().join("ana");
        let absolute = std::env::temp_dir().join("otro.PDF").to_string_lossy().into_owned();
        let args = ["pliego".into(), "-psn_0_123".into(), "factura.pdf".into(), absolute.clone()];
        let paths = pdf_args(args, Some(&cwd));
        assert_eq!(paths, [cwd.join("factura.pdf").to_string_lossy().into_owned(), absolute]);
    }
}
