/**
 * "Escanear documento": photos of paper pages become a straight, clean PDF. The person marks
 * each sheet's corners on its photo, chooses how it should look (scan.rs straightens and
 * cleans it up) and saves all the pages as one PDF. On the phone the photos come from the
 * camera; on the computer, from image files.
 */

import { invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { withProgress } from "./progress";

export interface ScanContext {
  /** Phone: photos come from the camera (or the gallery). */
  mobile: boolean;
  openPdf(path: string): Promise<void>;
  error(e: unknown): void;
}

type Mode = "document" | "gray" | "color";

interface Point {
  x: number;
  y: number;
}

interface Page {
  id: number;
  /** The sheet's corners on the photo (0 to 1): top-left, top-right, bottom-right, bottom-left. */
  corners: Point[];
  mode: Mode;
  /** Clockwise, in degrees. */
  rotation: number;
  thumb: string;
  /** The photo at editing size, loaded the first time the page is shown. */
  photo?: string;
}

const IMAGE_FILTER = [{ name: "Imágenes", extensions: ["png", "jpg", "jpeg", "webp", "gif", "bmp"] }];
const PDF_FILTER = [{ name: "PDF", extensions: ["pdf"] }];
/** Width of the photo shown to mark the corners (sharp on a phone screen). */
const PHOTO_WIDTH = 1200;
const THUMB_WIDTH = 160;
/** Longest side of the "Resultado" preview. */
const RESULT_SIZE = 1400;
/** Where the corners start: a little inside the photo, so all four can be grabbed. */
const INSET = 0.04;

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

let ctx: ScanContext;
let pages: Page[] = [];
let current = 0;
let view: "edges" | "result" = "edges";
/** New pages take the look of the last one chosen. */
let lastMode: Mode = "document";
/** Discards images that arrive after a newer one was asked for. */
let generation = 0;
let handles: HTMLElement[] = [];

const jpegUrl = (buffer: ArrayBuffer) => URL.createObjectURL(new Blob([buffer], { type: "image/jpeg" }));

const defaultCorners = (): Point[] => [
  { x: INSET, y: INSET },
  { x: 1 - INSET, y: INSET },
  { x: 1 - INSET, y: 1 - INSET },
  { x: INSET, y: 1 - INSET },
];

/** Starts a scan: opens the camera (or the image picker) and then the scan window. */
export async function scanDocument(context: ScanContext) {
  ctx = context;
  const dialog = $<HTMLDialogElement>("scan-dialog");
  if (dialog.open) return;
  reset();
  dialog.addEventListener("close", reset, { once: true });
  dialog.showModal();
  render();
  await addPhotos(ctx.mobile ? "camera" : "files");
  // Nothing taken: there's nothing to scan.
  if (pages.length === 0 && dialog.open) dialog.close();
}

function reset() {
  generation++;
  for (const page of pages) {
    URL.revokeObjectURL(page.thumb);
    if (page.photo) URL.revokeObjectURL(page.photo);
  }
  const img = $<HTMLImageElement>("scan-img");
  if (img.src.startsWith("blob:")) URL.revokeObjectURL(img.src);
  img.removeAttribute("src");
  pages = [];
  current = 0;
  view = "edges";
  invoke("scan_clear").catch(() => {});
}

async function addPhotos(source: "camera" | "files") {
  let photos: { path: string; temporary: boolean }[] = [];
  try {
    if (source === "camera") {
      const path = await invoke<string | null>("take_photo");
      if (path) photos = [{ path, temporary: true }];
    } else {
      const picked = await open({ multiple: true, filters: IMAGE_FILTER });
      const paths = Array.isArray(picked) ? picked : picked ? [picked] : [];
      photos = paths.map((path) => ({ path, temporary: false }));
    }
  } catch (e) {
    ctx.error(e);
    return;
  }
  if (photos.length === 0) return;
  try {
    await withProgress("scan-progress", "Cargando fotos", async () => {
      for (const { path, temporary } of photos) {
        const photo = await invoke<{ id: number }>("scan_add", { path, temporary });
        const thumb = jpegUrl(await invoke<ArrayBuffer>("scan_photo", { id: photo.id, width: THUMB_WIDTH }));
        pages.push({ id: photo.id, corners: defaultCorners(), mode: lastMode, rotation: 0, thumb });
      }
    });
  } catch (e) {
    ctx.error(e);
  }
  current = Math.max(0, pages.length - 1);
  view = "edges";
  render();
}

// ---------- Window ----------

function render() {
  renderStrip();
  const page = pages[current];
  const has = page !== undefined;
  for (const id of ["scan-rotate", "scan-remove", "scan-save"]) $<HTMLButtonElement>(id).disabled = !has;
  for (const b of $("scan-view").querySelectorAll<HTMLButtonElement>("button")) {
    b.setAttribute("aria-pressed", String(b.dataset.view === view));
    b.disabled = !has;
  }
  for (const b of $("scan-mode").querySelectorAll<HTMLButtonElement>("button")) {
    b.setAttribute("aria-pressed", String(page?.mode === b.dataset.mode));
    b.disabled = !has;
  }
  $("scan-hint").textContent = !has
    ? ctx.mobile
      ? "Saca una foto de la hoja con «Cámara»."
      : "Agrega fotos de las hojas con «Agregar fotos…»."
    : view === "edges"
      ? "Arrastra las esquinas hasta los bordes de la hoja."
      : "Así quedará la página. Usa «Bordes» para corregir el recorte.";
  $<HTMLButtonElement>("scan-save").textContent = pages.length > 1 ? `Guardar PDF (${pages.length} págs.)…` : "Guardar PDF…";
  showImage();
}

/** Thumbnails of the pages, and the buttons to add more. */
function renderStrip() {
  const strip = $("scan-strip");
  const items: HTMLElement[] = pages.map((page, i) => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "scan-thumb";
    button.setAttribute("aria-pressed", String(i === current));
    button.title = `Página ${i + 1}`;
    const img = document.createElement("img");
    img.src = page.thumb;
    img.alt = "";
    const number = document.createElement("span");
    number.textContent = String(i + 1);
    button.append(img, number);
    button.addEventListener("click", () => {
      current = i;
      view = "edges";
      render();
    });
    return button;
  });
  const add = (label: string, source: "camera" | "files") => {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "scan-add";
    button.textContent = label;
    button.addEventListener("click", () => addPhotos(source));
    return button;
  };
  if (ctx.mobile) items.push(add("+ Cámara", "camera"), add("+ Galería…", "files"));
  else items.push(add("+ Agregar fotos…", "files"));
  strip.replaceChildren(...items);
  strip.querySelector<HTMLElement>('[aria-pressed="true"]')?.scrollIntoView({ block: "nearest", inline: "nearest" });
}

