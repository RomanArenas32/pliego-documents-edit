import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { ask, open, save } from "@tauri-apps/plugin-dialog";
import { chooseSignature } from "./signature";
import { askPassword, initTools, printDocument } from "./tools";

interface PageSize {
  width: number;
  height: number;
}

interface DocInfo {
  path: string;
  pages: PageSize[];
  canUndo: boolean;
  canRedo: boolean;
}

/** Point relative to the displayed page: (0, 0) top-left, (1, 1) bottom-right. */
interface RelPoint {
  x: number;
  y: number;
}

interface RelRect {
  from: RelPoint;
  to: RelPoint;
}

/** Text added with the app; `anchor` is the start of the first line's baseline. */
interface TextInfo {
  lines: string[];
  size: number;
  color: string;
  anchor: RelPoint;
}

interface AnnotationHit {
  annotation: number;
  rect: RelRect;
  text: TextInfo | null;
  /** Image or signature added with the app. */
  image: boolean;
  /** Text of the sticky note, if it is a note. */
  note: string | null;
}

type ImageSource = { kind: "file"; path: string } | { kind: "data"; bytes: number[] };

interface FormField {
  annotation: number;
  kind: "text" | "checkbox" | "radio" | "combo" | "list" | "button" | "signature" | "unknown";
  name: string;
  value: string;
  checked: boolean;
  options: string[];
  selected: number | null;
  multiline: boolean;
  readOnly: boolean;
  rect: RelRect;
}

/** Original text fragment of the PDF (may span several objects, e.g. one per letter). */
interface TextObjectHit {
  objects: number[];
  text: string;
  rect: RelRect;
  size: number;
  color: string;
  font: string;
}

interface TextEditResult {
  doc: DocInfo;
  fontReplaced: boolean;
}

type Tool =
  | "select"
  | "text"
  | "edit-text"
  | "highlight"
  | "rect"
  | "pen"
  | "eraser"
  | "note"
  | "redact"
  | "batch"
  /** Place a signature or image with a click. */
  | "place";

const THUMB_WIDTH = 140;
const ZOOM_STEPS = [0.5, 0.75, 1, 1.25, 1.5, 2, 3];
const SVG_NS = "http://www.w3.org/2000/svg";
/** Baseline position within a line of text (Helvetica with line-height 1.2). */
const BASELINE_RATIO = 0.87;
const LINE_HEIGHT = 1.2;
const PDF_FILTER = [{ name: "PDF", extensions: ["pdf"] }];

const state = {
  doc: null as DocInfo | null,
  current: 0,
  zoomIndex: 2,
  dirty: false,
  tool: "select" as Tool,
  colors: { text: "#000000", highlight: "#ffd400", rect: "#e01e1e", pen: "#e01e1e", note: "#ffd400" } as Record<
    string,
    string
  >,
  textSize: 14,
  strokeWidth: 2,
  // Incremented when rebuilding the thumbnails to discard stale renders.
  thumbsVersion: 0,
  busy: false,
};

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const els = {
  open: $<HTMLButtonElement>("btn-open"),
  openEmpty: $<HTMLButtonElement>("btn-open-empty"),
  save: $<HTMLButtonElement>("btn-save"),
  saveAs: $<HTMLButtonElement>("btn-save-as"),
  undo: $<HTMLButtonElement>("btn-undo"),
  redo: $<HTMLButtonElement>("btn-redo"),
  prev: $<HTMLButtonElement>("btn-prev"),
  next: $<HTMLButtonElement>("btn-next"),
  zoomOut: $<HTMLButtonElement>("btn-zoom-out"),
  zoomIn: $<HTMLButtonElement>("btn-zoom-in"),
  rotateLeft: $<HTMLButtonElement>("btn-rotate-left"),
  rotateRight: $<HTMLButtonElement>("btn-rotate-right"),
  blank: $<HTMLButtonElement>("btn-blank"),
  insert: $<HTMLButtonElement>("btn-insert"),
  del: $<HTMLButtonElement>("btn-delete"),
  tools: $("tools"),
  toolOptions: $("tool-options"),
  color: $<HTMLInputElement>("opt-color"),
  size: $<HTMLSelectElement>("opt-size"),
  sizeWrap: $("opt-size-wrap"),
  width: $<HTMLInputElement>("opt-width"),
  widthWrap: $("opt-width-wrap"),
  pageIndicator: $("page-indicator"),
  zoomIndicator: $("zoom-indicator"),
  fileName: $("file-name"),
  thumbnails: $("thumbnails"),
  viewer: $("viewer"),
  empty: $("empty"),
  pages: $("pages"),
  /** Elements of the current page. */
  get pageWrap() {
    return views[state.current].wrap;
  },
  get page() {
    return views[state.current].img;
  },
  get overlay() {
    return views[state.current].overlay;
  },
  toast: $("toast"),
  search: $<HTMLButtonElement>("btn-search"),
  sign: $<HTMLButtonElement>("btn-sign"),
  image: $<HTMLButtonElement>("btn-image"),
  searchBar: $("search-bar"),
  searchInput: $<HTMLInputElement>("search-input"),
  searchCount: $("search-count"),
  searchPrev: $<HTMLButtonElement>("search-prev"),
  searchRedact: $<HTMLButtonElement>("search-redact"),
  searchNext: $<HTMLButtonElement>("search-next"),
  searchClose: $<HTMLButtonElement>("search-close"),
  batch: $<HTMLButtonElement>("btn-batch"),
  batchPanel: $("batch-panel"),
  batchClose: $<HTMLButtonElement>("batch-close"),
  batchFields: $("batch-fields"),
  batchLoad: $<HTMLButtonElement>("batch-load"),
  batchTableInfo: $("batch-table-info"),
  batchPreviewNav: $("batch-preview-nav"),
  batchPrev: $<HTMLButtonElement>("batch-prev"),
  batchNext: $<HTMLButtonElement>("batch-next"),
  batchRowLabel: $("batch-row-label"),
  batchPattern: $<HTMLInputElement>("batch-pattern"),
  batchRun: $<HTMLButtonElement>("batch-run"),
  batchProgress: $("batch-progress"),
  batchProgressBar: $<HTMLProgressElement>("batch-progress-bar"),
  batchProgressLabel: $("batch-progress-label"),
  batchCancel: $<HTMLButtonElement>("batch-cancel"),
  batchResult: $("batch-result"),
};

// ---------- Utilities ----------

async function renderPng(index: number, width: number): Promise<string> {
  const buf = await invoke<ArrayBuffer>("render_page", { index, width: Math.round(width) });
  return URL.createObjectURL(new Blob([buf], { type: "image/png" }));
}

function setImage(img: HTMLImageElement, url: string) {
  if (img.src.startsWith("blob:")) URL.revokeObjectURL(img.src);
  img.src = url;
}

let toastTimer = 0;
function showError(e: unknown, kind: "error" | "info" = "error") {
  els.toast.textContent = String(e);
  els.toast.dataset.kind = kind;
  els.toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => (els.toast.hidden = true), 4000);
}

function showInfo(message: string) {
  showError(message, "info");
}

function baseName(path: string) {
  return path.split(/[\\/]/).pop() ?? path;
}

function isPdf(path: string) {
  return path.toLowerCase().endsWith(".pdf");
}

/** CSS px width the page is displayed at (the sizes already include rotation). */
function displayWidth(index: number) {
  return state.doc!.pages[index].width * ZOOM_STEPS[state.zoomIndex] * (96 / 72);
}

/** CSS pixels per PDF point on the current page. */
function pxPerPt() {
  return ZOOM_STEPS[state.zoomIndex] * (96 / 72);
}

function relPoint(e: PointerEvent | MouseEvent, index = state.current): RelPoint {
  const r = views[index].img.getBoundingClientRect();
  return {
    x: Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)),
    y: Math.min(1, Math.max(0, (e.clientY - r.top) / r.height)),
  };
}

function toPx(p: RelPoint, index = state.current) {
  const r = views[index].img.getBoundingClientRect();
  return { x: p.x * r.width, y: p.y * r.height };
}

/** Inverse of `toPx`: from pixels within the page to a relative point. */
function relFromPx(x: number, y: number, index = state.current): RelPoint {
  const r = views[index].img.getBoundingClientRect();
  return { x: Math.min(1, Math.max(0, x / r.width)), y: Math.min(1, Math.max(0, y / r.height)) };
}

function svg<K extends keyof SVGElementTagNameMap>(tag: K, attrs: Record<string, string | number>) {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, String(v));
  return el;
}

function clearOverlay() {
  els.overlay.replaceChildren();
  selection = null;
  hoverBox = null;
  if (textSelection?.page === state.current) {
    textSelection.bar.remove();
    textSelection = null;
  }
  closeNote();
  syncToolOptions();
}

/** Draws an outline on the drawing layer around a relative rectangle. */
function outline(r: RelRect, className: string, index = state.current) {
  const a = toPx(r.from, index);
  const b = toPx(r.to, index);
  const pad = 2;
  const box = svg("rect", {
    class: className,
    x: a.x - pad,
    y: a.y - pad,
    width: b.x - a.x + pad * 2,
    height: b.y - a.y + pad * 2,
  });
  views[index].overlay.append(box);
  return box;
}

// ---------- Render ----------

// ---------- Continuous view ----------

interface PageView {
  wrap: HTMLDivElement;
  img: HTMLImageElement;
  overlay: SVGSVGElement;
  /** Discards renders that arrive after a newer one. */
  seq: number;
  /** Zoom it was drawn at; -1 if it must be redrawn. */
  renderedZoom: number;
}

let views: PageView[] = [];
/** Pages near the visible area (they get drawn). */
const nearby = new Set<number>();
let pageObserver: IntersectionObserver | null = null;
/** Distant pages: the image is released to avoid piling up memory. */
let unloadObserver: IntersectionObserver | null = null;
/** While scrolling programmatically, the current page isn't recomputed. */
let ignoreScrollUntil = 0;

function sizeView(view: PageView, index: number) {
  const { width, height } = state.doc!.pages[index];
  const cssWidth = displayWidth(index);
  view.wrap.style.width = `${cssWidth}px`;
  view.img.style.width = `${cssWidth}px`;
  view.img.style.height = `${(cssWidth * height) / width}px`;
}

