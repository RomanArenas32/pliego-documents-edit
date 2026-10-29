/**
 * "Tools" menu: print, extract and split pages, export them as images,
 * reduce the size, protect with a password, create a PDF from images and remove watermarks.
 * All of them except print and watermark removal write new files and don't touch the open
 * document; watermark removal is an edit (it can be undone).
 */

import { invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";

export interface ToolsContext {
  doc(): { path: string; pages: { width: number; height: number }[] } | null;
  current(): number;
  /** Opens a PDF (asks first if there are unsaved changes). */
  openPdf(path: string): Promise<void>;
  renderPng(index: number, width: number): Promise<string>;
  /** Runs an edit command on the open document (undoable); null if it failed. */
  edit(command: string, args: Record<string, unknown>): Promise<unknown | null>;
  info(message: string): void;
  error(e: unknown): void;
}

const PDF_FILTER = [{ name: "PDF", extensions: ["pdf"] }];
const IMAGE_FILTER = [{ name: "Imágenes", extensions: ["png", "jpg", "jpeg", "webp", "gif", "bmp"] }];
/** Resolution of the pages when printing. */
const PRINT_DPI = 200;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

let ctx: ToolsContext;

// ---------- Page ranges ----------

/** "1-3, 5" → [0, 1, 2, 4]. Throws a readable error if the text isn't valid. */
export function parsePages(text: string, count: number): number[] {
  const pages: number[] = [];
  for (const part of text.split(/[,;\s]+/).filter(Boolean)) {
    const m = part.match(/^(\d+)(?:-(\d+))?$/);
    if (!m) throw new Error(`"${part}" no es una página ni un rango (ejemplo: 1-3, 5)`);
    const from = Number(m[1]);
    const to = m[2] ? Number(m[2]) : from;
    if (from < 1 || to < 1 || from > count || to > count) {
      throw new Error(`El documento tiene ${count} página${count === 1 ? "" : "s"}.`);
    }
    const step = from <= to ? 1 : -1;
    for (let p = from; p !== to + step; p += step) pages.push(p - 1);
  }
  if (!pages.length) throw new Error("Escribe qué páginas (ejemplo: 1-3, 5).");
  return pages;
}

function baseName(path: string) {
  return (path.split(/[\\/]/).pop() ?? path).replace(/\.pdf$/i, "");
}

function folderOf(path: string) {
  const i = Math.max(path.lastIndexOf("/"), path.lastIndexOf("\\"));
  return i > 0 ? path.slice(0, i) : undefined;
}

function sizeText(bytes: number) {
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

// ---------- Options dialog ----------

interface Field {
  name: string;
  label: string;
  type: "text" | "number" | "password" | "select";
  value?: string;
  options?: [string, string][];
  hint?: string;
}

/** Shows the dialog with the fields and returns the values, or null if cancelled. */
function ask(title: string, fields: Field[], okLabel: string, validate: (v: Record<string, string>) => void) {
  const dialog = $<HTMLDialogElement>("tool-dialog");
  $("tool-title").textContent = title;
  const ok = $<HTMLButtonElement>("tool-ok");
  ok.textContent = okLabel;
  const error = $("tool-error");
  error.hidden = true;
  const container = $("tool-fields");
  const inputs = new Map<string, HTMLInputElement | HTMLSelectElement>();
  container.replaceChildren(
    ...fields.map((f) => {
      const label = document.createElement("label");
      label.className = "tool-field";
      const span = document.createElement("span");
      span.textContent = f.label;
      let input: HTMLInputElement | HTMLSelectElement;
      if (f.type === "select") {
        input = document.createElement("select");
        for (const [value, text] of f.options!) input.append(new Option(text, value, false, value === f.value));
      } else {
        input = document.createElement("input");
        input.type = f.type === "number" ? "text" : f.type;
        if (f.type === "number") input.inputMode = "numeric";
        input.value = f.value ?? "";
        input.spellcheck = false;
        input.autocomplete = "off";
      }
      inputs.set(f.name, input);
      label.append(span, input);
      if (f.hint) {
        const hint = document.createElement("small");
        hint.textContent = f.hint;
        label.append(hint);
      }
      return label;
    }),
  );

  return new Promise<Record<string, string> | null>((resolve) => {
    const values = () => Object.fromEntries([...inputs].map(([k, el]) => [k, el.value]));
    const submit = (e: Event) => {
      e.preventDefault();
      try {
        const v = values();
        validate(v);
        finish(v);
      } catch (err) {
        error.textContent = err instanceof Error ? err.message : String(err);
        error.hidden = false;
      }
    };
    const finish = (result: Record<string, string> | null) => {
      ok.removeEventListener("click", submit);
      dialog.removeEventListener("close", onClose);
      dialog.removeEventListener("keydown", onKey);
      if (dialog.open) dialog.close();
      resolve(result);
    };
    const onClose = () => finish(null);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Enter" && (e.target as HTMLElement).tagName === "INPUT") submit(e);
    };
    ok.addEventListener("click", submit);
    dialog.addEventListener("close", onClose);
    dialog.addEventListener("keydown", onKey);
    dialog.showModal();
    const first = inputs.values().next().value;
    if (first instanceof HTMLInputElement) first.select();
  });
}

/** Asks for the password of a protected PDF; null if cancelled. */
export async function askPassword(file: string, wrong: boolean) {
  const v = await ask(
    "PDF protegido",
    [
      {
        name: "password",
        label: wrong ? "Contraseña incorrecta, prueba de nuevo" : `Contraseña de ${file}`,
        type: "password",
      },
    ],
    "Abrir",
    (v) => {
      if (!v.password) throw new Error("Escribe la contraseña.");
    },
  );
  return v?.password ?? null;
}

// ---------- Tools ----------

/** Runs the task showing the notice; returns { value } or null if it failed. */
async function run<T>(working: string, task: () => Promise<T>): Promise<{ value: T } | null> {
  document.body.classList.add("working");
  ctx.info(working);
  try {
    return { value: await task() };
  } catch (e) {
    ctx.error(e);
    return null;
  } finally {
    document.body.classList.remove("working");
  }
}

async function extractPages() {
  const doc = ctx.doc()!;
  const count = doc.pages.length;
  const v = await ask(
    "Extraer páginas",
    [{ name: "pages", label: "Páginas", type: "text", value: String(ctx.current() + 1), hint: "Por ejemplo: 1-3, 5, 8" }],
    "Extraer…",
    (v) => parsePages(v.pages, count),
  );
  if (!v) return;
  const pages = parsePages(v.pages, count);
  const path = await save({
    defaultPath: `${folderOf(doc.path) ?? ""}/${baseName(doc.path)} (pág. ${v.pages.replace(/\s+/g, "")}).pdf`,
    filters: PDF_FILTER,
  });
  if (!path) return;
  if (await run("Extrayendo páginas…", () => invoke("extract_pages", { pages, path }))) {
    ctx.info(`Listo: ${pages.length} página${pages.length === 1 ? "" : "s"} en ${baseName(path)}.pdf`);
  }
}

async function splitPdf() {
  const doc = ctx.doc()!;
  const count = doc.pages.length;
  const v = await ask(
    "Dividir PDF",
    [{ name: "every", label: "Páginas por archivo", type: "number", value: "1", hint: `El documento tiene ${count} páginas.` }],
    "Elegir carpeta…",
    (v) => {
      const n = Number(v.every);
      if (!Number.isInteger(n) || n < 1) throw new Error("Escribe un número de páginas (1 o más).");
    },
  );
  if (!v) return;
  const dir = await open({ directory: true, defaultPath: folderOf(doc.path) });
  if (!dir || Array.isArray(dir)) return;
  const r = await run("Dividiendo…", () => invoke<string[]>("split_pdf", { every: Number(v.every), dir }));
  if (r) ctx.info(`Listo: ${r.value.length} archivo${r.value.length === 1 ? "" : "s"} en la carpeta elegida.`);
}

async function exportImages() {
  const doc = ctx.doc()!;
  const count = doc.pages.length;
  const v = await ask(
    "Exportar como imágenes",
    [
      { name: "pages", label: "Páginas", type: "text", value: count === 1 ? "1" : `1-${count}` },
      { name: "format", label: "Formato", type: "select", value: "png", options: [["png", "PNG (mejor calidad)"], ["jpeg", "JPG (más liviano)"]] },
      { name: "dpi", label: "Resolución", type: "select", value: "150", options: [["72", "Pantalla (72 ppp)"], ["150", "Normal (150 ppp)"], ["300", "Impresión (300 ppp)"]] },
    ],
    "Elegir carpeta…",
    (v) => parsePages(v.pages, count),
  );
  if (!v) return;
  const pages = parsePages(v.pages, count);
  const dir = await open({ directory: true, defaultPath: folderOf(doc.path) });
  if (!dir || Array.isArray(dir)) return;
  const r = await run(`Exportando ${pages.length} página${pages.length === 1 ? "" : "s"}…`, () =>
    invoke<string[]>("export_images", { pages, dir, format: v.format, dpi: Number(v.dpi) }),
  );
  if (r) {
    const n = r.value.length;
    ctx.info(`Listo: ${n} imagen${n === 1 ? "" : "es"} guardada${n === 1 ? "" : "s"}.`);
  }
}

async function compressPdf() {
  const doc = ctx.doc()!;
  const v = await ask(
    "Reducir tamaño",
    [
      {
        name: "level",
        label: "Compresión",
        type: "select",
        value: "medium",
        options: [["low", "Poca (casi sin pérdida)"], ["medium", "Media (recomendada)"], ["high", "Mucha (archivo más chico)"]],
        hint: "Se achican y comprimen las imágenes. El texto no pierde calidad.",
      },
    ],
    "Guardar como…",
    () => {},
  );
  if (!v) return;
  const path = await save({ defaultPath: `${folderOf(doc.path) ?? ""}/${baseName(doc.path)} (reducido).pdf`, filters: PDF_FILTER });
  if (!path) return;
  const done = await run("Reduciendo…", () =>
    invoke<{ before: number; after: number; images: number }>("compress_pdf", { path, level: v.level }),
  );
  if (!done) return;
  const r = done.value;
  const saved = Math.round((1 - r.after / r.before) * 100);
  ctx.info(
    saved > 0
      ? `Listo: ${sizeText(r.before)} → ${sizeText(r.after)} (${saved}% menos).`
      : "Este PDF ya está bien comprimido: se guardó una copia sin cambios.",
  );
}

async function protectPdf() {
  const doc = ctx.doc()!;
  const v = await ask(
    "Proteger con contraseña",
    [
      { name: "password", label: "Contraseña", type: "password", hint: "Se pedirá para abrir el PDF. Si la olvidas no hay forma de recuperarlo." },
      { name: "repeat", label: "Repetir contraseña", type: "password" },
    ],
    "Guardar como…",
    (v) => {
      if (!v.password) throw new Error("Escribe una contraseña.");
      if (v.password !== v.repeat) throw new Error("Las contraseñas no coinciden.");
    },
  );
  if (!v) return;
  const path = await save({ defaultPath: `${folderOf(doc.path) ?? ""}/${baseName(doc.path)} (protegido).pdf`, filters: PDF_FILTER });
  if (!path) return;
  if (await run("Protegiendo…", () => invoke("protect_pdf", { path, password: v.password }))) {
    ctx.info(`Listo: ${baseName(path)}.pdf pide contraseña para abrirse.`);
  }
}

async function exportWord() {
  const doc = ctx.doc()!;
  const path = await save({
    defaultPath: `${folderOf(doc.path) ?? ""}/${baseName(doc.path)}.docx`,
    filters: [{ name: "Word", extensions: ["docx"] }],
  });
  if (!path) return;
  const r = await run("Exportando a Word…", () => invoke<number>("export_word", { path }));
  if (!r) return;
  const empty = r.value;
  ctx.info(
    empty === doc.pages.length
      ? "Este PDF no tiene texto (parece escaneado): el documento de Word quedó vacío."
      : empty
        ? `Listo. ${empty} página${empty === 1 ? "" : "s"} sin texto (escaneada${empty === 1 ? "" : "s"}) quedaron vacías.`
        : "Listo: se exportó el texto con sus párrafos. Las imágenes y tablas no se copian.",
  );
}

// ---------- Watermarks ----------

interface WatermarkCandidate {
  key: string;
  label: string;
  pages: number[];
  likely: boolean;
}

/** Width of the preview render, in pixels. */
const WATERMARK_PREVIEW_WIDTH = 600;

async function removeWatermarks() {
  const found = await run("Buscando marcas de agua…", () => invoke<WatermarkCandidate[]>("find_watermarks"));
  if (!found) return;
  const candidates = found.value;
  if (candidates.length === 0) {
    ctx.info(
      "No se encontraron marcas de agua. Si la marca es parte de una página escaneada (una foto), no se puede quitar.",
    );
    return;
  }

  const dialog = $<HTMLDialogElement>("wm-dialog");
  const list = $("wm-list");
  const img = $<HTMLImageElement>("wm-img");
  const pageLabel = $("wm-page");
  const ok = $<HTMLButtonElement>("wm-ok");
  const viewButtons = [...dialog.querySelectorAll<HTMLButtonElement>(".wm-view button")];
  const checked = new Set(candidates.filter((c) => c.likely).map((c) => c.key));
  let page = candidates[0].pages[0];
  let view: "after" | "before" = "after";
  /** Discards previews that arrive after a newer one. */
  let generation = 0;

  const pagesText = (pages: number[]) =>
    pages.length === 1 ? `en la página ${pages[0] + 1}` : `en ${pages.length} páginas`;

  const refresh = async () => {
    const mine = ++generation;
    ok.disabled = checked.size === 0;
    ok.textContent = checked.size > 1 ? `Quitar (${checked.size})` : "Quitar";
    for (const b of viewButtons) b.setAttribute("aria-pressed", String(b.dataset.view === view));
    for (const li of list.querySelectorAll<HTMLElement>("li")) {
      li.classList.toggle("shown", candidates[Number(li.dataset.index)].pages.includes(page));
    }
    pageLabel.textContent = `Página ${page + 1}`;
    try {
      const url =
        view === "before"
          ? await ctx.renderPng(page, WATERMARK_PREVIEW_WIDTH)
          : URL.createObjectURL(
              new Blob(
                [
                  await invoke<ArrayBuffer>("preview_watermarks", {
                    keys: [...checked],
                    index: page,
                    width: WATERMARK_PREVIEW_WIDTH,
                  }),
                ],
                { type: "image/png" },
              ),
            );
      if (mine !== generation) return URL.revokeObjectURL(url);
      if (img.src.startsWith("blob:")) URL.revokeObjectURL(img.src);
      img.src = url;
    } catch (e) {
      ctx.error(e);
    }
  };

  list.replaceChildren(
    ...candidates.map((c, i) => {
      const li = document.createElement("li");
      li.dataset.index = String(i);
      const box = document.createElement("input");
      box.type = "checkbox";
      box.checked = checked.has(c.key);
      const text = document.createElement("div");
      const label = document.createElement("span");
      label.className = "wm-label";
      label.textContent = c.label;
      const detail = document.createElement("small");
      detail.textContent = `${pagesText(c.pages)}${c.likely ? "" : " · se repite (puede ser un encabezado o un logo)"}`;
      text.append(label, detail);
      li.append(box, text);
      box.addEventListener("change", () => {
        if (box.checked) checked.add(c.key);
        else checked.delete(c.key);
        page = c.pages[0];
        refresh();
      });
      // Clicking the row (not the box) only shows where it is.
      li.addEventListener("click", (e) => {
        if (e.target === box) return;
        page = c.pages[0];
        refresh();
      });
      return li;
    }),
  );
  for (const b of viewButtons) {
    b.onclick = () => {
      view = b.dataset.view as "after" | "before";
      refresh();
    };
  }

  const keys = await new Promise<string[] | null>((resolve) => {
    const finish = (result: string[] | null) => {
      ok.removeEventListener("click", apply);
      dialog.removeEventListener("close", cancel);
      if (dialog.open) dialog.close();
      resolve(result);
    };
    const apply = () => finish([...checked]);
    const cancel = () => finish(null);
    ok.addEventListener("click", apply);
    dialog.addEventListener("close", cancel);
    dialog.showModal();
    refresh();
  });
  generation++;
  if (img.src.startsWith("blob:")) URL.revokeObjectURL(img.src);
  img.removeAttribute("src");
  if (!keys?.length) return;
  if (await ctx.edit("remove_watermarks", { keys })) {
    ctx.info(keys.length === 1 ? "Listo: se quitó la marca de agua." : `Listo: se quitaron ${keys.length} marcas de agua.`);
  }
}

async function imagesToPdf() {
  const picked = await open({ multiple: true, filters: IMAGE_FILTER });
  const paths = Array.isArray(picked) ? picked : picked ? [picked] : [];
  if (!paths.length) return;
  const output = await save({ defaultPath: `${folderOf(paths[0]) ?? ""}/Imágenes.pdf`, filters: PDF_FILTER });
  if (!output) return;
  const n = paths.length;
  if (await run(`Creando PDF con ${n} imagen${n === 1 ? "" : "es"}…`, () => invoke("images_to_pdf", { paths, output }))) {
    await ctx.openPdf(output);
  }
}

// ---------- Print ----------

let printArea: HTMLElement | null = null;

function clearPrint() {
  printArea?.querySelectorAll("img").forEach((img) => URL.revokeObjectURL(img.src));
  printArea?.remove();
  printArea = null;
}

async function printDocument() {
  const doc = ctx.doc();
  if (!doc) return;
  clearPrint();
  const area = document.createElement("div");
  area.id = "print-area";
  printArea = area;
  const ok = await run("Preparando la impresión…", async () => {
    for (let i = 0; i < doc.pages.length; i++) {
      const { width, height } = doc.pages[i];
      const img = new Image();
      img.className = width > height ? "landscape" : "portrait";
      img.src = await ctx.renderPng(i, (width / 72) * PRINT_DPI);
      area.append(img);
      if (printArea !== area) return false;
    }
    document.body.append(area);
    await Promise.all([...area.querySelectorAll("img")].map((img) => img.decode().catch(() => {})));
    return true;
  });
  if (!ok?.value) return;
  try {
    await invoke("print_window");
  } catch {
    window.print();
  }
}

// ---------- Menu ----------

const ITEMS: { label: string; run: () => unknown; needsDoc: boolean; shortcut?: string }[] = [
  { label: "Imprimir…", run: printDocument, needsDoc: true, shortcut: "⌘P" },
  { label: "Extraer páginas…", run: extractPages, needsDoc: true },
  { label: "Dividir PDF…", run: splitPdf, needsDoc: true },
  { label: "Exportar como imágenes…", run: exportImages, needsDoc: true },
  { label: "Exportar a Word…", run: exportWord, needsDoc: true },
  { label: "Reducir tamaño…", run: compressPdf, needsDoc: true },
  { label: "Proteger con contraseña…", run: protectPdf, needsDoc: true },
  { label: "Quitar marca de agua…", run: removeWatermarks, needsDoc: true },
  { label: "Crear PDF desde imágenes…", run: imagesToPdf, needsDoc: false },
];

export function initTools(context: ToolsContext) {
  ctx = context;
  const button = $<HTMLButtonElement>("btn-tools");
  const menu = $("tools-menu");
  const close = () => {
    menu.hidden = true;
    button.setAttribute("aria-expanded", "false");
  };
  menu.replaceChildren(
    ...ITEMS.map((item, i) => {
      const b = document.createElement("button");
      b.type = "button";
      b.dataset.item = String(i);
      b.innerHTML = `<span></span><kbd></kbd>`;
      b.querySelector("span")!.textContent = item.label;
      b.querySelector("kbd")!.textContent = item.shortcut ?? "";
      if (i === ITEMS.length - 1) b.classList.add("separated");
      b.addEventListener("click", () => {
        close();
        item.run();
      });
      return b;
    }),
  );
  button.addEventListener("click", (e) => {
    e.stopPropagation();
    if (!menu.hidden) return close();
    const has = ctx.doc() !== null;
    menu.querySelectorAll<HTMLButtonElement>("button").forEach((b) => {
      b.disabled = ITEMS[Number(b.dataset.item)].needsDoc && !has;
    });
    const r = button.getBoundingClientRect();
    menu.style.left = `${r.left}px`;
    menu.style.top = `${r.bottom + 4}px`;
    menu.hidden = false;
    button.setAttribute("aria-expanded", "true");
  });
  document.addEventListener("pointerdown", (e) => {
    if (!menu.hidden && !menu.contains(e.target as Node) && e.target !== button) close();
  });
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !menu.hidden) close();
  });
  window.addEventListener("afterprint", clearPrint);
}

export { printDocument };