/** The photo with its corners ("Bordes") or the finished page ("Resultado"). */
async function showImage() {
  const mine = ++generation;
  const img = $<HTMLImageElement>("scan-img");
  const frame = $("scan-frame");
  const page = pages[current];
  frame.classList.toggle("editing", view === "edges" && page !== undefined);
  drawCorners();
  if (!page) {
    if (img.src.startsWith("blob:") && !isPhoto(img.src)) URL.revokeObjectURL(img.src);
    img.removeAttribute("src");
    return;
  }
  frame.classList.add("loading");
  try {
    let url: string;
    if (view === "edges") {
      page.photo ??= jpegUrl(await invoke<ArrayBuffer>("scan_photo", { id: page.id, width: PHOTO_WIDTH }));
      url = page.photo;
    } else {
      const args = { id: page.id, corners: page.corners, mode: page.mode, rotation: page.rotation };
      url = jpegUrl(await invoke<ArrayBuffer>("scan_preview", { page: args, width: RESULT_SIZE }));
    }
    if (mine !== generation) {
      if (!isPhoto(url)) URL.revokeObjectURL(url);
      return;
    }
    // Previews are thrown away when replaced; photos are kept for going back to "Bordes".
    if (img.src.startsWith("blob:") && !isPhoto(img.src)) URL.revokeObjectURL(img.src);
    img.src = url;
  } catch (e) {
    if (mine === generation) ctx.error(e);
  } finally {
    if (mine === generation) frame.classList.remove("loading");
  }
}