function buildPages() {
  pageObserver?.disconnect();
  unloadObserver?.disconnect();
  for (const view of views) setImage(view.img, "");
  views = [];
  nearby.clear();
  els.pages.replaceChildren();
  const doc = state.doc;
  els.pages.hidden = !doc;
  if (!doc) return;

  pageObserver = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        const index = Number((entry.target as HTMLElement).dataset.index);
        if (entry.isIntersecting) {
          nearby.add(index);
          if (views[index].renderedZoom !== state.zoomIndex) renderPage(index).catch(showError);
        } else {
          nearby.delete(index);
        }
      }
    },
    { root: els.viewer, rootMargin: "100% 0px" },
  );
  unloadObserver = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) continue;
        const view = views[Number((entry.target as HTMLElement).dataset.index)];
        if (!view || view.renderedZoom === -1) continue;
        view.seq++;
        view.renderedZoom = -1;
        setImage(view.img, "");
        view.img.removeAttribute("src");
      }
    },
    { root: els.viewer, rootMargin: "400% 0px" },
  );

  doc.pages.forEach((_, index) => {
    const wrap = document.createElement("div");
    wrap.className = "page-wrap";
    wrap.dataset.index = String(index);
    const img = document.createElement("img");
    img.className = "page";
    img.alt = "";
    img.draggable = false;
    const overlay = svg("svg", { class: "overlay" });
    overlay.addEventListener("pointerdown", (e) => onPagePointerDown(e, index));
    overlay.addEventListener("pointermove", (e) => onOverlayHover(e, index));
    overlay.addEventListener("pointerleave", () => {
      hoverPending = null;
      clearHover();
    });
    wrap.append(img, overlay);
    const view: PageView = { wrap, img, overlay, seq: 0, renderedZoom: -1 };
    sizeView(view, index);
    views.push(view);
    els.pages.append(wrap);
    pageObserver!.observe(wrap);
    unloadObserver!.observe(wrap);
  });
}

async function renderPage(index: number) {
  const view = views[index];
  if (!state.doc || !view) return;
  const seq = ++view.seq;
  const zoom = state.zoomIndex;
  const width = displayWidth(index) * window.devicePixelRatio;
  const url = batchPreviewing() ? await renderBatchPreview(index, width) : await renderPng(index, width);
  if (seq !== view.seq || views[index] !== view) {
    URL.revokeObjectURL(url);
    return;
  }
  setImage(view.img, url);
  view.renderedZoom = zoom;
  if (index === state.current) clearOverlay();
  else view.overlay.replaceChildren();
  drawBatchFields(index);
  drawSearchHits(index);
  drawFormFields(index).catch(() => {});
}

/** Redraws the current page (after editing it). */
function renderCurrent() {
  return renderPage(state.current);
}

/** Redraws the visible pages; the rest are drawn as they come close. */
function renderVisible() {
  for (const view of views) view.renderedZoom = -1;
  return Promise.all([...nearby].map((i) => renderPage(i))).then(() => {});
}

/** Changes the current page (the one tools act on) without scrolling the view. */
function setCurrent(index: number) {
  if (index === state.current) return;
  finishTextInput();
  clearSelection();
  clearTextSelection();
  clearHover();
  state.current = index;
  updateUi();
}

/** The current page is the one at the center of the view. */
function onViewerScroll() {
  if (!state.doc || textInput || performance.now() < ignoreScrollUntil || dragging) return;
  const r = els.viewer.getBoundingClientRect();
  const center = r.top + r.height / 2;
  let best = state.current;
  let bestDistance = Infinity;
  for (const i of nearby) {
    const box = views[i].wrap.getBoundingClientRect();
    const distance = center < box.top ? box.top - center : center > box.bottom ? center - box.bottom : 0;
    if (distance < bestDistance) [best, bestDistance] = [i, distance];
  }
  setCurrent(best);
}

function scrollToPage(index: number) {
  ignoreScrollUntil = performance.now() + 300;
  const top = views[index].wrap.offsetTop - els.pages.offsetTop;
  els.viewer.scrollTo({ top: Math.max(0, top) });
}

let thumbObserver: IntersectionObserver | null = null;

function renderThumb(item: HTMLElement) {
  const version = state.thumbsVersion;
  const index = Number(item.dataset.index);
  renderPng(index, THUMB_WIDTH * window.devicePixelRatio)
    .then((url) => {
      if (version !== state.thumbsVersion) return URL.revokeObjectURL(url);
      setImage(item.querySelector("img")!, url);
    })
    .catch(showError);
}

function buildThumbnails() {
  thumbObserver?.disconnect();
  els.thumbnails.querySelectorAll("img").forEach((img) => setImage(img, ""));
  els.thumbnails.replaceChildren();
  state.thumbsVersion++;
  if (!state.doc) return;

  thumbObserver = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue;
        thumbObserver!.unobserve(entry.target);
        renderThumb(entry.target as HTMLElement);
      }
    },
    { root: els.thumbnails, rootMargin: "200px" },
  );

  state.doc.pages.forEach((size, index) => {
    const item = document.createElement("button");
    item.className = "thumb";
    item.dataset.index = String(index);
    item.title = "Arrastra para reordenar";
    const img = document.createElement("img");
    img.draggable = false;
    img.style.aspectRatio = `${size.width} / ${size.height}`;
    const label = document.createElement("span");
    label.textContent = String(index + 1);
    item.append(img, label);
    setupThumbDrag(item, index);
    els.thumbnails.append(item);
    thumbObserver!.observe(item);
  });
}

function refreshThumb(index: number) {
  const item = els.thumbnails.querySelector<HTMLElement>(`.thumb[data-index="${index}"]`);
  if (item) renderThumb(item);
}

function updateUi() {
  const doc = state.doc;
  const has = doc !== null;
  els.empty.hidden = has;
  if (!has) els.pages.hidden = true;

  const docButtons = [
    els.search,
    els.sign,
    els.image,
    els.saveAs,
    els.zoomIn,
    els.zoomOut,
    els.rotateLeft,
    els.rotateRight,
    els.blank,
    els.insert,
    ...els.tools.querySelectorAll("button"),
  ];
  for (const b of docButtons) b.disabled = !has || state.busy;
  els.save.disabled = !has || !state.dirty || state.busy;
  els.undo.disabled = !has || !doc.canUndo || state.busy;
  els.redo.disabled = !has || !doc.canRedo || state.busy;
  els.del.disabled = !has || doc.pages.length <= 1 || state.busy;
  els.prev.disabled = !has || state.current === 0;
  els.next.disabled = !has || state.current >= doc.pages.length - 1;
  els.zoomOut.disabled ||= state.zoomIndex === 0;
  els.zoomIn.disabled ||= state.zoomIndex === ZOOM_STEPS.length - 1;
  els.batch.disabled = !has || state.busy || batch.active;
  // In batch mode the document is the template: it isn't edited, so the fields stay valid.
  if (batch.active) {
    const editing = [
      ...els.tools.querySelectorAll("button"),
      els.sign,
      els.image,
      els.rotateLeft,
      els.rotateRight,
      els.blank,
      els.insert,
      els.del,
      els.undo,
      els.redo,
    ];
    for (const b of editing) b.disabled = true;
  }

  els.pageIndicator.textContent = has ? `${state.current + 1} / ${doc.pages.length}` : "–";
  els.zoomIndicator.textContent = `${Math.round(ZOOM_STEPS[state.zoomIndex] * 100)}%`;

  const name = has ? baseName(doc.path) : "";
  els.fileName.textContent = name + (state.dirty ? " •" : "");
  els.fileName.title = has ? doc.path : "";
  getCurrentWindow()
    .setTitle(has ? `${state.dirty ? "• " : ""}${name} — Pliego` : "Pliego")
    .catch(() => {});

  els.thumbnails.querySelectorAll<HTMLElement>(".thumb").forEach((t) => {
    const active = Number(t.dataset.index) === state.current;
    t.classList.toggle("active", active);
    if (active) t.setAttribute("aria-current", "page");
    else t.removeAttribute("aria-current");
  });
  els.thumbnails.querySelector(".thumb.active")?.scrollIntoView({ block: "nearest" });
}

/**
 * Applies a new `DocInfo` from the backend and repaints.
 * With `page`, only that thumbnail is refreshed (edits that don't change the structure).
 */
function applyDoc(doc: DocInfo, opts: { dirty: boolean; page?: number }) {
  const structural = opts.page === undefined;
  state.doc = doc;
  state.dirty = opts.dirty;
  state.current = Math.max(0, Math.min(state.current, doc.pages.length - 1));
  if (structural) {
    formCache.clear();
    clearSearch();
    buildThumbnails();
    buildPages();
  } else {
    formCache.delete(opts.page!);
    refreshThumb(opts.page!);
  }
  updateUi();
  // When rebuilding, visible pages draw themselves; only the current one is awaited.
  return renderCurrent().catch(showError);
}

// ---------- Document actions ----------

function confirmDiscard() {
  return ask("Hay cambios sin guardar. ¿Descartarlos?", { title: "Pliego", kind: "warning" });
}

async function openPath(path: string) {
  if (batch.running) return;
  closeBatch();
  try {
    const doc = await openWithPassword(path);
    if (!doc) return;
    state.current = 0;
    formAnnounced = false;
    await applyDoc(doc, { dirty: false });
    scrollToPage(0);
  } catch (e) {
    showError(e);
  }
}

/** Opens the PDF; if it's protected, asks for the password until it's right or cancelled. */
async function openWithPassword(path: string) {
  let password: string | null = null;
  for (;;) {
    try {
      return await invoke<DocInfo>("open_pdf", { path, password });
    } catch (e) {
      if (!String(e).includes("contraseña")) throw e;
      password = await askPassword(baseName(path), password !== null);
      if (password === null) return null;
    }
  }
}

async function openDocument() {
  if (state.dirty && !(await confirmDiscard())) return;
  const path = await open({ multiple: false, filters: PDF_FILTER });
  if (path) await openPath(path);
}

async function saveDocument(saveAs: boolean) {
  if (!state.doc) return;
  let path: string | null = null;
  if (saveAs) {
    path = await save({ defaultPath: state.doc.path, filters: PDF_FILTER });
    if (!path) return;
  }
  try {
    state.doc = await invoke<DocInfo>("save_pdf", { path });
    state.dirty = false;
    updateUi();
  } catch (e) {
    showError(e);
  }
}

/**
 * Runs an edit command; prevents two edits from overlapping.
 * Returns the backend response, or `null` if it failed.
 */
async function edit<T extends DocInfo | { doc: DocInfo } = DocInfo>(
  command: string,
  args: Record<string, unknown>,
  page?: number,
): Promise<T | null> {
  if (state.busy) return null;
  if (batch.active) {
    showInfo("Cierra «Generar en serie» para editar el documento.");
    return null;
  }
  state.busy = true;
  updateUi();
  try {
    const result = await invoke<T>(command, args);
    state.busy = false;
    await applyDoc("doc" in result ? result.doc : result, { dirty: true, page });
    return result;
  } catch (e) {
    state.busy = false;
    updateUi();
    showError(e);
    return null;
  }
}

async function history(command: "undo" | "redo") {
  if (!state.doc || (command === "undo" ? !state.doc.canUndo : !state.doc.canRedo)) return;
  cancelTextInput();
  clearSelection();
  await edit(command, {});
}

