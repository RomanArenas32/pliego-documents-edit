/**
 * Touch layout for phones (Android): the drawing tools go to a bar at the bottom, the page
 * list becomes a drawer, one finger scrolls (or draws, with a drawing tool) and two fingers
 * zoom. The desktop markup is reused: elements are moved, not duplicated, so their
 * listeners keep working.
 */

export interface MobileContext {
  zoom(): number;
  /** Zooms keeping the page point under `anchor` and moving it to `to`. */
  setZoom(value: number, anchor: Point, to: Point): void;
  /** Abandons a drag in progress (a second finger means zoom, not draw). */
  cancelDrag(): void;
  minZoom: number;
  maxZoom: number;
  /** Back button: closes what's open; false when there's nothing to close and the app may leave. */
  onBack(): Promise<boolean>;
  /** Sends the app to the background, like leaving any other app. */
  leave(): void;
}

interface Point {
  x: number;
  y: number;
}

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

export function initMobile(ctx: MobileContext) {
  document.documentElement.classList.add("mobile");
  // "Herramientas ▾" doesn't fit next to the other buttons on a phone.
  const tools = $("btn-tools");
  tools.textContent = "⋯";
  tools.classList.add("icon");
  // Nothing can be dragged onto the app on a phone.
  document.querySelector("#empty p")!.textContent = "Abre un PDF para empezar";
  buildBottomBar();
  const drawer = buildPageDrawer();
  enablePinch(ctx);
  handleBackButton(ctx, drawer);
  // The keyboard shrinks the window: keep the text being typed in sight.
  window.addEventListener("resize", () => {
    const active = document.activeElement;
    if (active instanceof HTMLElement && active.closest("#viewer")) active.scrollIntoView({ block: "nearest" });
  });
}

/**
 * Android's back button: MainActivity.kt calls this instead of closing the app, which would
 * lose unsaved changes.
 */
function handleBackButton(ctx: MobileContext, drawer: { close(): boolean }) {
  (window as unknown as { __pliegoBack: () => void }).__pliegoBack = async () => {
    if (drawer.close()) return;
    if (!(await ctx.onBack())) ctx.leave();
  };
}

/** Drawing tools, signature and image at the bottom, where the thumb reaches. */
function buildBottomBar() {
  const bar = document.createElement("nav");
  bar.className = "bottom-bar";
  bar.setAttribute("aria-label", "Herramientas de dibujo");
  bar.append($("tools"), $("btn-sign").parentElement!);
  // Color, size and thickness float just above the bar.
  const options = $("tool-options");
  options.classList.add("floating");
  document.body.append(options, bar);
}

/** Thumbnails and page actions (rotate, insert, delete) slide in from the left. */
function buildPageDrawer(): { close(): boolean } {
  const drawer = document.createElement("div");
  drawer.className = "page-drawer";
  drawer.append($("btn-rotate-left").parentElement!, $("thumbnails"));
  const workspace = document.querySelector(".workspace")!;
  workspace.prepend(drawer);

  const toggle = document.createElement("button");
  toggle.id = "btn-pages";
  toggle.className = "icon";
  toggle.title = "Páginas";
  toggle.setAttribute("aria-expanded", "false");
  toggle.textContent = "☰";
  const firstGroup = $("btn-open").parentElement!;
  firstGroup.prepend(toggle);

  const setOpen = (open: boolean) => {
    document.body.classList.toggle("drawer-open", open);
    toggle.setAttribute("aria-expanded", String(open));
  };
  toggle.addEventListener("click", () => setOpen(!document.body.classList.contains("drawer-open")));
  // Choosing a page, or touching outside the drawer, closes it.
  $("thumbnails").addEventListener("click", (e) => {
    if ((e.target as HTMLElement).closest(".thumb")) setOpen(false);
  });
  workspace.addEventListener("pointerdown", (e) => {
    if (document.body.classList.contains("drawer-open") && !drawer.contains(e.target as Node)) setOpen(false);
  });
  return {
    /** Closes the drawer; false if it was already closed. */
    close() {
      if (!document.body.classList.contains("drawer-open")) return false;
      setOpen(false);
      return true;
    },
  };
}

/**
 * Two fingers zoom. While pinching, the pages are only scaled with CSS (instant); when the
 * fingers lift, the real zoom is applied and the pages are redrawn sharp.
 */
function enablePinch(ctx: MobileContext) {
  const viewer = $("viewer");
  const pages = $("pages");
  let pinch: { distance: number; start: Point; mid: Point; scale: number } | null = null;

  const distance = (t: TouchList) => Math.hypot(t[0].clientX - t[1].clientX, t[0].clientY - t[1].clientY);
  const middle = (t: TouchList) => ({ x: (t[0].clientX + t[1].clientX) / 2, y: (t[0].clientY + t[1].clientY) / 2 });

  viewer.addEventListener(
    "touchstart",
    (e) => {
      if (e.touches.length !== 2 || pages.hidden) return;
      ctx.cancelDrag();
      const mid = middle(e.touches);
      const box = pages.getBoundingClientRect();
      pages.style.transformOrigin = `${mid.x - box.left}px ${mid.y - box.top}px`;
      pinch = { distance: distance(e.touches), start: mid, mid, scale: 1 };
    },
    { passive: true },
  );

  viewer.addEventListener(
    "touchmove",
    (e) => {
      if (!pinch || e.touches.length !== 2) return;
      // Without this the browser would also scroll with the two fingers.
      if (e.cancelable) e.preventDefault();
      const zoom = ctx.zoom();
      const scale = Math.max(ctx.minZoom / zoom, Math.min(distance(e.touches) / pinch.distance, ctx.maxZoom / zoom));
      pinch.mid = middle(e.touches);
      pinch.scale = scale;
      const dx = pinch.mid.x - pinch.start.x;
      const dy = pinch.mid.y - pinch.start.y;
      pages.style.transform = `translate(${dx}px, ${dy}px) scale(${scale})`;
    },
    { passive: false },
  );

  const end = (e: TouchEvent) => {
    if (!pinch || e.touches.length >= 2) return;
    const { scale, start, mid } = pinch;
    pinch = null;
    pages.style.transform = "";
    if (Math.abs(scale - 1) > 0.01 || Math.hypot(mid.x - start.x, mid.y - start.y) > 1) {
      ctx.setZoom(ctx.zoom() * scale, start, mid);
    }
  };
  viewer.addEventListener("touchend", end);
  viewer.addEventListener("touchcancel", end);
}
