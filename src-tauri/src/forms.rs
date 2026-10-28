//! PDF forms (AcroForm): list the fields of a page and fill them in.
//!
//! Filling uses pdfium's "form fill" API, which simulates what a person
//! would do (focus the field, type, click). That way pdfium also updates the
//! field's appearance and the result looks the same in any other PDF reader.
//! pdfium-render doesn't expose that API, so those functions are loaded from the same library.

use std::ffi::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

use pdfium_render::prelude::*;
use serde::{Deserialize, Serialize};
use tauri::State;

use crate::pdf::{err, get_page, rect_to_rel, AppState, DocInfo, RelRect};

static LIBRARY_PATH: OnceLock<PathBuf> = OnceLock::new();
static FFI: OnceLock<Result<Ffi, String>> = OnceLock::new();

/// Called when pdfium is initialized, with the path of the library that was loaded.
pub(crate) fn set_library_path(path: PathBuf) {
    let _ = LIBRARY_PATH.set(path);
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FieldKind {
    Text,
    Checkbox,
    Radio,
    Combo,
    List,
    Button,
    Signature,
    Unknown,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FormField {
    annotation: PdfPageAnnotationIndex,
    kind: FieldKind,
    name: String,
    value: String,
    checked: bool,
    options: Vec<String>,
    /// Option chosen in list boxes and dropdowns.
    selected: Option<usize>,
    multiline: bool,
    read_only: bool,
    rect: RelRect,
}

/// What to do with a field.
#[derive(Deserialize, Debug)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FieldEdit {
    /// Replaces the text of a text field.
    Text { value: String },
    /// Checks or unchecks a checkbox, or picks a radio button (like a click).
    Toggle,
    /// Picks an option from a list box or dropdown.
    Choice { index: usize },
}

// ---------- pdfium functions loaded by hand ----------

type LoadMem = unsafe extern "C" fn(*const c_void, usize, *const c_char) -> FPDF_DOCUMENT;

struct Ffi {
    _library: libloading::Library,
    load_mem: LoadMem,
    close_document: unsafe extern "C" fn(FPDF_DOCUMENT),
    save_as_copy: unsafe extern "C" fn(FPDF_DOCUMENT, *mut FPDF_FILEWRITE, FPDF_DWORD) -> FPDF_BOOL,
    load_page: unsafe extern "C" fn(FPDF_DOCUMENT, c_int) -> FPDF_PAGE,
    close_page: unsafe extern "C" fn(FPDF_PAGE),
    init_form: unsafe extern "C" fn(FPDF_DOCUMENT, *mut FPDF_FORMFILLINFO) -> FPDF_FORMHANDLE,
    exit_form: unsafe extern "C" fn(FPDF_FORMHANDLE),
    after_load_page: unsafe extern "C" fn(FPDF_PAGE, FPDF_FORMHANDLE),
    before_close_page: unsafe extern "C" fn(FPDF_PAGE, FPDF_FORMHANDLE),
    get_annot: unsafe extern "C" fn(FPDF_PAGE, c_int) -> FPDF_ANNOTATION,
    close_annot: unsafe extern "C" fn(FPDF_ANNOTATION),
    get_rect: unsafe extern "C" fn(FPDF_ANNOTATION, *mut FS_RECTF) -> FPDF_BOOL,
    set_focused: unsafe extern "C" fn(FPDF_FORMHANDLE, FPDF_ANNOTATION) -> FPDF_BOOL,
    kill_focus: unsafe extern "C" fn(FPDF_FORMHANDLE) -> FPDF_BOOL,
    select_all: unsafe extern "C" fn(FPDF_FORMHANDLE, FPDF_PAGE) -> FPDF_BOOL,
    replace_selection: unsafe extern "C" fn(FPDF_FORMHANDLE, FPDF_PAGE, FPDF_WIDESTRING),
    mouse_down: unsafe extern "C" fn(FPDF_FORMHANDLE, FPDF_PAGE, c_int, f64, f64) -> FPDF_BOOL,
    mouse_up: unsafe extern "C" fn(FPDF_FORMHANDLE, FPDF_PAGE, c_int, f64, f64) -> FPDF_BOOL,
    set_index_selected: unsafe extern "C" fn(FPDF_FORMHANDLE, FPDF_PAGE, c_int, FPDF_BOOL) -> FPDF_BOOL,
}

// pdfium functions are always used with the global lock held.
unsafe impl Send for Ffi {}
unsafe impl Sync for Ffi {}

impl Ffi {
    fn get() -> Result<&'static Ffi, String> {
        FFI.get_or_init(|| {
            let path = LIBRARY_PATH.get().ok_or("pdfium no está inicializado")?;
            unsafe { Ffi::load(path) }.map_err(|e| format!("función no disponible: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
    }

    unsafe fn load(path: &PathBuf) -> Result<Ffi, libloading::Error> {
        let library = unsafe { libloading::Library::new(path)? };
        macro_rules! sym {
            ($name:literal) => {
                *unsafe { library.get(concat!($name, "\0").as_bytes())? }
            };
        }
        Ok(Ffi {
            load_mem: sym!("FPDF_LoadMemDocument64"),
            close_document: sym!("FPDF_CloseDocument"),
            save_as_copy: sym!("FPDF_SaveAsCopy"),
            load_page: sym!("FPDF_LoadPage"),
            close_page: sym!("FPDF_ClosePage"),
            init_form: sym!("FPDFDOC_InitFormFillEnvironment"),
            exit_form: sym!("FPDFDOC_ExitFormFillEnvironment"),
            after_load_page: sym!("FORM_OnAfterLoadPage"),
            before_close_page: sym!("FORM_OnBeforeClosePage"),
            get_annot: sym!("FPDFPage_GetAnnot"),
            close_annot: sym!("FPDFPage_CloseAnnot"),
            get_rect: sym!("FPDFAnnot_GetRect"),
            set_focused: sym!("FORM_SetFocusedAnnot"),
            kill_focus: sym!("FORM_ForceToKillFocus"),
            select_all: sym!("FORM_SelectAllText"),
            replace_selection: sym!("FORM_ReplaceSelection"),
            mouse_down: sym!("FORM_OnLButtonDown"),
            mouse_up: sym!("FORM_OnLButtonUp"),
            set_index_selected: sym!("FORM_SetIndexSelected"),
            _library: library,
        })
    }
}

/// Destination for `FPDF_SaveAsCopy`: pdfium calls `WriteBlock` with each chunk of the file.
#[repr(C)]
struct Writer {
    base: FPDF_FILEWRITE,
    out: Vec<u8>,
}

unsafe extern "C" fn write_block(this: *mut FPDF_FILEWRITE, data: *const c_void, size: std::ffi::c_ulong) -> c_int {
    // `base` is the first field of `Writer` (repr C), so the pointer is the same.
    let writer = unsafe { &mut *(this as *mut Writer) };
    let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, size as usize) };
    writer.out.extend_from_slice(bytes);
    1
}

/// `FPDF_SaveAsCopy` flag that saves without the password protection.
const REMOVE_SECURITY: FPDF_DWORD = 3;

/// Opens a document from memory (with its password, if any). It must be closed afterwards.
unsafe fn open_mem(ffi: &Ffi, bytes: &[u8], password: Option<&str>) -> Result<FPDF_DOCUMENT, String> {
    let password = password.map(|p| std::ffi::CString::new(p).map_err(err)).transpose()?;
    let doc = unsafe {
        (ffi.load_mem)(
            bytes.as_ptr().cast(),
            bytes.len(),
            password.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
        )
    };
    if doc.is_null() {
        Err("no se pudo abrir el documento".into())
    } else {
        Ok(doc)
    }
}

unsafe fn save_copy(ffi: &Ffi, doc: FPDF_DOCUMENT, flags: FPDF_DWORD, capacity: usize) -> Result<Vec<u8>, String> {
    let mut writer = Writer {
        base: FPDF_FILEWRITE { version: 1, WriteBlock: Some(write_block) },
        out: Vec::with_capacity(capacity),
    };
    if unsafe { (ffi.save_as_copy)(doc, &mut writer.base, flags) } == 0 {
        Err("no se pudo guardar el documento".to_string())
    } else {
        Ok(writer.out)
    }
}

/// Copy of the document without a password (for tools that read the PDF on their own).
/// Must be called with the pdfium lock held.
pub(crate) fn decrypted(bytes: &[u8], password: &str) -> Result<Vec<u8>, String> {
    let ffi = Ffi::get()?;
    unsafe {
        let doc = open_mem(ffi, bytes, Some(password))?;
        let out = save_copy(ffi, doc, REMOVE_SECURITY, bytes.len());
        (ffi.close_document)(doc);
        out
    }
}

/// Applies an edit to a field and returns the document saved again.
/// Must be called with the pdfium lock held.
fn fill(
    bytes: &[u8],
    password: Option<&str>,
    page_index: PdfPageIndex,
    annotation: PdfPageAnnotationIndex,
    edit: &FieldEdit,
) -> Result<Vec<u8>, String> {
    let ffi = Ffi::get()?;
    unsafe {
        let doc = open_mem(ffi, bytes, password)?;
        let mut info: FPDF_FORMFILLINFO = std::mem::zeroed();
        info.version = 1;
        let form = (ffi.init_form)(doc, &mut info);
        let page = (ffi.load_page)(doc, page_index as c_int);
        let result = if form.is_null() || page.is_null() {
            Err("el documento no tiene formulario".to_string())
        } else {
            (ffi.after_load_page)(page, form);
            let annot = (ffi.get_annot)(page, annotation as c_int);
            let applied = if annot.is_null() {
                Err("no se encontró el campo".to_string())
            } else {
                let applied = apply(ffi, form, page, annot, edit);
                (ffi.close_annot)(annot);
                applied
            };
            (ffi.kill_focus)(form);
            (ffi.before_close_page)(page, form);
            applied
        };
        if !page.is_null() {
            (ffi.close_page)(page);
        }
        let saved = result.and_then(|_| save_copy(ffi, doc, 0, bytes.len() + 4096));
        if !form.is_null() {
            (ffi.exit_form)(form);
        }
        (ffi.close_document)(doc);
        saved
    }
}

unsafe fn apply(ffi: &Ffi, form: FPDF_FORMHANDLE, page: FPDF_PAGE, annot: FPDF_ANNOTATION, edit: &FieldEdit) -> Result<(), String> {
    unsafe {
        match edit {
            FieldEdit::Text { value } => {
                if (ffi.set_focused)(form, annot) == 0 {
                    return Err("no se pudo escribir en el campo".into());
                }
                (ffi.select_all)(form, page);
                let wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
                (ffi.replace_selection)(form, page, wide.as_ptr());
            }
            FieldEdit::Toggle => {
                let mut rect: FS_RECTF = std::mem::zeroed();
                if (ffi.get_rect)(annot, &mut rect) == 0 {
                    return Err("el campo no tiene posición".into());
                }
                let x = ((rect.left + rect.right) / 2.0) as f64;
                let y = ((rect.top + rect.bottom) / 2.0) as f64;
                (ffi.mouse_down)(form, page, 0, x, y);
                (ffi.mouse_up)(form, page, 0, x, y);
            }
            FieldEdit::Choice { index } => {
                if (ffi.set_focused)(form, annot) == 0 {
                    return Err("no se pudo elegir la opción".into());
                }
                if (ffi.set_index_selected)(form, page, *index as c_int, 1) == 0 {
                    return Err("opción inválida".into());
                }
            }
        }
    }
    Ok(())
}

// ---------- Reading fields ----------

fn options_of(options: &PdfFormFieldOptions) -> (Vec<String>, Option<usize>) {
    let mut labels = Vec::new();
    let mut selected = None;
    for i in 0..options.len() {
        if let Ok(option) = options.get(i) {
            if option.is_set() && selected.is_none() {
                selected = Some(labels.len());
            }
            labels.push(option.label().cloned().unwrap_or_default());
        }
    }
    (labels, selected)
}

fn field_info(page: &PdfPage, annotation: PdfPageAnnotationIndex, field: &PdfFormField, rect: PdfRect) -> Result<FormField, String> {
    let mut info = FormField {
        annotation,
        kind: FieldKind::Unknown,
        name: field.name().unwrap_or_default(),
        value: String::new(),
        checked: false,
        options: Vec::new(),
        selected: None,
        multiline: false,
        read_only: field.is_read_only(),
        rect: rect_to_rel(page, rect)?,
    };
    match field.field_type() {
        PdfFormFieldType::Text => {
            let text = field.as_text_field().ok_or("campo inválido")?;
            info.kind = FieldKind::Text;
            info.value = text.value().unwrap_or_default();
            info.multiline = text.is_multiline();
        }
        PdfFormFieldType::Checkbox => {
            info.kind = FieldKind::Checkbox;
            info.checked = field.as_checkbox_field().and_then(|c| c.is_checked().ok()).unwrap_or(false);
        }
        PdfFormFieldType::RadioButton => {
            info.kind = FieldKind::Radio;
            info.checked = field.as_radio_button_field().and_then(|r| r.is_checked().ok()).unwrap_or(false);
        }
        PdfFormFieldType::ComboBox => {
            let combo = field.as_combo_box_field().ok_or("campo inválido")?;
            info.kind = FieldKind::Combo;
            info.value = combo.value().unwrap_or_default();
            (info.options, info.selected) = options_of(combo.options());
        }
        PdfFormFieldType::ListBox => {
            let list = field.as_list_box_field().ok_or("campo inválido")?;
            info.kind = FieldKind::List;
            info.value = list.value().unwrap_or_default();
            (info.options, info.selected) = options_of(list.options());
        }
        PdfFormFieldType::PushButton => info.kind = FieldKind::Button,
        PdfFormFieldType::Signature => info.kind = FieldKind::Signature,
        PdfFormFieldType::Unknown => {}
    }
    // If the dropdown doesn't mark the option, look it up by value.
    if info.selected.is_none() && !info.value.is_empty() {
        info.selected = info.options.iter().position(|o| *o == info.value);
    }
    Ok(info)
}

impl AppState {
    pub fn form_fields(&self, index: PdfPageIndex) -> Result<Vec<FormField>, String> {
        self.with_doc(|open| {
            let page = get_page(open, index)?;
            let annotations = page.annotations();
            let mut fields = Vec::new();
            for i in 0..annotations.len() {
                let annotation = annotations.get(i).map_err(err)?;
                if let Some(field) = annotation.as_form_field() {
                    let rect = annotation.bounds().map_err(err)?;
                    fields.push(field_info(&page, i, field, rect)?);
                }
            }
            Ok(fields)
        })
    }

    pub fn set_form_field(
        &self,
        index: PdfPageIndex,
        annotation: PdfPageAnnotationIndex,
        edit: FieldEdit,
    ) -> Result<DocInfo, String> {
        self.edit(|open| {
            let bytes = open.doc.save_to_bytes().map_err(err)?;
            let filled = fill(&bytes, open.password.as_deref(), index, annotation, &edit)?;
            open.reload(filled)
        })
    }
}

#[tauri::command]
pub async fn form_fields(index: PdfPageIndex, state: State<'_, AppState>) -> Result<Vec<FormField>, String> {
    state.form_fields(index)
}

#[tauri::command]
pub async fn set_form_field(
    index: PdfPageIndex,
    annotation: PdfPageAnnotationIndex,
    edit: FieldEdit,
    state: State<'_, AppState>,
) -> Result<DocInfo, String> {
    state.set_form_field(index, annotation, edit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::tests::state_with_pages;

    fn open_form() -> AppState {
        let state = state_with_pages(1);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/test-data/form.pdf");
        state.open_pdf(path.into(), None).unwrap();
        state
    }

    fn field<'a>(fields: &'a [FormField], name: &str) -> &'a FormField {
        fields.iter().find(|f| f.name == name).unwrap_or_else(|| panic!("falta {name}: {fields:?}"))
    }

    /// Page text extracted by reopening the saved PDF (includes what the fields show).
    fn saved_and_reloaded(state: &AppState) -> AppState {
        let bytes = state.with_doc(|o| o.doc.save_to_bytes().map_err(err)).unwrap();
        let path = std::env::temp_dir().join(format!("pliego-form-{}.pdf", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let reloaded = state_with_pages(1);
        reloaded.open_pdf(path.to_string_lossy().into(), None).unwrap();
        reloaded
    }

    #[test]
    fn lists_fields() {
        let state = open_form();
        let fields = state.form_fields(0).unwrap();
        assert_eq!(field(&fields, "nombre").kind, FieldKind::Text);
        assert_eq!(field(&fields, "acepto").kind, FieldKind::Checkbox);
        assert_eq!(fields.iter().filter(|f| f.kind == FieldKind::Radio).count(), 2);
        let pais = field(&fields, "pais");
        assert_eq!(pais.kind, FieldKind::Combo);
        assert_eq!(pais.options, ["Argentina", "Chile", "México", "Perú"]);
    }

    #[test]
    fn fills_text_checkbox_radio_and_combo() {
        let state = open_form();
        let fields = state.form_fields(0).unwrap();
        let id = |name: &str| field(&fields, name).annotation;
        let radios: Vec<_> = fields.iter().filter(|f| f.kind == FieldKind::Radio).map(|f| f.annotation).collect();

        state.set_form_field(0, id("nombre"), FieldEdit::Text { value: "José Ñúñez".into() }).unwrap();
        state.set_form_field(0, id("acepto"), FieldEdit::Toggle).unwrap();
        state.set_form_field(0, radios[1], FieldEdit::Toggle).unwrap();
        state.set_form_field(0, id("pais"), FieldEdit::Choice { index: 2 }).unwrap();

        // Kept after saving and reopening.
        let reloaded = saved_and_reloaded(&state);
        let fields = reloaded.form_fields(0).unwrap();
        assert_eq!(field(&fields, "nombre").value, "José Ñúñez");
        assert!(field(&fields, "acepto").checked);
        let checked: Vec<bool> = fields.iter().filter(|f| f.kind == FieldKind::Radio).map(|f| f.checked).collect();
        assert_eq!(checked, [false, true]);
        assert_eq!(field(&fields, "pais").value, "México");

        // The typed text is drawn in the field's appearance (other readers see it).
        let nombre = field(&fields, "nombre").rect;
        let dark = reloaded
            .with_doc(|o| {
                let image = crate::pdf::render(&get_page(o, 0)?, 400)?;
                let (w, h) = (image.width() as f32, image.height() as f32);
                let mut dark = 0;
                for y in (nombre.from.y * h) as u32..(nombre.to.y * h) as u32 {
                    for x in (nombre.from.x * w) as u32..(nombre.to.x * w) as u32 {
                        if image.get_pixel(x, y).0[0] < 100 {
                            dark += 1;
                        }
                    }
                }
                Ok(dark)
            })
            .unwrap();
        assert!(dark > 30, "el campo se ve vacío ({dark} píxeles oscuros)");
        if let Ok(dir) = std::env::var("FORM_OUT") {
            let bytes = reloaded.with_doc(|o| o.doc.save_to_bytes().map_err(err)).unwrap();
            std::fs::write(format!("{dir}/form-filled.pdf"), bytes).unwrap();
        }

        // Uncheck the checkbox and undo.
        state.set_form_field(0, id("acepto"), FieldEdit::Toggle).unwrap();
        assert!(!field(&state.form_fields(0).unwrap(), "acepto").checked);
        state.undo().unwrap();
        assert!(field(&state.form_fields(0).unwrap(), "acepto").checked);
    }
}