async function insertPdfs(paths: string[]) {
  for (const path of paths) {
    const at = state.current + 1;
    const before = state.doc!.pages.length;
    if (!(await edit("insert_pdf", { path, at }))) return;
    // The last inserted page stays selected.
    goTo(at + state.doc!.pages.length - before - 1, true);
  }
}

async function insertDocument() {
  if (!state.doc) return;
  const picked = await open({ multiple: true, filters: PDF_FILTER });
  if (picked) await insertPdfs(picked);
}

async function insertBlankPage() {
  if (!state.doc) return;
  const at = state.current + 1;
  if (await edit("insert_blank_page", { at, like: state.current })) goTo(at);
}

function goTo(index: number, force = false) {
  if (!state.doc) return;
  const clamped = Math.max(0, Math.min(index, state.doc.pages.length - 1));
  if (clamped === state.current && !force) return;
  setCurrent(clamped);
  scrollToPage(clamped);
}

function zoom(delta: number) {
  const next = Math.max(0, Math.min(state.zoomIndex + delta, ZOOM_STEPS.length - 1));
  if (!state.doc || next === state.zoomIndex) return;
  finishTextInput();
  clearSelection();
  // Keeps the point of the current page that was at the top of the view.
  const view = views[state.current];
  const before = (els.viewer.scrollTop - view.wrap.offsetTop) / view.wrap.offsetHeight;
  state.zoomIndex = next;
  views.forEach(sizeView);
  ignoreScrollUntil = performance.now() + 300;
  els.viewer.scrollTop = view.wrap.offsetTop + before * view.wrap.offsetHeight;
  updateUi();
  renderVisible().catch(showError);
}

// ---------- Reordering thumbnails ----------

let suppressClick = false;
const dropIndicator = document.createElement("div");
dropIndicator.className = "drop-indicator";

/** Insertion position (0..n) from the pointer's height over the thumbnails. */
function dropIndexAt(clientY: number) {
  const thumbs = [...els.thumbnails.querySelectorAll<HTMLElement>(".thumb")];
  const i = thumbs.findIndex((t) => {
    const r = t.getBoundingClientRect();
    return clientY < r.top + r.height / 2;
  });
  return i === -1 ? thumbs.length : i;
}

function showDropIndicator(index: number) {
  const thumbs = els.thumbnails.querySelectorAll<HTMLElement>(".thumb");
  const gap = 6;
  const top =
    index < thumbs.length
      ? thumbs[index].offsetTop - gap
      : thumbs[thumbs.length - 1].offsetTop + thumbs[thumbs.length - 1].offsetHeight + gap;
  dropIndicator.style.top = `${top}px`;
  if (!dropIndicator.isConnected) els.thumbnails.append(dropIndicator);
}

function setupThumbDrag(item: HTMLElement, from: number) {
  item.addEventListener("click", () => {
    if (!suppressClick) goTo(from);
    suppressClick = false;
  });

  item.addEventListener("pointerdown", (down) => {
    if (down.button !== 0 || state.busy) return;
    const startY = down.clientY;
    let dragging = false;
    let target = from;
    let scrollTimer = 0;

    const onMove = (e: PointerEvent) => {
      if (!dragging && Math.abs(e.clientY - startY) < 6) return;
      if (!dragging) {
        dragging = true;
        item.classList.add("dragging");
        item.setPointerCapture(down.pointerId);
      }
      target = dropIndexAt(e.clientY);
      showDropIndicator(target);

      // Scrolls the list if the pointer is near the edges.
      clearInterval(scrollTimer);
      const r = els.thumbnails.getBoundingClientRect();
      const dir = e.clientY < r.top + 40 ? -1 : e.clientY > r.bottom - 40 ? 1 : 0;
      if (dir) scrollTimer = window.setInterval(() => els.thumbnails.scrollBy(0, dir * 12), 16);
    };

    const onUp = () => {
      window.removeEventListener("pointermove", onMove);
      clearInterval(scrollTimer);
      if (!dragging) return;
      suppressClick = true;
      item.classList.remove("dragging");
      dropIndicator.remove();
      if (target === from || target === from + 1) return;
      movePage(from, target);
    };

    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp, { once: true });
  });
}

async function movePage(from: number, to: number) {
  const order = state.doc!.pages.map((_, i) => i);
  order.splice(from, 1);
  order.splice(to > from ? to - 1 : to, 0, from);
  const previous = state.current;
  if (await edit("reorder_pages", { order })) goTo(order.indexOf(previous), true);
}

// ---------- Annotation tools ----------

function setTool(tool: Tool) {
  if (!state.doc || (batch.active && tool !== "batch")) return;
  finishTextInput();
  clearSelection();
  clearHover();
  els.overlay.style.cursor = "";
  if (tool !== "place") placing = null;
  state.tool = tool;
  els.pages.dataset.tool = tool;
  els.tools.querySelectorAll<HTMLButtonElement>("button").forEach((b) => {
    b.setAttribute("aria-pressed", String(b.dataset.tool === tool));
  });
  syncToolOptions();
}

/**
 * Shows the options of the current tool. With Select, if a text is
 * selected, shows its color so it can be changed.
 */
function syncToolOptions() {
  const tool = state.tool;
  // With Edit PDF text a click opens the editor: the color is changed from Select.
  const selected = selection && tool === "select" ? selectionColor(selection.target) : null;
  const toolColor = tool in state.colors;
  // When editing original PDF text the color can also be chosen.
  const editing = textInput?.dataset.styled !== undefined && !toolColor ? textInput.dataset.color! : null;
  els.toolOptions.hidden = !toolColor && selected === null && editing === null;
  els.sizeWrap.hidden = tool !== "text";
  els.widthWrap.hidden = tool !== "rect" && tool !== "pen";
  if (toolColor) els.color.value = state.colors[tool];
  else if (editing !== null) els.color.value = editing;
  else if (selected !== null) els.color.value = selected;
}

let textInput: HTMLTextAreaElement | null = null;
/** Commits the text being edited (if any). */
let commitText: (() => void) | null = null;

function cancelTextInput() {
  textInput?.remove();
  textInput = null;
  commitText = null;
  syncToolOptions();
}

function finishTextInput() {
  if (commitText) commitText();
  else cancelTextInput();
}

function autosize(ta: HTMLTextAreaElement) {
  ta.style.width = "1px";
  ta.style.height = "1px";
  ta.style.width = `${ta.scrollWidth + 4}px`;
  ta.style.height = `${ta.scrollHeight}px`;
}

function pageHeightPx() {
  return els.page.getBoundingClientRect().height;
}

/** Baseline of each line from the top-left corner of the block. */
function layoutLines(text: string, topLeft: RelPoint, fontPx: number) {
  const height = pageHeightPx();
  return text.split("\n").map((line, i) => ({
    text: line,
    x: topLeft.x,
    y: topLeft.y + (fontPx * (BASELINE_RATIO + i * LINE_HEIGHT)) / height,
  }));
}

function topLeftOf(info: TextInfo): RelPoint {
  const fontPx = info.size * pxPerPt();
  return { x: info.anchor.x, y: info.anchor.y - (fontPx * BASELINE_RATIO) / pageHeightPx() };
}

/** PDF font name to CSS: "ABCDEF+Arial-BoldMT" → "Arial". */
function cssFont(pdfFont: string) {
  const family = pdfFont.replace(/^[A-Z]{6}\+/, "").split(/[-,]/)[0];
  return `"${family}", Helvetica, Arial, sans-serif`;
}

function setTextSize(size: number) {
  state.textSize = size;
  const value = String(size);
  if (![...els.size.options].some((o) => o.value === value)) {
    els.size.append(new Option(value, value));
  }
  els.size.value = value;
}

interface EditorOptions {
  /** Top-left corner of the box. */
  at: RelPoint;
  value: string;
  fontPx: number;
  color: string;
  fontFamily?: string;
  minWidth?: number;
  /** Covers what's underneath (when editing existing text). */
  opaque?: boolean;
  /** Enter always commits; line breaks aren't allowed. */
  singleLine?: boolean;
  /** The toolbar's size and color are applied live. */
  styled?: boolean;
  /** Forces the commit even if the text didn't change. */
  styleChanged?: () => boolean;
  /** Receives the text and the color chosen while editing. */
  onCommit: (text: string, color: string) => Promise<unknown>;
  /** Tab commits and calls this (to move to the next form field). */
  onTab?: (back: boolean) => void;
}

function openTextEditor(opts: EditorOptions) {
  cancelTextInput();
  clearHover();
  const px = toPx(opts.at);
  const ta = document.createElement("textarea");
  ta.className = opts.opaque ? "text-input opaque" : "text-input";
  ta.rows = 1;
  ta.spellcheck = false;
  ta.value = opts.value;
  if (opts.styled) ta.dataset.styled = "";
  ta.dataset.color = opts.color;
  Object.assign(ta.style, {
    left: `${px.x}px`,
    top: `${px.y}px`,
    fontSize: `${opts.fontPx}px`,
    lineHeight: String(LINE_HEIGHT),
    color: opts.color,
    fontFamily: opts.fontFamily ?? "",
  });
  textInput = ta;
  els.pageWrap.append(ta);
  syncToolOptions();
  autosize(ta);
  if (opts.minWidth) ta.style.width = `${Math.max(opts.minWidth, ta.scrollWidth + 4)}px`;
  requestAnimationFrame(() => {
    ta.focus();
    ta.setSelectionRange(ta.value.length, ta.value.length);
  });

  let done = false;
  const commit = async () => {
    if (done) return;
    done = true;
    commitText = null;
    const text = ta.value.replace(/\s+$/, "");
    const color = ta.dataset.color!;
    const unchanged =
      text === opts.value.replace(/\s+$/, "") && color === opts.color && !opts.styleChanged?.();
    if (!unchanged) await opts.onCommit(text, color);
    if (textInput === ta) cancelTextInput();
  };
  commitText = commit;

  ta.addEventListener("input", () => {
    autosize(ta);
    if (opts.minWidth) ta.style.width = `${Math.max(opts.minWidth, ta.scrollWidth + 4)}px`;
  });
  ta.addEventListener("blur", (e) => {
    // Changing the size or color in the toolbar doesn't close the box.
    const to = e.relatedTarget;
    if (opts.styled && to instanceof Node && els.toolOptions.contains(to)) return;
    commit();
  });
  ta.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Escape") {
      done = true;
      cancelTextInput();
    } else if (e.key === "Enter" && (!e.shiftKey || opts.singleLine)) {
      e.preventDefault();
      commit();
    } else if (e.key === "Tab" && opts.onTab) {
      e.preventDefault();
      const next = opts.onTab;
      commit().then(() => next(e.shiftKey));
    }
  });
}

