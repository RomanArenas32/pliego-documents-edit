import { listen } from "@tauri-apps/api/event";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

/**
 * Runs a long command showing "<label> página 3 de 15…" and a bar, from the `event` the
 * command emits after each page. Shown after a moment, so quick runs don't flash it.
 */
export async function withProgress<T>(event: string, label: string, task: () => Promise<T>): Promise<T> {
  const dialog = $<HTMLDialogElement>("progress-dialog");
  const text = $("progress-label");
  const bar = $<HTMLProgressElement>("progress-bar");
  text.textContent = `${label}…`;
  bar.removeAttribute("value");
  // It can't be closed: the task keeps running anyway.
  const keepOpen = (e: Event) => e.preventDefault();
  dialog.addEventListener("cancel", keepOpen);
  const timer = window.setTimeout(() => dialog.showModal(), 300);
  const unlisten = await listen<{ done: number; total: number }>(event, ({ payload }) => {
    text.textContent = `${label} página ${payload.done} de ${payload.total}…`;
    bar.max = payload.total;
    bar.value = payload.done;
  });
  document.body.classList.add("working");
  try {
    return await task();
  } finally {
    clearTimeout(timer);
    unlisten();
    dialog.removeEventListener("cancel", keepOpen);
    if (dialog.open) dialog.close();
    document.body.classList.remove("working");
  }
}
