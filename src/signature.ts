/**
 * "Sign" window: draw, type or upload a signature, and pick from the saved ones.
 * Returns the signature as a PNG with a transparent background, cropped to what was drawn.
 */

export interface DataSource {
  kind: "data";
  bytes: number[];
}

const STORAGE_KEY = "pliego.signatures";
const MAX_SAVED = 8;
/** Common handwriting fonts on macOS and Windows; only the installed ones are offered. */
const SCRIPT_FONTS = [
  "Snell Roundhand",
  "Apple Chancery",
  "Bradley Hand",
  "Noteworthy",
  "Segoe Script",
  "Lucida Handwriting",
  "Ink Free",
  "Brush Script MT",
];

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const els = {
  dialog: $<HTMLDialogElement>("sign-dialog"),
  savedSection: $("sign-saved-section"),
  saved: $("sign-saved"),
  tabs: $("sign-tabs"),
  draw: $<HTMLCanvasElement>("sign-canvas"),
  ink: $<HTMLSelectElement>("sign-ink"),
  clear: $<HTMLButtonElement>("sign-clear"),
  name: $<HTMLInputElement>("sign-name"),
  font: $<HTMLSelectElement>("sign-font"),
  typed: $<HTMLCanvasElement>("sign-typed"),
  pick: $<HTMLButtonElement>("sign-pick"),
  uploaded: $<HTMLCanvasElement>("sign-uploaded"),
  remember: $<HTMLInputElement>("sign-remember"),
  use: $<HTMLButtonElement>("sign-use"),
};

type Tab = "draw" | "type" | "file";
let tab: Tab = "draw";
let drawn = false;
let uploaded = false;
let resolve: ((source: DataSource | null) => void) | null = null;

function loadSaved(): string[] {
  try {
    const list = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "[]");
    return Array.isArray(list) ? list.filter((s) => typeof s === "string") : [];
  } catch {
    return [];
  }
}

function storeSaved(list: string[]) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(list.slice(0, MAX_SAVED)));
  } catch {
    // Without storage the signature is still used; it just isn't saved.
  }
}

function toSource(dataUrl: string): DataSource {
  const binary = atob(dataUrl.split(",")[1]);
  return { kind: "data", bytes: Array.from(binary, (c) => c.charCodeAt(0)) };
}

/** PNG cropped to the visible pixels (with a small margin); null if empty. */
function trimmed(canvas: HTMLCanvasElement): string | null {
  const g = canvas.getContext("2d")!;
  const { width, height } = canvas;
  const data = g.getImageData(0, 0, width, height).data;
  let [left, top, right, bottom] = [width, height, -1, -1];
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      if (data[(y * width + x) * 4 + 3] > 8) {
        left = Math.min(left, x);
        right = Math.max(right, x);
        top = Math.min(top, y);
        bottom = Math.max(bottom, y);
      }
    }
  }
  if (right < 0) return null;
  const pad = 6;
  left = Math.max(0, left - pad);
  top = Math.max(0, top - pad);
  right = Math.min(width - 1, right + pad);
  bottom = Math.min(height - 1, bottom + pad);
  const out = document.createElement("canvas");
  out.width = right - left + 1;
  out.height = bottom - top + 1;
  out.getContext("2d")!.drawImage(canvas, left, top, out.width, out.height, 0, 0, out.width, out.height);
  return out.toDataURL("image/png");
}

function current(): HTMLCanvasElement | null {
  if (tab === "draw") return drawn ? els.draw : null;
  if (tab === "type") return els.name.value.trim() ? els.typed : null;
  return uploaded ? els.uploaded : null;
}

function refreshUse() {
  els.use.disabled = current() === null;
}

function setTab(next: Tab) {
  tab = next;
  els.tabs.querySelectorAll<HTMLButtonElement>("button").forEach((b) => {
    b.setAttribute("aria-pressed", String(b.dataset.tab === next));
  });
  els.dialog.querySelectorAll<HTMLElement>("[data-panel]").forEach((p) => {
    p.hidden = p.dataset.panel !== next;
  });
  if (next === "type") els.name.focus();
  refreshUse();
}

function renderSaved() {
  const list = loadSaved();
  els.savedSection.hidden = list.length === 0;
  els.saved.replaceChildren(
    ...list.map((url, i) => {
      const item = document.createElement("button");
      item.type = "button";
      item.className = "saved";
      item.title = "Usar esta firma";
      const img = document.createElement("img");
      img.src = url;
      img.alt = "Firma guardada";
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "remove";
      remove.title = "Olvidar esta firma";
      remove.textContent = "×";
      remove.addEventListener("click", (e) => {
        e.stopPropagation();
        storeSaved(loadSaved().filter((_, j) => j !== i));
        renderSaved();
      });
      item.addEventListener("click", () => finish(url, false));
      item.append(img, remove);
      return item;
    }),
  );
}

function finish(dataUrl: string | null, remember: boolean) {
  if (dataUrl && remember) {
    storeSaved([dataUrl, ...loadSaved().filter((u) => u !== dataUrl)]);
  }
  const done = resolve;
  resolve = null;
  els.dialog.close();
  done?.(dataUrl ? toSource(dataUrl) : null);
}

// ---------- Draw ----------