function newText(at: RelPoint) {
  const index = state.current;
  openTextEditor({
    at,
    value: "",
    fontPx: state.textSize * pxPerPt(),
    color: state.colors.text,
    styled: true,
    onCommit: (text) =>
      edit(
        "add_text",
        {
          index,
          lines: layoutLines(text, at, state.textSize * pxPerPt()),
          size: state.textSize,
          color: state.colors.text,
        },
        index,
      ),
  });
}

/** Reopens a text added with the app to edit it. */
function editOwnText(annotation: number, info: TextInfo) {
  const index = state.current;
  setTool("text");
  setTextSize(info.size);
  state.colors.text = info.color;
  els.color.value = info.color;
  const at = topLeftOf(info);
  openTextEditor({
    at,
    value: info.lines.join("\n"),
    fontPx: info.size * pxPerPt(),
    color: info.color,
    opaque: true,
    styled: true,
    styleChanged: () => state.textSize !== info.size || state.colors.text !== info.color,
    onCommit: (text) =>
      edit(
        "update_text",
        {
          index,
          annotation,
          lines: text ? layoutLines(text, at, state.textSize * pxPerPt()) : [],
          size: state.textSize,
          color: state.colors.text,
        },
        index,
      ),
  });
}

async function onTextToolClick(at: RelPoint) {
  const index = state.current;
  const hit = await invoke<AnnotationHit | null>("annotation_at", { index, at }).catch(() => null);
  if (index !== state.current) return;
  if (hit?.text) editOwnText(hit.annotation, hit.text);
  else newText(at);
}

/** Opens the editor over an original PDF text fragment (experimental). */
function editOriginalText(hit: TextObjectHit) {
  const index = state.current;
  const fontPx = hit.size * pxPerPt();
  const a = toPx(hit.rect.from);
  const b = toPx(hit.rect.to);
  // The rectangle starts at the top of the letters; the text box, slightly above.
  const top = { x: hit.rect.from.x, y: hit.rect.from.y - (fontPx * 0.15) / pageHeightPx() };
  openTextEditor({
    at: top,
    value: hit.text,
    fontPx,
    color: hit.color,
    fontFamily: cssFont(hit.font),
    minWidth: b.x - a.x,
    opaque: true,
    singleLine: true,
    styled: true,
    onCommit: async (text, color) => {
      const newColor = color === hit.color ? null : color;
      if (text === hit.text.replace(/\s+$/, "")) {
        // Only the color changed: no need to rewrite the text.
        return edit("recolor_text_object", { index, objects: hit.objects, color }, index);
      }
      const result = await edit<TextEditResult>(
        "edit_text_object",
        { index, objects: hit.objects, text, color: newColor },
        index,
      );
      if (result?.fontReplaced) {
        showInfo("La fuente original no tiene todas esas letras: este fragmento ahora usa otra fuente parecida.");
      }
    },
  });
}

// ---------- Selection and moving ----------

/** Something that can be touched on the page: an annotation or an original text fragment. */
type Hit = { kind: "annotation"; hit: AnnotationHit } | { kind: "object"; hit: TextObjectHit };

/**
 * With Select, annotations are looked up first and then original text;
 * with Edit PDF text, only original text.
 */
async function hitAt(index: number, at: RelPoint, tool: Tool): Promise<Hit | null> {
  if (tool === "select") {
    const annotation = await invoke<AnnotationHit | null>("annotation_at", { index, at });
    if (annotation) return { kind: "annotation", hit: annotation };
  }
  const object = await invoke<TextObjectHit | null>("text_object_at", { index, at });
  return object ? { kind: "object", hit: object } : null;
}

/**
 * What was added with the app is moved with Select; original PDF text only with
 * Edit PDF text (with Select, dragging over it selects it for copying).
 */
function isMovable(h: Hit, tool: Tool) {
  return h.kind === "object" ? tool === "edit-text" : h.hit.text !== null || h.hit.image;
}

function hitKey(h: Hit) {
  return h.kind === "object" ? `o${h.hit.objects.join(",")}` : `a${h.hit.annotation}`;
}

const NO_TEXT_HERE =
  "No hay texto editable en ese punto. Si ahí se ve texto, es parte de una imagen y no se puede editar como texto.";

let selection: { target: Hit; box: SVGRectElement; handle?: SVGRectElement } | null = null;
let lastClick = { key: "", time: 0 };

function clearSelection() {
  selection?.box.remove();
  selection?.handle?.remove();
  selection = null;
  syncToolOptions();
}

function select(target: Hit) {
  selection?.box.remove();
  selection?.handle?.remove();
  selection = { target, box: outline(target.hit.rect, "selection") };
  if (target.kind === "annotation" && target.hit.image) {
    selection.handle = resizeHandle(target.hit);
  }
  syncToolOptions();
}

/** Reselects whatever is at a point (after an edit that redraws). */
async function reselectAt(index: number, at: RelPoint) {
  const tool = state.tool;
  const again = await hitAt(index, at, tool).catch(() => null);
  if (again && index === state.current && tool === state.tool) select(again);
}

function center(r: RelRect): RelPoint {
  return { x: (r.from.x + r.to.x) / 2, y: (r.from.y + r.to.y) / 2 };
}

/** Color of a selected text; null if the selection isn't text (e.g. a drawing). */
function selectionColor(target: Hit) {
  return target.kind === "object" ? target.hit.color : (target.hit.text?.color ?? null);
}

let recolorTimer = 0;

/** The color picker fires many times while dragging: it's applied on release. */
function scheduleRecolor() {
  clearTimeout(recolorTimer);
  recolorTimer = window.setTimeout(recolorSelection, 350);
}

async function recolorSelection() {
  const target = selection?.target;
  const color = els.color.value;
  if (!target || selectionColor(target) === color) return;
  if (state.busy) return scheduleRecolor();

  const index = state.current;
  const { from, to } = target.hit.rect;
  const center = { x: (from.x + to.x) / 2, y: (from.y + to.y) / 2 };
  if (target.kind === "object") {
    await edit("recolor_text_object", { index, objects: target.hit.objects, color }, index);
  } else {
    const info = target.hit.text!;
    const lines = layoutLines(info.lines.join("\n"), topLeftOf(info), info.size * pxPerPt());
    await edit(
      "update_text",
      { index, annotation: target.hit.annotation, lines, size: info.size, color },
      index,
    );
  }
  // Redrawing loses the selection: the same text is picked again.
  const tool = state.tool;
  const again = await hitAt(index, center, tool).catch(() => null);
  if (again && !selection && index === state.current && tool === state.tool) select(again);
}

/**
 * Click: selects. Drag: moves a text (own or original).
 * With Select, double click edits; with Edit PDF text, a single click is enough.
 */
function pickAt(down: PointerEvent, tool: Tool) {
  const index = state.current;
  const start = relPoint(down);
  const r = els.page.getBoundingClientRect();
  let end = start;

  // If there's nothing under the pointer, dragging selects text for copying.
  let resolved: Hit | null | undefined;
  const band = svg("rect", { class: "band" });
  clearTextSelection();
  const hitPromise = hitAt(index, start, tool).catch((e) => (showError(e), null));
  hitPromise.then((target) => {
    resolved = target;
    clearSelection();
    if (!target || index !== state.current) return;
    select(target);
  });

  trackPointer(
    down,
    (e) => {
      end = relPoint(e);
      if (selection && isMovable(selection.target, tool)) {
        const dx = (end.x - start.x) * r.width;
        const dy = (end.y - start.y) * r.height;
        selection.box.setAttribute("transform", `translate(${dx} ${dy})`);
      } else if (resolved !== undefined && tool === "select") {
        const a = toPx(start);
        const b = toPx(end);
        if (!band.isConnected) els.overlay.append(band);
        band.setAttribute("x", String(Math.min(a.x, b.x)));
        band.setAttribute("y", String(Math.min(a.y, b.y)));
        band.setAttribute("width", String(Math.abs(a.x - b.x)));
        band.setAttribute("height", String(Math.abs(a.y - b.y)));
      }
    },
    async () => {
      band.remove();
      const target = await hitPromise;
      if (index !== state.current) return;
      if (!target) {
        if (tool === "edit-text") showInfo(NO_TEXT_HERE);
        else selectTextIn(index, start, end);
        return;
      }
      const moved = Math.hypot((end.x - start.x) * r.width, (end.y - start.y) * r.height) >= 3;

      if (moved && isMovable(target, tool)) {
        lastClick.key = "";
        await moveTarget(index, target, start, end);
        return;
      }
      if (moved && tool === "select") {
        lastClick.key = "";
        clearSelection();
        await selectTextIn(index, start, end);
        return;
      }

      if (target.kind === "annotation" && target.hit.note !== null) {
        openNote(index, target.hit.rect.from, target.hit);
        return;
      }
      const now = performance.now();
      const double = lastClick.key === hitKey(target) && now - lastClick.time < 400;
      lastClick = { key: hitKey(target), time: now };
      if (tool === "edit-text" || double) {
        if (target.kind === "object") editOriginalText(target.hit);
        else if (target.hit.text) editOwnText(target.hit.annotation, target.hit.text);
      }
    },
  );
}

async function moveTarget(index: number, target: Hit, from: RelPoint, to: RelPoint) {
  if (target.kind === "object") {
    return edit("move_text_object", { index, objects: target.hit.objects, from, to }, index);
  }
  if (target.hit.image) {
    const dx = to.x - from.x;
    const dy = to.y - from.y;
    const r = target.hit.rect;
    const moved = {
      from: { x: r.from.x + dx, y: r.from.y + dy },
      to: { x: r.to.x + dx, y: r.to.y + dy },
    };
    const ok = await edit("set_image_rect", { index, annotation: target.hit.annotation, ...moved }, index);
    if (ok) await reselectAt(index, center(moved));
    return ok;
  }
  const info = target.hit.text!;
  const topLeft = topLeftOf(info);
  const at = { x: topLeft.x + to.x - from.x, y: topLeft.y + to.y - from.y };
  const lines = layoutLines(info.lines.join("\n"), at, info.size * pxPerPt());
  return edit(
    "update_text",
    { index, annotation: target.hit.annotation, lines, size: info.size, color: info.color },
    index,
  );
}

function deleteSelection() {
  if (!selection) return;
  const { target } = selection;
  const index = state.current;
  clearSelection();
  if (target.kind === "annotation") {
    edit("delete_annotation", { index, annotation: target.hit.annotation }, index);
  } else {
    // Emptying an original fragment deletes it.
    edit("edit_text_object", { index, objects: target.hit.objects, text: "" }, index);
  }
}

// ---------- Forms ----------

/** Form fields per page (cleared when that page is edited). */
const formCache = new Map<number, Promise<FormField[]>>();
let formAnnounced = false;

function formFields(index: number) {
  let fields = formCache.get(index);
  if (!fields) {
    fields = invoke<FormField[]>("form_fields", { index }).catch(() => []);
    formCache.set(index, fields);
  }
  return fields;
}