const isPhoto = (url: string) => pages.some((p) => p.photo === url);

// ---------- Corners ----------

function drawCorners() {
  const page = pages[current];
  const polygon = $("scan-outline").querySelector("polygon")!;
  const editing = view === "edges" && page !== undefined;
  polygon.setAttribute("points", editing ? page.corners.map((c) => `${c.x * 100},${c.y * 100}`).join(" ") : "");
  handles.forEach((h, i) => {
    h.hidden = !editing;
    if (editing) {
      h.style.left = `${page.corners[i].x * 100}%`;
      h.style.top = `${page.corners[i].y * 100}%`;
    }
  });
}

/** Four handles, one per corner, dragged with the mouse or a finger. */
function createHandles() {
  const frame = $("scan-frame");
  const img = $<HTMLImageElement>("scan-img");
  const names = ["superior izquierda", "superior derecha", "inferior derecha", "inferior izquierda"];
  handles = names.map((name, i) => {
    const handle = document.createElement("div");
    handle.className = "scan-handle";
    handle.title = `Esquina ${name}`;
    handle.addEventListener("pointerdown", (e) => {
      e.preventDefault();
      handle.setPointerCapture(e.pointerId);
      handle.classList.add("dragging");
      const move = (ev: PointerEvent) => {
        const page = pages[current];
        if (!page) return;
        const r = img.getBoundingClientRect();
        page.corners[i] = {
          x: Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width)),
          y: Math.min(1, Math.max(0, (ev.clientY - r.top) / r.height)),
        };
        drawCorners();
      };
      const end = () => {
        handle.classList.remove("dragging");
        handle.removeEventListener("pointermove", move);
        handle.removeEventListener("pointerup", end);
        handle.removeEventListener("pointercancel", end);
      };
      handle.addEventListener("pointermove", move);
      handle.addEventListener("pointerup", end);
      handle.addEventListener("pointercancel", end);
    });
    frame.append(handle);
    return handle;
  });
}

// ---------- Setup ----------

export function initScan() {
  createHandles();
  for (const b of $("scan-view").querySelectorAll<HTMLButtonElement>("button")) {
    b.addEventListener("click", () => {
      view = b.dataset.view as "edges" | "result";
      render();
    });
  }
  for (const b of $("scan-mode").querySelectorAll<HTMLButtonElement>("button")) {
    b.addEventListener("click", () => {
      const page = pages[current];
      if (!page) return;
      page.mode = lastMode = b.dataset.mode as Mode;
      view = "result";
      render();
    });
  }
  $("scan-rotate").addEventListener("click", () => {
    const page = pages[current];
    if (!page) return;
    page.rotation = (page.rotation + 90) % 360;
    view = "result";
    render();
  });
  $("scan-remove").addEventListener("click", () => {
    const [page] = pages.splice(current, 1);
    if (!page) return;
    URL.revokeObjectURL(page.thumb);
    if (page.photo) URL.revokeObjectURL(page.photo);
    current = Math.min(current, pages.length - 1);
    view = "edges";
    render();
  });
  $("scan-save").addEventListener("click", saveScan);
}

async function saveScan() {
  if (pages.length === 0) return;
  const today = new Date().toISOString().slice(0, 10);
  let output: string | null;
  try {
    output = await save({ defaultPath: `Escaneo ${today}.pdf`, filters: PDF_FILTER });
  } catch (e) {
    ctx.error(e);
    return;
  }
  if (!output) return;
  const args = pages.map((p) => ({ id: p.id, corners: p.corners, mode: p.mode, rotation: p.rotation }));
  try {
    await withProgress("scan-progress", "Creando PDF:", () => invoke("scan_save", { pages: args, output }));
  } catch (e) {
    ctx.error(e);
    return;
  }
  $<HTMLDialogElement>("scan-dialog").close();
  await ctx.openPdf(output);
}