function setupDrawing() {
  const g = els.draw.getContext("2d")!;
  g.lineCap = "round";
  g.lineJoin = "round";
  let last: { x: number; y: number } | null = null;
  let mid: { x: number; y: number } | null = null;

  const point = (e: PointerEvent) => {
    const r = els.draw.getBoundingClientRect();
    return {
      x: ((e.clientX - r.left) / r.width) * els.draw.width,
      y: ((e.clientY - r.top) / r.height) * els.draw.height,
    };
  };

  els.draw.addEventListener("pointerdown", (e) => {
    els.draw.setPointerCapture(e.pointerId);
    last = point(e);
    mid = last;
    g.strokeStyle = els.ink.value;
    g.fillStyle = els.ink.value;
    // A tap leaves a dot.
    g.beginPath();
    g.arc(last.x, last.y, 2.5, 0, Math.PI * 2);
    g.fill();
    drawn = true;
    refreshUse();
  });
  els.draw.addEventListener("pointermove", (e) => {
    if (!last || !mid) return;
    const p = point(e);
    // Curves between midpoints for a smooth stroke; thinner when moving fast.
    const next = { x: (last.x + p.x) / 2, y: (last.y + p.y) / 2 };
    const speed = Math.hypot(p.x - last.x, p.y - last.y);
    g.lineWidth = Math.max(3, 6.5 - speed / 12);
    g.beginPath();
    g.moveTo(mid.x, mid.y);
    g.quadraticCurveTo(last.x, last.y, next.x, next.y);
    g.stroke();
    last = p;
    mid = next;
  });
  const end = () => {
    last = null;
    mid = null;
  };
  els.draw.addEventListener("pointerup", end);
  els.draw.addEventListener("pointercancel", end);
  els.clear.addEventListener("click", () => {
    g.clearRect(0, 0, els.draw.width, els.draw.height);
    drawn = false;
    refreshUse();
  });
}

// ---------- Type ----------

function fontInstalled(family: string) {
  const g = document.createElement("canvas").getContext("2d")!;
  const sample = "Firma manuscrita 123";
  g.font = "40px monospace";
  const base = g.measureText(sample).width;
  g.font = `40px "${family}", monospace`;
  return g.measureText(sample).width !== base;
}

function setupTyping() {
  const fonts = SCRIPT_FONTS.filter(fontInstalled);
  for (const f of fonts.length ? fonts : ["cursive"]) els.font.append(new Option(f, f));
  const draw = () => {
    const g = els.typed.getContext("2d")!;
    const { width, height } = els.typed;
    g.clearRect(0, 0, width, height);
    const text = els.name.value.trim();
    if (text) {
      const family = els.font.value === "cursive" ? "cursive" : `"${els.font.value}"`;
      let size = 120;
      g.font = `${size}px ${family}`;
      // Shrinks until it fits the width.
      while (size > 30 && g.measureText(text).width > width - 60) {
        size -= 6;
        g.font = `${size}px ${family}`;
      }
      g.fillStyle = els.ink.value;
      g.textAlign = "center";
      g.textBaseline = "middle";
      g.fillText(text, width / 2, height / 2);
    }
    refreshUse();
  };
  els.name.addEventListener("input", draw);
  els.font.addEventListener("change", draw);
  els.ink.addEventListener("change", draw);
}

// ---------- Upload ----------

/** Draws the image and makes the light background transparent (scanned or photographed paper). */
function loadUploaded(file: File) {
  const url = URL.createObjectURL(file);
  const img = new Image();
  img.onload = () => {
    URL.revokeObjectURL(url);
    const c = els.uploaded;
    const g = c.getContext("2d")!;
    g.clearRect(0, 0, c.width, c.height);
    const scale = Math.min(c.width / img.width, c.height / img.height, 1);
    const w = img.width * scale;
    const h = img.height * scale;
    g.drawImage(img, (c.width - w) / 2, (c.height - h) / 2, w, h);
    const pixels = g.getImageData(0, 0, c.width, c.height);
    const d = pixels.data;
    for (let i = 0; i < d.length; i += 4) {
      const light = 0.299 * d[i] + 0.587 * d[i + 1] + 0.114 * d[i + 2];
      // Light → transparent, dark → opaque, with a ramp for soft edges.
      const alpha = Math.max(0, Math.min(1, (215 - light) / 60));
      d[i + 3] = Math.round(d[i + 3] * alpha);
    }
    g.putImageData(pixels, 0, 0);
    uploaded = true;
    refreshUse();
  };
  img.onerror = () => URL.revokeObjectURL(url);
  img.src = url;
}

function setupUpload() {
  const input = document.createElement("input");
  input.type = "file";
  input.accept = "image/png,image/jpeg,image/webp";
  input.addEventListener("change", () => {
    if (input.files?.[0]) loadUploaded(input.files[0]);
    input.value = "";
  });
  els.pick.addEventListener("click", () => input.click());
}

let ready = false;

/** Opens the window and waits for a signature to be chosen (or null if closed). */
export function chooseSignature(): Promise<DataSource | null> {
  if (!ready) {
    ready = true;
    setupDrawing();
    setupTyping();
    setupUpload();
    els.tabs.addEventListener("click", (e) => {
      const next = (e.target as HTMLElement).closest<HTMLButtonElement>("button")?.dataset.tab;
      if (next) setTab(next as Tab);
    });
    els.use.addEventListener("click", () => {
      const canvas = current();
      const url = canvas && trimmed(canvas);
      if (url) finish(url, els.remember.checked);
    });
    els.dialog.addEventListener("close", () => {
      if (resolve) finish(null, false);
    });
  }
  renderSaved();
  setTab(loadSaved().length ? tab : "draw");
  els.dialog.showModal();
  return new Promise((r) => (resolve = r));
}