function isFillable(f: FormField) {
  return !f.readOnly && ["text", "checkbox", "radio", "combo", "list"].includes(f.kind);
}

async function drawFormFields(index: number) {
  const view = views[index];
  const seq = view?.seq;
  const fields = (await formFields(index)).filter(isFillable);
  if (!fields.length || views[index] !== view || view.seq !== seq) return;
  view.overlay.querySelectorAll(".form-field").forEach((el) => el.remove());
  for (const f of fields) {
    const box = outline(f.rect, "form-field", index);
    // Behind selections and highlights.
    view.overlay.prepend(box);
  }
  if (!formAnnounced) {
    formAnnounced = true;
    showInfo("Este PDF tiene un formulario: haz clic en los campos celestes para rellenarlos.");
  }
}

async function fieldAt(index: number, at: RelPoint) {
  const fields = await formFields(index);
  return fields.find(
    (f) =>
      isFillable(f) &&
      at.x >= f.rect.from.x &&
      at.x <= f.rect.to.x &&
      at.y >= f.rect.from.y &&
      at.y <= f.rect.to.y,
  );
}

function setField(index: number, field: FormField, change: Record<string, unknown>) {
  return edit("set_form_field", { index, annotation: field.annotation, edit: change }, index);
}

/** Fills a field: types, checks, or opens the options. */
function fillField(index: number, field: FormField) {
  switch (field.kind) {
    case "checkbox":
    case "radio":
      return setField(index, field, { kind: "toggle" });
    case "combo":
    case "list":
      return chooseOption(index, field);
    case "text":
      return writeField(index, field);
  }
}

function writeField(index: number, field: FormField) {
  const a = toPx(field.rect.from, index);
  const b = toPx(field.rect.to, index);
  const height = b.y - a.y;
  const fontPx = Math.max(9, Math.min(field.multiline ? 14 * pxPerPt() : height * 0.7, 14 * pxPerPt()));
  openTextEditor({
    at: field.rect.from,
    value: field.value,
    fontPx,
    color: "#000000",
    minWidth: b.x - a.x,
    opaque: true,
    singleLine: !field.multiline,
    onCommit: (text) => setField(index, field, { kind: "text", value: text }),
    onTab: (back) => nextField(index, field, back),
  });
}

/** Tab: moves to the next text field (or the previous one with ⇧Tab). */
async function nextField(index: number, from: FormField, back: boolean) {
  const fields = (await formFields(index)).filter((f) => isFillable(f) && f.kind === "text");
  const i = fields.findIndex((f) => f.annotation === from.annotation);
  const next = fields[i + (back ? -1 : 1)];
  if (next && index === state.current) writeField(index, next);
}

function chooseOption(index: number, field: FormField) {
  const a = toPx(field.rect.from, index);
  const b = toPx(field.rect.to, index);
  const select = document.createElement("select");
  select.className = "field-select";
  Object.assign(select.style, { left: `${a.x}px`, top: `${a.y}px`, width: `${Math.max(120, b.x - a.x)}px` });
  if (field.selected === null) select.append(new Option("Elige una opción…", "", true, true));
  field.options.forEach((o, i) => select.append(new Option(o, String(i), false, i === field.selected)));
  if (field.kind === "list") select.size = Math.min(8, Math.max(2, field.options.length));
  views[index].wrap.append(select);
  select.focus();
  // Removing it fires "blur" while it's being removed, which calls close again.
  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    select.remove();
  };
  select.addEventListener("blur", close);
  select.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Escape") close();
  });
  select.addEventListener("change", () => {
    const choice = Number(select.value);
    close();
    if (select.value !== "" && choice !== field.selected) setField(index, field, { kind: "choice", index: choice });
  });
}

// ---------- Sticky notes ----------

let notePop: HTMLElement | null = null;

function closeNote() {
  notePop?.remove();
  notePop = null;
}

/** Small popup to write a new note (existing = null) or view and edit an existing one. */
function openNote(index: number, at: RelPoint, existing: AnnotationHit | null) {
  closeNote();
  const pop = document.createElement("div");
  pop.className = "note-pop";
  const area = document.createElement("textarea");
  area.placeholder = "Escribe tu nota…";
  area.value = existing?.note ?? "";
  const actions = document.createElement("div");
  actions.className = "note-actions";
  const button = (label: string, run: () => unknown, cls = "") => {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = label;
    if (cls) b.className = cls;
    b.addEventListener("click", () => run());
    actions.append(b);
    return b;
  };

  const save = async () => {
    const text = area.value.trim();
    closeNote();
    if (existing) {
      if (text !== existing.note) {
        await edit("set_note", { index, annotation: existing.annotation, text }, index);
      }
    } else {
      await edit("add_note", { index, at, text, color: state.colors.note }, index);
    }
  };
  if (existing) {
    button("Eliminar", async () => {
      closeNote();
      clearSelection();
      await edit("delete_annotation", { index, annotation: existing.annotation }, index);
    }, "danger");
  }
  button("Cancelar", closeNote);
  button(existing ? "Guardar" : "Agregar nota", save, "primary");

  area.addEventListener("keydown", (e) => {
    e.stopPropagation();
    if (e.key === "Escape") closeNote();
    else if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) save();
  });
  pop.addEventListener("pointerdown", (e) => e.stopPropagation());
  pop.append(area, actions);

  const p = toPx(at, index);
  const page = views[index].img.getBoundingClientRect();
  pop.style.left = `${Math.max(0, Math.min(p.x + 26, page.width - 260))}px`;
  pop.style.top = `${Math.max(0, Math.min(p.y, page.height - 170))}px`;
  views[index].wrap.append(pop);
  notePop = pop;
  area.focus();
}

// ---------- Signatures and images ----------

/** What gets placed with the next click on the page ("place" mode). */
let placing: { source: ImageSource; width: number } | null = null;

function startPlacing(source: ImageSource, width: number, hint: string) {
  setTool("place");
  placing = { source, width };
  showInfo(hint);
}

async function placeAt(at: RelPoint) {
  if (!placing) return setTool("select");
  const index = state.current;
  const { source, width } = placing;
  const ok = await edit("add_image", { index, source, center: at, width }, index);
  placing = null;
  setTool("select");
  if (ok) await reselectAt(index, at);
}

async function signDocument() {
  if (!state.doc || state.busy) return;
  finishTextInput();
  const source = await chooseSignature();
  if (source) startPlacing(source, 0.25, "Haz clic en la página donde va la firma (Esc para cancelar).");
}

async function insertImage() {
  if (!state.doc || state.busy) return;
  finishTextInput();
  const path = await open({
    multiple: false,
    filters: [{ name: "Imágenes", extensions: ["png", "jpg", "jpeg", "webp", "gif", "bmp"] }],
  });
  if (path) startPlacing({ kind: "file", path }, 0.4, "Haz clic en la página donde va la imagen (Esc para cancelar).");
}

/** Handle at the bottom-right corner of a selected image: resizes it. */
function resizeHandle(hit: AnnotationHit) {
  const index = state.current;
  const corner = toPx(hit.rect.to);
  const size = 10;
  const handle = svg("rect", {
    class: "resize-handle",
    x: corner.x - size / 2 + 2,
    y: corner.y - size / 2 + 2,
    width: size,
    height: size,
    rx: 2,
  });
  handle.addEventListener("pointerdown", (down) => {
    if (down.button !== 0 || state.busy) return;
    down.stopPropagation();
    down.preventDefault();
    const box = selection!.box;
    const a = toPx(hit.rect.from);
    const b = toPx(hit.rect.to);
    const aspect = (b.y - a.y) / (b.x - a.x);
    let width = b.x - a.x;
    trackPointer(
      down,
      (e) => {
        const p = toPx(relPoint(e));
        // The aspect ratio is kept: the direction that moved the most wins.
        width = Math.max(12, Math.max(p.x - a.x, (p.y - a.y) / aspect));
        box.setAttribute("width", String(width + 4));
        box.setAttribute("height", String(width * aspect + 4));
        handle.setAttribute("x", String(a.x + width - size / 2 + 2));
        handle.setAttribute("y", String(a.y + width * aspect - size / 2 + 2));
      },
      async () => {
        const r = views[index].img.getBoundingClientRect();
        const to = {
          x: Math.min(1, hit.rect.from.x + width / r.width),
          y: Math.min(1, hit.rect.from.y + (width * aspect) / r.height),
        };
        const rect = { from: hit.rect.from, to };
        const ok = await edit("set_image_rect", { index, annotation: hit.annotation, ...rect }, index);
        if (ok) await reselectAt(index, center(rect));
      },
    );
  });
  els.overlay.append(handle);
  return handle;
}

// ---------- Hover highlight ----------

let hoverBox: SVGRectElement | null = null;
let hoverPending: { at: RelPoint; page: number } | null = null;
let hoverRunning = false;

function clearHover() {
  hoverBox?.remove();
  hoverBox = null;
}

/** Marks what can be touched under the pointer. Only one query runs at a time. */
async function onOverlayHover(e: PointerEvent, page: number) {
  if (!state.doc || state.busy || textInput || e.buttons) return;
  if (state.tool !== "select" && state.tool !== "edit-text" && state.tool !== "batch") return;
  hoverPending = { at: relPoint(e, page), page };
  if (hoverRunning) return;
  hoverRunning = true;
  try {
    while (hoverPending) {
      const { at, page: index } = hoverPending;
      hoverPending = null;
      const target = await hitAt(index, at, state.tool);
      clearHover();
      const view = views[index];
      if (!view) continue;
      view.overlay.style.cursor = !target
        ? ""
        : isMovable(target, state.tool)
          ? "move"
          : target.kind === "object"
            ? "text"
            : "pointer";
      if (target && !textInput && !state.busy) hoverBox = outline(target.hit.rect, "hover", index);
    }
  } catch {
    // A failed query just stops marking.
  } finally {
    hoverRunning = false;
  }
}

function startShape(down: PointerEvent, tool: "highlight" | "rect" | "redact") {
  const start = relPoint(down);
  const color = state.colors[tool] ?? "#000000";
  const shape =
    tool === "redact"
      ? svg("rect", { class: "redact-preview" })
      : svg("rect", {
          fill: tool === "highlight" ? color : "none",
          "fill-opacity": tool === "highlight" ? 0.4 : 0,
          stroke: tool === "rect" ? color : "none",
          "stroke-width": state.strokeWidth * pxPerPt(),
        });
  els.overlay.append(shape);
  let end = start;

  const draw = () => {
    const a = toPx(start);
    const b = toPx(end);
    shape.setAttribute("x", String(Math.min(a.x, b.x)));
    shape.setAttribute("y", String(Math.min(a.y, b.y)));
    shape.setAttribute("width", String(Math.abs(a.x - b.x)));
    shape.setAttribute("height", String(Math.abs(a.y - b.y)));
  };

  trackPointer(
    down,
    (e) => {
      end = relPoint(e);
      draw();
    },
    async () => {
      const a = toPx(start);
      const b = toPx(end);
      if (Math.abs(a.x - b.x) < 3 || Math.abs(a.y - b.y) < 3) return shape.remove();
      const index = state.current;
      if (tool === "redact") {
        await redactAreas([{ page: index, rects: [{ from: start, to: end }] }]);
        shape.remove();
        return;
      }
      const args =
        tool === "highlight"
          ? { index, from: start, to: end, color }
          : { index, from: start, to: end, color, width: state.strokeWidth };
      if (!(await edit(tool === "highlight" ? "add_highlight" : "add_rect", args, index))) {
        shape.remove();
      }
    },
  );
}

