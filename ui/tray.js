// The tray menu window. A socket for the one
// menu the tray thread derives: `sotone://tray-items` carries the same
// `Vec<TrayItem>` the native menu used to be built from, serialized; this
// page renders it and does nothing else on its own authority.
//
// The contract with tray.rs, in one place:
//
// - `sotone://tray-items` `{ glyph, items }` — the menu, whenever it changes.
//   Arrives while hidden too; the latest payload is what an open shows.
// - `sotone://tray-open` — the user clicked the icon. Render, measure, answer
//   with `tray_menu_ready { width, height }` (logical px); the tray thread
//   sizes, places, shows and focuses the window.
// - a click on an actionable row → `tray_menu_action { id }`, then close.
//   The ids are tray.rs's own (`sotone:tray:*`); this file never invents one.
// - losing focus, Escape, or any action → the 140ms out animation, then
//   `tray_menu_closed`; the tray thread hides the window. Focus is the whole
//   dismissal story — this window is focusable precisely so blur can close
//   it, the same deal the native menu had.
//
// Invariants: nothing here generates input (1), asks for focus (2 — the tray
// thread gives it focus, the page only ever gives it back by closing), or
// touches the network (3).

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const EVENT = {
  items: "sotone://tray-items",
  open: "sotone://tray-open",
};

const menu = document.getElementById("menu");

// The latest payload, rendered or not: an open must never show a stale menu,
// so the render always draws from here.
let latest = { glyph: "dim", items: [] };
// Whether the window believes it is on screen. Gates the re-measure on item
// updates (a hidden menu has nothing to re-place) and makes close idempotent
// (blur fires once from the user and once from our own hide).
let open = false;
// The height last reported, so a clock tick that changes no geometry does not
// spam the tray thread with placements.
let reported = 0;

// --------------------------------------------------------------------------
// Rendering
// --------------------------------------------------------------------------

/* One row. `kind` decides everything; ids arrive only on actionable rows. */
function row(item, quiet) {
  switch (item.kind) {
    case "status": {
      const div = document.createElement("div");
      div.className = "tm-status";
      const glyph = document.createElement("span");
      glyph.className = "tm-glyph";
      glyph.setAttribute("aria-hidden", "true");
      for (let i = 0; i < 4; i += 1) glyph.appendChild(document.createElement("i"));
      const text = document.createElement("span");
      text.textContent = item.label;
      div.append(glyph, text);
      return div;
    }
    case "hint": {
      const div = document.createElement("div");
      div.className = "tm-hint";
      div.textContent = item.label;
      return div;
    }
    case "separator": {
      const div = document.createElement("div");
      div.className = "tm-sep";
      return div;
    }
    case "section": {
      const div = document.createElement("div");
      div.className = "tm-section";
      div.textContent = item.label;
      return div;
    }
    case "writing": {
      const div = document.createElement("div");
      div.className = "tm-writing";
      const label = document.createElement("span");
      label.className = "tm-item__label";
      label.textContent = item.label;
      div.appendChild(label);
      if (item.lines != null) {
        const count = document.createElement("span");
        count.className = "tm-item__count";
        count.textContent = String(item.lines);
        div.appendChild(count);
      }
      return div;
    }
    // toggle · note · open · settings · quit — the actionable rows share one
    // shape: a button whose id is the tray thread's own vocabulary.
    default: {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "tm-item";
      button.setAttribute("role", "menuitem");
      button.dataset.id = item.id;
      if (item.enabled === false) button.disabled = true;
      if (quiet) button.dataset.quiet = "yes";
      const label = document.createElement("span");
      label.className = "tm-item__label";
      label.textContent = item.label;
      button.appendChild(label);
      if (item.lines != null) {
        const count = document.createElement("span");
        count.className = "tm-item__count";
        count.textContent = String(item.lines);
        button.appendChild(count);
      }
      return button;
    }
  }
}

function render() {
  menu.replaceChildren();
  menu.dataset.glyph = latest.glyph;
  // The mock brightens only the newest recent; the rest are --muted until
  // hovered. Presentation, so it lives here rather than in the payload.
  let notesSeen = 0;
  for (const item of latest.items) {
    const quiet = item.kind === "note" && notesSeen > 0;
    if (item.kind === "note") notesSeen += 1;
    menu.appendChild(row(item, quiet));
  }
}

/* Measure after render and tell the tray thread, which sizes and places the
   window before it shows (the dropdown clamp pattern, in window form). Skipped
   when nothing changed: the recording clock ticks once a second. */
function report() {
  const height = menu.offsetHeight;
  if (height === reported) return;
  reported = height;
  invoke("tray_menu_ready", { width: menu.offsetWidth, height }).catch(() => {
    // The tray thread logs its own failures; a menu that cannot be placed
    // stays hidden, which is the honest outcome.
  });
}

// --------------------------------------------------------------------------
// Open and close
// --------------------------------------------------------------------------

/* The close is animated, so it is asynchronous; `open` flips first so a blur
   fired by our own hide cannot re-enter. */
function close() {
  if (!open) return;
  open = false;
  reported = 0;
  let finished = false;
  const finish = () => {
    if (finished) return;
    finished = true;
    menu.dataset.motion = "";
    invoke("tray_menu_closed").catch(() => {});
  };
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (reduced) {
    finish();
    return;
  }
  menu.dataset.motion = "out";
  // `animationend` when the 140ms in tray.css really ran — with a timeout
  // racing it, because an animation that never runs (a hidden window does
  // not composite) must never leave the menu unclosable.
  menu.addEventListener("animationend", finish, { once: true });
  setTimeout(finish, 220);
}

// A rejected listen() is a build whose capability file forgot this window —
// the page would look alive and never hear an open. Loud,
// because silence here cost a shipped build.
const deaf = (err) =>
  console.error("tray menu cannot listen — capability file?", err);

listen(EVENT.items, (event) => {
  latest = event.payload;
  render();
  if (open) report();
}).catch(deaf);

listen(EVENT.open, () => {
  open = true;
  reported = 0;
  render();
  menu.dataset.motion = "in";
  report();
}).catch(deaf);

menu.addEventListener("click", (event) => {
  const button = event.target.closest(".tm-item");
  if (!button || button.disabled) return;
  invoke("tray_menu_action", { id: button.dataset.id }).catch(() => {});
  close();
});

window.addEventListener("blur", close);

window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") close();
});