function startPen(down: PointerEvent) {
  const points = [relPoint(down)];
  const line = svg("polyline", {
    fill: "none",
    stroke: state.colors.pen,
    "stroke-width": state.strokeWidth * pxPerPt(),
    "stroke-linecap": "round",
    "stroke-linejoin": "round",
  });
  els.overlay.append(line);

  const draw = () => {
    line.setAttribute(
      "points",
      points
        .map(toPx)
        .map((p) => `${p.x},${p.y}`)
        .join(" "),
    );
  };
  draw();

  trackPointer(
    down,
    (e) => {
      const p = relPoint(e);
      const last = toPx(points[points.length - 1]);
      const cur = toPx(p);
      if (Math.hypot(cur.x - last.x, cur.y - last.y) >= 1.5) {
        points.push(p);
        draw();
      }
    },
    async () => {
      const index = state.current;
      const args = { index, points, width: state.strokeWidth, color: state.colors.pen };
      if (!(await edit("add_ink", args, index))) line.remove();
    },
  );
}

/** A drag is in progress over a page. */
let dragging = false;

function trackPointer(down: PointerEvent, onMove: (e: PointerEvent) => void, onUp: () => void) {
  const overlay = els.overlay;
  overlay.setPointerCapture(down.pointerId);
  dragging = true;
  const move = (e: PointerEvent) => onMove(e);
  overlay.addEventListener("pointermove", move);
  overlay.addEventListener(
    "pointerup",
    () => {
      dragging = false;
      overlay.removeEventListener("pointermove", move);
      onUp();
    },
    { once: true },
  );
}

function onPagePointerDown(e: PointerEvent, index: number) {
  if (e.button !== 0 || !state.doc || state.busy) return;
  // A click on another page first makes it current (commits a text being edited).
  if (index !== state.current) {
    if (textInput) {
      e.preventDefault();
      finishTextInput();
      return;
    }
    setCurrent(index);
  }
  onOverlayPointerDown(e);
}

/** With Select, a click on a form field fills it; otherwise it selects. */
async function pickOrFill(e: PointerEvent) {
  const index = state.current;
  const at = relPoint(e);
  const cached = formCache.has(index);
  const field = cached ? await fieldAt(index, at) : undefined;
  if (field) {
    clearSelection();
    fillField(index, field);
    return;
  }
  pickAt(e, "select");
}

function onOverlayPointerDown(e: PointerEvent) {
  if (e.button !== 0 || !state.doc || state.busy) return;
  e.preventDefault();
  // With a text being edited, clicking outside only commits it.
  if (textInput) {
    finishTextInput();
    return;
  }
  clearHover();
  switch (state.tool) {
    case "select":
      pickOrFill(e);
      break;
    case "edit-text":
      pickAt(e, state.tool);
      break;
    case "batch":
      batchPointerDown(e);
      break;
    case "place":
      placeAt(relPoint(e));
      break;
    case "text":
      onTextToolClick(relPoint(e));
      break;
    case "highlight":
    case "rect":
    case "redact":
      startShape(e, state.tool);
      break;
    case "pen":
      startPen(e);
      break;
    case "eraser":
      edit("erase_at", { index: state.current, at: relPoint(e) }, state.current);
      break;
    case "note":
      e.preventDefault();
      openNote(state.current, relPoint(e), null);
      break;
  }
}

// ---------- Search ----------

interface SearchHit {
  page: number;
  rects: RelRect[];
}

const search = {
  query: "",
  hits: [] as SearchHit[],
  /** Highlighted match; -1 if none. */
  current: -1,
  /** Discards responses from earlier searches. */
  seq: 0,
};

function openSearch() {
  if (!state.doc) return;
  els.searchBar.hidden = false;
  els.searchInput.focus();
  els.searchInput.select();
}

function closeSearch() {
  els.searchBar.hidden = true;
  clearSearch();
}

function clearSearch() {
  search.seq++;
  search.query = "";
  search.hits = [];
  search.current = -1;
  els.searchCount.textContent = "";
  els.searchRedact.disabled = true;
  document.querySelectorAll(".overlay .search-hit").forEach((el) => el.remove());
}

let searchTimer = 0;

async function runSearch() {
  const query = els.searchInput.value.trim();
  if (query === search.query) return;
  const seq = ++search.seq;
  if (!query) return clearSearch();
  try {
    const hits = await invoke<SearchHit[]>("search_text", { query });
    if (seq !== search.seq) return;
    search.query = query;
    search.hits = hits;
    // Start from the first match from the current page on.
    const next = hits.findIndex((h) => h.page >= state.current);
    search.current = hits.length ? Math.max(0, next) : -1;
    redrawSearch();
    if (search.current >= 0) revealHit();
  } catch (e) {
    showError(e);
  }
}

function stepSearch(delta: number) {
  if (els.searchInput.value.trim() !== search.query) {
    clearTimeout(searchTimer);
    runSearch();
    return;
  }
  if (!search.hits.length) return;
  search.current = (search.current + delta + search.hits.length) % search.hits.length;
  redrawSearch();
  revealHit();
}

function redrawSearch() {
  document.querySelectorAll(".overlay .search-hit").forEach((el) => el.remove());
  const pages = new Set(search.hits.map((h) => h.page));
  for (const page of pages) if (views[page]?.renderedZoom !== -1) drawSearchHits(page);
  const n = search.hits.length;
  els.searchCount.textContent = !search.query ? "" : n ? `${search.current + 1} de ${n}` : "Sin resultados";
  els.searchRedact.disabled = n === 0;
}

function drawSearchHits(index: number) {
  search.hits.forEach((hit, i) => {
    if (hit.page !== index) return;
    for (const r of hit.rects) {
      const box = outline(r, i === search.current ? "search-hit current" : "search-hit", index);
      box.setAttribute("rx", "2");
    }
  });
}

/** Brings the current match into view. */
function revealHit() {
  const hit = search.hits[search.current];
  if (hit.page !== state.current) setCurrent(hit.page);
  const view = views[hit.page];
  const r = hit.rects[0];
  const height = view.wrap.offsetHeight;
  const y = view.wrap.offsetTop - els.pages.offsetTop + r.from.y * height;
  const visible = els.viewer.clientHeight;
  if (y < els.viewer.scrollTop + 40 || y > els.viewer.scrollTop + visible - 60) {
    ignoreScrollUntil = performance.now() + 300;
    els.viewer.scrollTo({ top: Math.max(0, y - visible / 3) });
  }
}

// ---------- Text selection for copying ----------

let textSelection: {
  page: number;
  text: string;
  rects: RelRect[];
  boxes: SVGRectElement[];
  bar: HTMLElement;
} | null = null;

function clearTextSelection() {
  textSelection?.boxes.forEach((b) => b.remove());
  textSelection?.bar.remove();
  textSelection = null;
}

type MarkupKind = "highlight" | "underline" | "strikeout";
const MARKUP_COLORS: Record<MarkupKind, string> = {
  highlight: "#ffd400",
  underline: "#1f6feb",
  strikeout: "#e01e1e",
};

/** Bar next to the selected text: copy, highlight, underline or strike out. */
function selectionBar(index: number, rects: RelRect[]) {
  const bar = document.createElement("div");
  bar.className = "selection-bar";
  const actions: [string, string, () => unknown][] = [
    ["Copiar", "Copiar (⌘C)", copySelection],
    ["Resaltar", "Resaltar el texto seleccionado", () => markSelection("highlight")],
    ["Subrayar", "Subrayar el texto seleccionado", () => markSelection("underline")],
    ["Tachar", "Tachar el texto seleccionado", () => markSelection("strikeout")],
    ["Ocultar", "Borrar del PDF el texto seleccionado y taparlo con negro", redactSelection],
  ];
  for (const [label, title, run] of actions) {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = label;
    b.title = title;
    b.addEventListener("pointerdown", (e) => e.stopPropagation());
    b.addEventListener("click", () => run());
    bar.append(b);
  }
  // Below the last line (or above if it doesn't fit).
  const last = rects[rects.length - 1];
  const a = toPx(last.from, index);
  const b = toPx(last.to, index);
  const page = views[index].img.getBoundingClientRect();
  const below = b.y + 8;
  bar.style.left = `${Math.max(0, Math.min(a.x, page.width - 300))}px`;
  bar.style.top = `${below + 36 > page.height ? toPx(rects[0].from, index).y - 40 : below}px`;
  views[index].wrap.append(bar);
  return bar;
}

async function redactSelection() {
  if (!textSelection || state.busy) return;
  const { page, rects } = textSelection;
  clearTextSelection();
  await redactAreas([{ page, rects }]);
}

// ---------- Redaction ----------

let redactExplained = false;

/** Truly removes what's in the areas and covers them with black. */
async function redactAreas(areas: { page: number; rects: RelRect[] }[]) {
  const pages = [...new Set(areas.map((a) => a.page))];
  const ok = await edit("redact", { areas }, pages.length === 1 ? pages[0] : undefined);
  if (ok && !redactExplained) {
    redactExplained = true;
    showInfo("Lo tapado se borró del PDF (texto, imágenes y anotaciones), no solo se cubrió. ⌘Z para deshacer.");
  }
  return ok;
}

async function redactSearchHits() {
  const hits = search.hits;
  if (!hits.length || state.busy) return;
  const n = hits.length;
  const yes = await ask(
    `¿Ocultar ${n === 1 ? "la coincidencia" : `las ${n} coincidencias`} de "${search.query}"? Se borran del PDF y se tapan con negro.`,
    { title: "Ocultar", kind: "warning", okLabel: "Ocultar", cancelLabel: "Cancelar" },
  );
  if (!yes) return;
  const byPage = new Map<number, RelRect[]>();
  for (const h of hits) byPage.set(h.page, [...(byPage.get(h.page) ?? []), ...h.rects]);
  const areas = [...byPage].map(([page, rects]) => ({ page, rects }));
  if (await redactAreas(areas)) closeSearch();
}

async function markSelection(kind: MarkupKind) {
  if (!textSelection || state.busy) return;
  const { page: index, rects } = textSelection;
  clearTextSelection();
  await edit("add_markup", { index, rects, kind, color: MARKUP_COLORS[kind] }, index);
}

/** Selects (for copying) the text inside the rectangle between two points. */
async function selectTextIn(index: number, a: RelPoint, b: RelPoint) {
  const from = { x: Math.min(a.x, b.x), y: Math.min(a.y, b.y) };
  const to = { x: Math.max(a.x, b.x), y: Math.max(a.y, b.y) };
  const r = views[index].img.getBoundingClientRect();
  if ((to.x - from.x) * r.width < 4 || (to.y - from.y) * r.height < 4) return;
  const found = await invoke<{ text: string; rects: RelRect[] } | null>("text_in_rect", {
    index,
    from,
    to,
  }).catch((e) => (showError(e), null));
  if (!found || index !== state.current) return;
  clearTextSelection();
  textSelection = {
    page: index,
    text: found.text,
    rects: found.rects,
    boxes: found.rects.map((rect) => outline(rect, "text-selection", index)),
    bar: selectionBar(index, found.rects),
  };
}

async function copySelection() {
  const text =
    textSelection?.text ??
    (selection?.target.kind === "object"
      ? selection.target.hit.text
      : selection?.target.hit.text?.lines.join("\n"));
  if (!text) return false;
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Fallback for webviews without access to the modern clipboard.
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.opacity = "0";
    document.body.append(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    if (!ok) {
      showError("No se pudo copiar el texto.");
      return true;
    }
  }
  showInfo("Copiado.");
  return true;
}

// ---------- Batch generation ----------

interface BatchField {
  page: number;
  objects: number[];
  text: string;
  rect: RelRect;
  column: number;
  align: "left" | "center" | "right";
  /** Shrink the text if it doesn't fit the page. */
  shrink: boolean;
  /** QR code field (drawn in `rect`) instead of text. */
  qr: boolean;
}

interface Table {
  headers: string[];
  rows: string[][];
}

interface BatchResult {
  generated: number;
  cancelled: boolean;
  fontReplacedRows: number[];
  files: string[];
}

const ALIGN_LABELS = { left: "Izq.", center: "Centro", right: "Der." } as const;

const batch = {
  active: false,
  running: false,
  fields: [] as BatchField[],
  table: null as Table | null,
  tableName: "",
  /** Row shown in the preview. */
  row: 0,
  /** The file name fills itself in until the person edits it. */
  patternAuto: true,
};

function batchPreviewing() {
  return batch.active && batch.fields.length > 0 && (batch.table?.rows.length ?? 0) > 0;
}

function backendFields() {
  return batch.fields.map(({ page, objects, column, align, shrink, qr, rect }) => ({
    page,
    objects,
    column,
    align,
    shrink,
    qr: qr ? rect : null,
  }));
}

async function renderBatchPreview(index: number, width: number) {
  const buf = await invoke<ArrayBuffer>("batch_preview", {
    fields: backendFields(),
    row: batch.table!.rows[batch.row],
    page: index,
    width: Math.round(width),
  });
  return URL.createObjectURL(new Blob([buf], { type: "image/png" }));
}

function openBatch() {
  if (!state.doc || batch.active) return;
  finishTextInput();
  clearSelection();
  batch.active = true;
  setTool("batch");
  els.batchPanel.hidden = false;
  els.batchResult.textContent = "";
  renderBatchPanel();
  updateUi();
}

function closeBatch() {
  if (!batch.active || batch.running) return;
  Object.assign(batch, { active: false, fields: [], table: null, tableName: "", row: 0, patternAuto: true });
  els.batchPattern.value = "";
  els.batchPanel.hidden = true;
  setTool("select");
  updateUi();
  renderVisible().catch(showError);
}

/** Draws the marked fields of a page with their number. */
function drawBatchFields(index: number) {
  if (!batch.active) return;
  batch.fields.forEach((field, i) => {
    if (field.page !== index) return;
    outline(field.rect, field.qr ? "field qr" : "field", index);
    const corner = toPx(field.rect.from, index);
    const label = svg("text", { class: "field-label", x: corner.x - 2, y: corner.y - 6 });
    label.textContent = field.qr ? `${i + 1} · QR` : String(i + 1);
    views[index].overlay.append(label);
  });
}

/** Click: marks or unmarks a text. Drag: box for a QR code. */
function batchPointerDown(down: PointerEvent) {
  const index = state.current;
  const start = relPoint(down);
  let end = start;
  const box = svg("rect", { class: "field qr" });
  trackPointer(
    down,
    (e) => {
      end = relPoint(e);
      const a = toPx(start);
      const b = toPx(end);
      if (!box.isConnected) els.overlay.append(box);
      // Square: a QR code always is.
      const side = Math.max(Math.abs(a.x - b.x), Math.abs(a.y - b.y));
      box.setAttribute("x", String(b.x < a.x ? a.x - side : a.x));
      box.setAttribute("y", String(b.y < a.y ? a.y - side : a.y));
      box.setAttribute("width", String(side));
      box.setAttribute("height", String(side));
    },
    () => {
      box.remove();
      const a = toPx(start);
      const b = toPx(end);
      const side = Math.max(Math.abs(a.x - b.x), Math.abs(a.y - b.y));
      if (side < 12) return toggleBatchField(start);
      const x = b.x < a.x ? a.x - side : a.x;
      const y = b.y < a.y ? a.y - side : a.y;
      const columns = batch.table?.headers.length ?? Infinity;
      batch.fields.push({
        page: index,
        objects: [],
        text: "Código QR",
        rect: { from: relFromPx(x, y, index), to: relFromPx(x + side, y + side, index) },
        column: Math.min(batch.fields.length, columns - 1),
        align: "left",
        shrink: false,
        qr: true,
      });
      onBatchChanged();
    },
  );
}

async function toggleBatchField(at: RelPoint) {
  const index = state.current;
  const hit = await invoke<TextObjectHit | null>("text_object_at", { index, at }).catch(
    (e) => (showError(e), null),
  );
  if (index !== state.current || !batch.active) return;
  if (!hit) return showInfo(NO_TEXT_HERE);

  const key = hit.objects.join(",");
  const existing = batch.fields.findIndex((f) => f.page === index && f.objects.join(",") === key);
  if (existing >= 0) {
    batch.fields.splice(existing, 1);
  } else {
    // If the text is centered on the page, it most likely should stay centered.
    const center = (hit.rect.from.x + hit.rect.to.x) / 2;
    const columns = batch.table?.headers.length ?? Infinity;
    batch.fields.push({
      page: index,
      objects: hit.objects,
      text: hit.text,
      rect: hit.rect,
      column: Math.min(batch.fields.length, columns - 1),
      align: Math.abs(center - 0.5) < 0.04 ? "center" : "left",
      shrink: true,
      qr: false,
    });
  }
  onBatchChanged();
}

function onBatchChanged() {
  renderBatchPanel();
  renderVisible().catch(showError);
}

function fileNames() {
  const { headers, rows } = batch.table!;
  return rows.map((row) =>
    els.batchPattern.value.replace(/\{([^}]*)\}/g, (_, name: string) => {
      const i = headers.findIndex((h) => h.toLowerCase() === name.trim().toLowerCase());
      return i >= 0 ? row[i] : "";
    }),
  );
}

function batchMode() {
  return document.querySelector<HTMLInputElement>('input[name="batch-mode"]:checked')!.value as
    | "files"
    | "single";
}

function renderBatchPanel() {
  const table = batch.table;

  els.batchFields.replaceChildren(
    ...batch.fields.map((field, i) => {
      const li = document.createElement("li");
      const text = document.createElement("div");
      text.className = "field-text";
      text.title = field.text;
      text.textContent = `${i + 1}. “${field.text}” `;
      const page = document.createElement("small");
      page.textContent = `pág. ${field.page + 1}`;
      text.append(page);

      const remove = document.createElement("button");
      remove.className = "icon";
      remove.title = "Quitar campo";
      remove.textContent = "×";
      remove.addEventListener("click", () => {
        batch.fields.splice(i, 1);
        onBatchChanged();
      });

      const options = document.createElement("div");
      options.className = "field-options";
      const column = document.createElement("select");
      column.title = "Columna con los valores";
      if (table) {
        table.headers.forEach((h, c) => column.append(new Option(h, String(c), false, c === field.column)));
      } else {
        column.append(new Option("Carga los datos…", ""));
        column.disabled = true;
      }
      column.addEventListener("change", () => {
        field.column = Number(column.value);
        onBatchChanged();
      });

      const align = document.createElement("div");
      align.className = "group segmented small";
      align.title = "Alineación del texto nuevo";
      for (const value of ["left", "center", "right"] as const) {
        const b = document.createElement("button");
        b.textContent = ALIGN_LABELS[value];
        b.setAttribute("aria-pressed", String(field.align === value));
        b.addEventListener("click", () => {
          field.align = value;
          onBatchChanged();
        });
        align.append(b);
      }
      if (field.qr) {
        options.append(column);
      } else {
        const shrink = document.createElement("label");
        shrink.className = "radio";
        shrink.title = "Si el valor es muy largo, se achica la letra para que no se salga de la página";
        const check = document.createElement("input");
        check.type = "checkbox";
        check.checked = field.shrink;
        check.addEventListener("change", () => {
          field.shrink = check.checked;
          onBatchChanged();
        });
        shrink.append(check, " Achicar si no entra");
        options.append(column, align, shrink);
      }
      li.append(text, remove, options);
      return li;
    }),
  );

  els.batchTableInfo.textContent = table
    ? `${batch.tableName}: ${table.rows.length} filas · columnas: ${table.headers.join(", ")}`
    : "La primera fila debe tener los nombres de las columnas.";

  const previewing = batchPreviewing();
  els.batchPreviewNav.hidden = !previewing;
  if (previewing) {
    const value = table!.rows[batch.row][batch.fields[0].column] ?? "";
    els.batchRowLabel.textContent = `Vista previa: fila ${batch.row + 1} de ${table!.rows.length} · ${value}`;
    els.batchPrev.disabled = batch.row === 0;
    els.batchNext.disabled = batch.row >= table!.rows.length - 1;
  }

  if (batch.patternAuto && table && batch.fields.length) {
    els.batchPattern.value = `{${table.headers[batch.fields[0].column]}}`;
  }
  els.batchPattern.disabled = batchMode() !== "files";

  const count = table?.rows.length ?? 0;
  els.batchRun.disabled = !previewing || batch.running;
  els.batchRun.textContent =
    batchMode() === "files"
      ? `Generar ${count || ""} PDF${count === 1 ? "" : "s"}`
      : `Generar 1 PDF con ${count || ""} página${count === 1 ? "" : "s"}`;
}

async function loadBatchTable() {
  const path = await open({
    multiple: false,
    filters: [{ name: "Excel o CSV", extensions: ["xlsx", "xlsm", "xls", "ods", "csv", "tsv", "txt"] }],
  });
  if (!path) return;
  try {
    const table = await invoke<Table>("read_table_file", { path });
    batch.table = table;
    batch.tableName = baseName(path);
    batch.row = 0;
    batch.fields.forEach((f, i) => (f.column = Math.min(i, table.headers.length - 1)));
    onBatchChanged();
  } catch (e) {
    showError(e);
  }
}

async function runBatch() {
  if (!batchPreviewing() || batch.running) return;
  const table = batch.table!;
  let output: Record<string, unknown>;
  if (batchMode() === "files") {
    const dir = await open({ directory: true, title: "Carpeta donde guardar los PDFs" });
    if (!dir) return;
    output = { mode: "files", dir, names: fileNames() };
  } else {
    const path = await save({ defaultPath: "generados.pdf", filters: PDF_FILTER });
    if (!path) return;
    output = { mode: "single", path };
  }

  batch.running = true;
  els.batchResult.textContent = "";
  els.batchProgress.hidden = false;
  els.batchProgressBar.value = 0;
  els.batchProgressBar.max = table.rows.length;
  els.batchProgressLabel.textContent = `0 / ${table.rows.length}`;
  renderBatchPanel();
  const unlisten = await listen<{ done: number; total: number }>("batch-progress", (e) => {
    els.batchProgressBar.value = e.payload.done;
    els.batchProgressLabel.textContent = `${e.payload.done} / ${e.payload.total}`;
  });

  try {
    const result = await invoke<BatchResult>("batch_generate", {
      fields: backendFields(),
      rows: table.rows,
      output,
    });
    const where =
      output.mode === "files" ? `en ${baseName(String(output.dir))}` : `en ${baseName(String(output.path))}`;
    let message = result.cancelled
      ? output.mode === "files"
        ? `Cancelado: se generaron ${result.generated} de ${table.rows.length} PDFs ${where}.`
        : "Cancelado: no se guardó el archivo."
      : output.mode === "files"
        ? `Listo: ${result.generated} PDFs ${where}.`
        : `Listo: ${result.generated} páginas ${where}.`;
    const replaced = result.fontReplacedRows;
    if (replaced.length) {
      const list = replaced.slice(0, 8).map((r) => r + 1).join(", ") + (replaced.length > 8 ? "…" : "");
      message += ` En ${replaced.length} fila${replaced.length === 1 ? "" : "s"} (${list}) la fuente original no tenía todas las letras y se usó otra parecida.`;
    }
    els.batchResult.textContent = message;
  } catch (e) {
    showError(e);
  } finally {
    unlisten();
    batch.running = false;
    els.batchProgress.hidden = true;
    renderBatchPanel();
  }
}

// ---------- Events ----------

function isTyping(e: KeyboardEvent) {
  const t = e.target as HTMLElement;
  return t.tagName === "TEXTAREA" || (t.tagName === "INPUT" && (t as HTMLInputElement).type === "text");
}

let wheelZoom = 0;

const TOOL_KEYS: Record<string, Tool> = {
  v: "select",
  t: "text",
  x: "edit-text",
  h: "highlight",
  r: "rect",
  p: "pen",
  e: "eraser",
  n: "note",
  o: "redact",
};

window.addEventListener("DOMContentLoaded", () => {
  initTools({
    doc: () => state.doc,
    current: () => state.current,
    openPdf: async (path) => {
      if (state.dirty && !(await confirmDiscard())) return;
      await openPath(path);
    },
    renderPng,
    edit: (command, args) => edit(command, args),
    info: showInfo,
    error: showError,
  });
  els.open.addEventListener("click", openDocument);
  els.openEmpty.addEventListener("click", openDocument);
  els.save.addEventListener("click", () => saveDocument(false));
  els.saveAs.addEventListener("click", () => saveDocument(true));
  els.undo.addEventListener("click", () => history("undo"));
  els.redo.addEventListener("click", () => history("redo"));
  els.prev.addEventListener("click", () => goTo(state.current - 1));
  els.next.addEventListener("click", () => goTo(state.current + 1));
  els.zoomOut.addEventListener("click", () => zoom(-1));
  els.zoomIn.addEventListener("click", () => zoom(1));
  els.rotateLeft.addEventListener("click", () =>
    edit("rotate_page", { index: state.current, clockwise: false }),
  );
  els.rotateRight.addEventListener("click", () =>
    edit("rotate_page", { index: state.current, clockwise: true }),
  );
  els.blank.addEventListener("click", insertBlankPage);
  els.insert.addEventListener("click", insertDocument);
  els.del.addEventListener("click", () => edit("delete_page", { index: state.current }));

  els.tools.addEventListener("click", (e) => {
    const tool = (e.target as HTMLElement).closest<HTMLButtonElement>("button")?.dataset.tool;
    if (tool) setTool(tool as Tool);
  });
  els.color.addEventListener("input", () => {
    if (state.tool in state.colors) state.colors[state.tool] = els.color.value;
    else if (selection && state.tool === "select" && !textInput) scheduleRecolor();
    if (textInput?.dataset.styled !== undefined) {
      textInput.style.color = els.color.value;
      textInput.dataset.color = els.color.value;
    }
  });
  els.color.addEventListener("change", () => textInput?.focus());
  els.size.addEventListener("change", () => {
    state.textSize = Number(els.size.value);
    if (textInput?.dataset.styled !== undefined) {
      textInput.style.fontSize = `${state.textSize * pxPerPt()}px`;
      autosize(textInput);
      textInput.focus();
    }
  });
  els.width.addEventListener("input", () => (state.strokeWidth = Number(els.width.value)));
  els.viewer.addEventListener("scroll", () => requestAnimationFrame(onViewerScroll), { passive: true });
  // ⌘ + wheel (or trackpad pinch) changes the zoom.
  els.viewer.addEventListener(
    "wheel",
    (e) => {
      if (!e.ctrlKey && !e.metaKey) return;
      e.preventDefault();
      wheelZoom += e.deltaY;
      if (Math.abs(wheelZoom) >= 40) {
        zoom(wheelZoom < 0 ? 1 : -1);
        wheelZoom = 0;
      }
    },
    { passive: false },
  );
  els.batch.addEventListener("click", openBatch);
  els.search.addEventListener("click", openSearch);
  els.sign.addEventListener("click", signDocument);
  els.image.addEventListener("click", insertImage);
  els.searchClose.addEventListener("click", closeSearch);
  els.searchPrev.addEventListener("click", () => stepSearch(-1));
  els.searchRedact.addEventListener("click", redactSearchHits);
  els.searchNext.addEventListener("click", () => stepSearch(1));
  els.searchInput.addEventListener("input", () => {
    clearTimeout(searchTimer);
    searchTimer = window.setTimeout(runSearch, 250);
  });
  els.searchInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      stepSearch(e.shiftKey ? -1 : 1);
    } else if (e.key === "Escape") {
      e.preventDefault();
      closeSearch();
    }
  });
  els.batchClose.addEventListener("click", closeBatch);
  els.batchLoad.addEventListener("click", loadBatchTable);
  els.batchRun.addEventListener("click", runBatch);
  els.batchCancel.addEventListener("click", () => invoke("batch_cancel"));
  els.batchPrev.addEventListener("click", () => {
    batch.row = Math.max(0, batch.row - 1);
    onBatchChanged();
  });
  els.batchNext.addEventListener("click", () => {
    batch.row = Math.min((batch.table?.rows.length ?? 1) - 1, batch.row + 1);
    onBatchChanged();
  });
  els.batchPattern.addEventListener("input", () => {
    batch.patternAuto = els.batchPattern.value === "";
  });
  document.querySelectorAll('input[name="batch-mode"]').forEach((r) =>
    r.addEventListener("change", renderBatchPanel),
  );

  window.addEventListener("keydown", (e) => {
    if (document.querySelector("dialog[open]")) return;
    const mod = e.metaKey || e.ctrlKey;
    const key = e.key.toLowerCase();
    if (mod && key === "f") {
      e.preventDefault();
      openSearch();
      return;
    }
    if (isTyping(e)) return;
    if (mod && key === "c" && !window.getSelection()?.toString()) {
      copySelection().then((copied) => copied || undefined);
      return;
    }
    if (mod && key === "o") {
      e.preventDefault();
      openDocument();
    } else if (mod && key === "s") {
      e.preventDefault();
      saveDocument(e.shiftKey);
    } else if (mod && key === "p") {
      e.preventDefault();
      printDocument();
    } else if (mod && key === "z") {
      e.preventDefault();
      history(e.shiftKey ? "redo" : "undo");
    } else if (mod && key === "y") {
      e.preventDefault();
      history("redo");
    } else if (mod && (key === "=" || key === "+")) {
      e.preventDefault();
      zoom(1);
    } else if (mod && key === "-") {
      e.preventDefault();
      zoom(-1);
    } else if (!mod && (key === "arrowright" || key === "pagedown")) {
      goTo(state.current + 1);
    } else if (!mod && (key === "arrowleft" || key === "pageup")) {
      goTo(state.current - 1);
    } else if (batch.active) {
      if (!mod && key === "escape") closeBatch();
    } else if (!mod && (key === "delete" || key === "backspace") && selection) {
      e.preventDefault();
      deleteSelection();
    } else if (!mod && key === "escape") {
      if (textSelection) clearTextSelection();
      else if (selection) clearSelection();
      else if (!els.searchBar.hidden) closeSearch();
      else setTool("select");
    } else if (!mod && !e.altKey && key in TOOL_KEYS) {
      setTool(TOOL_KEYS[key]);
    }
  });

  // Dropping PDFs on the window: the first opens if there's no document, the rest are inserted.
  getCurrentWebview()
    .onDragDropEvent(async (event) => {
      if (event.payload.type !== "drop") return;
      const paths = event.payload.paths.filter(isPdf);
      if (!paths.length) return;
      if (!state.doc) await openPath(paths.shift()!);
      if (state.doc && paths.length) await insertPdfs(paths);
    })
    .catch(() => {});

  // PDFs opened from the system (double click, "Open with…", dropping on the icon).
  const openRequested = async () => {
    const paths = await invoke<string[]>("take_pending_files").catch(() => []);
    if (!paths.length || batch.running) return;
    if (state.dirty && !(await confirmDiscard())) return;
    await openPath(paths[0]);
    if (paths.length > 1 && state.doc) await insertPdfs(paths.slice(1));
  };
  listen("open-files", openRequested).catch(() => {});
  openRequested();

  getCurrentWindow()
    .onCloseRequested(async (event) => {
      if (state.dirty && !(await confirmDiscard())) event.preventDefault();
    })
    .catch(() => {});

  updateUi();
});
