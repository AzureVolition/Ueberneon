/* Hallmark · skills-panel demo interactions
 * primitives: toggle tick · dialog reveal · search-as-you-type
 * reduced-motion handled in CSS */

"use strict";

/* ── demo registry (wire to real data) ── */
const SKILLS = [
  { id: "hallmark", name: "hallmark", cat: "design", ver: "1.1.0", status: "enabled",
    desc: "anti-slop design rules for pages, audits, and redesigns.",
    author: "ueberneon core", source: "built-in", scope: "read-only",
    last: "12 min ago", usage: "24 tasks", builtin: true },
  { id: "pdf", name: "pdf", cat: "documents", ver: "0.8.2", status: "enabled",
    desc: "read, create, and verify pdf documents with visual layout checks.",
    author: "ueberneon core", source: "built-in", scope: "ask",
    last: "4 min ago", usage: "61 tasks", builtin: true },
  { id: "spreadsheets", name: "spreadsheets", cat: "data", ver: "1.2.0", status: "enabled",
    desc: "build and verify xlsx workbooks with formulas and formatting.",
    author: "ueberneon core", source: "built-in", scope: "ask",
    last: "9 min ago", usage: "38 tasks", builtin: true },
  { id: "gmail", name: "gmail", cat: "comms", ver: "2.1.0", status: "enabled",
    desc: "search, summarize, and draft email through a connected mailbox.",
    author: "ueberneon core", source: "built-in", scope: "ask",
    last: "just now", usage: "97 tasks", builtin: true },
  { id: "slack", name: "slack", cat: "comms", ver: "2.3.1", status: "disabled",
    desc: "read channels and prepare messages for slack workflows.",
    author: "ueberneon core", source: "built-in", scope: "ask",
    last: "yesterday", usage: "7 tasks", builtin: true },
  { id: "google-calendar", name: "google-calendar", cat: "comms", ver: "1.6.0", status: "enabled",
    desc: "inspect calendars and draft timezone-aware events.",
    author: "ueberneon core", source: "built-in", scope: "read-only",
    last: "2 h ago", usage: "11 tasks", builtin: true },
  { id: "skill-creator", name: "skill-creator", cat: "automation", ver: "1.0.0", status: "enabled",
    desc: "turn workflows into reusable codex skills.",
    author: "ueberneon core", source: "built-in", scope: "ask",
    last: "36 min ago", usage: "19 tasks", builtin: true },
  { id: "plugin-creator", name: "plugin-creator", cat: "automation", ver: "0.7.0", status: "enabled",
    desc: "scaffold local plugins with valid manifests.",
    author: "ueberneon core", source: "built-in", scope: "ask",
    last: "3 h ago", usage: "4 tasks", builtin: true },
  { id: "template-creator", name: "template-creator", cat: "automation", ver: "0.5.0", status: "available",
    desc: "capture reference artifacts as reusable templates.",
    author: "community", source: "registry", scope: "ask",
    last: "—", usage: "—", builtin: false },
  { id: "visualize", name: "visualize", cat: "data", ver: "0.6.2", status: "available",
    desc: "build interactive simulators, maps, and charts in conversation.",
    author: "community", source: "registry", scope: "ask",
    last: "—", usage: "—", builtin: false },
  { id: "latex", name: "latex", cat: "documents", ver: "0.4.0", status: "available",
    desc: "compile tex projects with tectonic or a local tex live.",
    author: "community", source: "registry", scope: "ask",
    last: "—", usage: "—", builtin: false },
  { id: "presentations", name: "presentations", cat: "documents", ver: "0.9.1", status: "update",
    desc: "create and edit pptx decks with a render-and-verify loop.",
    author: "ueberneon core", source: "built-in", scope: "ask",
    last: "1 h ago", usage: "15 tasks", builtin: true },
  { id: "documents", name: "documents", cat: "documents", ver: "1.0.3", status: "enabled",
    desc: "edit docx files with a strict render and qa workflow.",
    author: "ueberneon core", source: "built-in", scope: "ask",
    last: "5 min ago", usage: "29 tasks", builtin: true }
];

const state = {
  status: "all",
  cat: "all",
  query: "",
  selectedId: "hallmark"
};

const $ = (sel) => document.querySelector(sel);
const listEl = $("#skill-list");
const detailEl = $("#detail");
const emptyEl = $("#empty");
const liveRegion = $("#live-region");

/* ── derived counts ── */
function counts() {
  return {
    all: SKILLS.length,
    installed: SKILLS.filter((s) => s.status !== "available").length,
    available: SKILLS.filter((s) => s.status === "available").length,
    update: SKILLS.filter((s) => s.status === "update").length,
    disabled: SKILLS.filter((s) => s.status === "disabled").length,
    active: SKILLS.filter((s) => s.status === "enabled").length
  };
}

function matches(s) {
  const statusOk = state.status === "all" ||
    (state.status === "installed" ? s.status !== "available" : s.status === state.status);
  const catOk = state.cat === "all" || s.cat === state.cat;
  const q = state.query.trim().toLowerCase();
  const queryOk = !q || s.name.includes(q) || s.desc.includes(q) || (s.cat || "").includes(q);
  return statusOk && catOk && queryOk;
}

/* ── status strip / meter ── */
function renderMeter() {
  const c = counts();
  $("#meter-active").textContent = `active · ${c.active}`;
  $("#meter-updates").textContent = `updates · ${c.update}`;
  const meter = $("#meter");
  meter.innerHTML = "";
  for (let i = 0; i < 48; i++) {
    const tick = document.createElement("span");
    const h = 5 + Math.round(14 * Math.abs(Math.sin(i * 0.42))) + (i % 7 === 0 ? 3 : 0);
    tick.style.height = `${h}px`;
    tick.style.opacity = String(0.35 + 0.4 * Math.abs(Math.sin(i * 0.31)));
    meter.appendChild(tick);
  }
}

/* ── index ── */
function renderList() {
  const c = counts();
  document.querySelectorAll(".chip[data-status]").forEach((chip) => {
    chip.querySelector("[data-count]").textContent = String(c[chip.dataset.status] ?? 0);
    chip.disabled = c[chip.dataset.status] === 0;
    if (chip.disabled && chip.dataset.status === state.status) {
      chip.classList.remove("is-active");
      chip.setAttribute("aria-pressed", "false");
    }
  });
  $("#installed-count").textContent = String(c.installed);

  const filtered = SKILLS.filter(matches);
  listEl.innerHTML = "";

  if (!filtered.length) {
    emptyEl.hidden = false;
    $("#clear-filters").hidden = false;
    return;
  }
  emptyEl.hidden = true;
  $("#clear-filters").hidden = !(state.status !== "all" || state.cat !== "all" || state.query);

  filtered.forEach((s) => {
    const row = document.createElement("li");
    row.className = "skill-row";
    row.dataset.id = s.id;
    row.dataset.status = s.status;
    if (s.id === state.selectedId) row.classList.add("is-selected");

    const statusLabel = s.status === "enabled" ? "enabled" : s.status === "update" ? "needs update" : s.status;
    const meta = s.cat
      ? `<span>${s.cat}</span> · <span>${statusLabel}</span>`
      : `<span>${statusLabel}</span>`;
    row.innerHTML = `
      <button class="skill-row__select" type="button" aria-pressed="${s.id === state.selectedId}">
        <span class="skill-status skill-status--${s.status}" aria-hidden="true"></span>
        <span class="skill-row__body">
          <span class="skill-name">${s.name}</span>
          <span class="skill-meta">${meta}</span>
        </span>
        <span class="skill-version">${s.ver}</span>
        <span class="skill-row__chevron" aria-hidden="true">›</span>
      </button>
      <button class="toggle" role="switch" aria-checked="${s.status === "enabled"}"
              aria-label="toggle ${s.name}" ${s.status === "available" ? "disabled" : ""}>
        <span class="toggle__track"><span class="toggle__thumb"></span></span>
      </button>`;

    row.querySelector(".toggle").addEventListener("click", (e) => {
      e.stopPropagation();
      if (s.status === "available") return;
      const t = e.currentTarget;
      t.dataset.state = "loading";
      setTimeout(() => {
        s.status = s.status === "enabled" ? "disabled" : "enabled";
        delete t.dataset.state;
        renderList();
        if (s.id === state.selectedId) renderDetail(s.id);
        liveRegion.textContent = `${s.name} ${s.status}`;
      }, 280);
    });

    row.querySelector(".skill-row__select").addEventListener("click", () => {
      selectSkill(s.id);
    });
    listEl.appendChild(row);
  });
}

/* ── detail ── */
function renderDetail(id) {
  const s = SKILLS.find((x) => x.id === id);
  if (!s) return;
  const isAvailable = s.status === "available";
  detailEl.innerHTML = `
    <header class="detail__head">
      <p class="mono-label">selected skill</p>
      <h2 class="detail__title">${s.name}</h2>
      <p class="detail__desc">${s.desc}</p>
    </header>

    <dl class="spec">
      <div class="spec__row"><dt>author</dt><dd>${s.author}</dd></div>
      ${s.cat ? `<div class="spec__row"><dt>category</dt><dd>${s.cat}</dd></div>` : ""}
      <div class="spec__row"><dt>version</dt><dd>${s.ver}</dd></div>
      <div class="spec__row"><dt>source</dt><dd>${s.source}</dd></div>
      <div class="spec__row"><dt>permission scope</dt><dd>${s.scope}</dd></div>
      <div class="spec__row"><dt>last run</dt><dd>${s.last}</dd></div>
      <div class="spec__row"><dt>usage</dt><dd>${s.usage}</dd></div>
    </dl>

    ${s.status !== "available" ? `
    <div class="usage">
      <span class="mono-label">usage · ${s.scope}</span>
      <pre class="code">request: ${sampleArgs(s.id)}<span class="caret" aria-hidden="true">▮</span></pre>
    </div>` : `
    <div class="usage">
      <span class="mono-label">install source</span>
      <pre class="code">ueberneon skill add ${s.name}@${s.ver}<span class="caret" aria-hidden="true">▮</span></pre>
    </div>`}

    <div class="detail__actions">
      ${isAvailable ? `
        <button class="btn btn--accent" id="quick-install" type="button">
          <span class="btn__label">install ${s.name}</span>
        </button>` : `
        <div class="action-toggle">
          <span id="toggle-label">${s.status === "enabled" ? "enabled" : "disabled"}</span>
          <button class="toggle" role="switch" aria-checked="${s.status === "enabled"}" aria-label="toggle ${s.name}">
            <span class="toggle__track"><span class="toggle__thumb"></span></span>
          </button>
        </div>
        <button class="btn btn--ghost" id="edit-config" type="button">
          <span class="btn__label">edit config</span>
        </button>
        <button class="btn btn--danger" id="uninstall" type="button" ${s.builtin ? "disabled" : ""}>
          <span class="btn__label">uninstall</span>
        </button>`}
    </div>`;

  wireDetailActions(s);
}

function wireDetailActions(s) {
  const toggle = detailEl.querySelector(".detail__actions .toggle");
  if (toggle) {
    toggle.addEventListener("click", () => {
      toggle.dataset.state = "loading";
      setTimeout(() => {
        s.status = s.status === "enabled" ? "disabled" : "enabled";
        delete toggle.dataset.state;
        renderList();
        renderDetail(s.id);
        liveRegion.textContent = `${s.name} ${s.status}`;
      }, 280);
    });
  }

  const quickInstall = detailEl.querySelector("#quick-install");
  if (quickInstall) {
    quickInstall.addEventListener("click", () => {
      quickInstall.dataset.state = "loading";
      setTimeout(() => {
        s.status = "enabled";
        quickInstall.dataset.state = "success";
        quickInstall.querySelector(".btn__label").textContent = "installed";
        setTimeout(() => {
          renderList();
          renderDetail(s.id);
          liveRegion.textContent = `${s.name} installed`;
        }, 450);
      }, 650);
    });
  }

  const edit = detailEl.querySelector("#edit-config");
  if (edit) {
    edit.addEventListener("click", () => {
      edit.dataset.state = "loading";
      edit.querySelector(".btn__label").textContent = "opening…";
      setTimeout(() => {
        edit.dataset.state = "success";
        edit.querySelector(".btn__label").textContent = "config open";
      }, 600);
    });
  }

  const uninstall = detailEl.querySelector("#uninstall");
  if (uninstall) {
    uninstall.addEventListener("click", () => {
      uninstall.dataset.state = "loading";
      setTimeout(() => {
        const idx = SKILLS.findIndex((x) => x.id === s.id);
        if (idx !== -1) SKILLS.splice(idx, 1);
        const next = SKILLS.find((x) => x.status !== "available") || SKILLS[0];
        state.selectedId = next ? next.id : "";
        renderMeter();
        renderList();
        if (next) renderDetail(next.id);
        liveRegion.textContent = `${s.name} uninstalled`;
      }, 500);
    });
  }
}

function sampleArgs(id) {
  const samples = {
    hallmark: 'audit this page with hallmark',
    pdf: "verify output/contract.pdf",
    spreadsheets: "build design/tokens.xlsx",
    gmail: "triage inbox, last two days",
    "google-calendar": "show tomorrow, tz asia/shanghai",
    "skill-creator": 'capture "export flow" as a skill',
    "plugin-creator": "scaffold my-plugin",
    documents: "redline brief.docx",
    presentations: "render deck.pptx, then qa",
    slack: "draft an update for #dev"
  };
  return samples[id] || "--help";
}

function selectSkill(id) {
  state.selectedId = id;
  renderList();
  renderDetail(id);
}

/* ── filters ── */
document.querySelectorAll(".chip[data-status]").forEach((chip) => {
  chip.addEventListener("click", () => {
    document.querySelectorAll(".chip[data-status]").forEach((c) => {
      c.classList.remove("is-active");
      c.setAttribute("aria-pressed", "false");
    });
    chip.classList.add("is-active");
    chip.setAttribute("aria-pressed", "true");
    state.status = chip.dataset.status;
    renderList();
  });
});

document.querySelectorAll(".chip[data-cat]").forEach((chip) => {
  chip.addEventListener("click", () => {
    document.querySelectorAll(".chip[data-cat]").forEach((c) => {
      c.classList.remove("is-active");
      c.setAttribute("aria-pressed", "false");
    });
    chip.classList.add("is-active");
    chip.setAttribute("aria-pressed", "true");
    state.cat = chip.dataset.cat;
    renderList();
  });
});

function clearFilters() {
  state.status = "all";
  state.cat = "all";
  document.querySelectorAll(".chip[data-status]").forEach((c, i) => {
    c.classList.toggle("is-active", i === 0);
    c.setAttribute("aria-pressed", String(i === 0));
  });
  document.querySelectorAll(".chip[data-cat]").forEach((c, i) => {
    c.classList.toggle("is-active", i === 0);
    c.setAttribute("aria-pressed", String(i === 0));
  });
  renderList();
}

$("#clear-filters").addEventListener("click", clearFilters);
$("#empty-clear").addEventListener("click", clearFilters);

/* ── command palette · N13 ── */
const cmdk = $("#cmdk");
const cmdkInput = $("#cmdk-input");
const cmdkResults = $("#cmdk-results");
let paletteItems = [];
let paletteActive = 0;

const ACTIONS = [
  { label: "install skill from repo", cat: "action" },
  { label: "refresh registry", cat: "action" },
  { label: "open skills folder", cat: "action" }
];

function openPalette() {
  buildPalette("");
  cmdk.showModal();
  cmdkInput.value = "";
  cmdkInput.focus();
}

function buildPalette(q) {
  const query = q.trim().toLowerCase();
  const skillHits = SKILLS.filter((s) => !query || s.name.includes(query) || s.desc.includes(query));
  const actionHits = ACTIONS.filter((a) => !query || a.label.includes(query));
  paletteItems = [];
  cmdkResults.innerHTML = "";

  if (skillHits.length) {
    const group = document.createElement("p");
    group.className = "cmdk__group";
    group.textContent = "skills";
    cmdkResults.appendChild(group);
    skillHits.forEach((s) => {
      const item = document.createElement("button");
      item.className = "cmdk__item";
      item.type = "button";
      item.innerHTML = `<span>${s.name}</span>${s.cat ? `<span class="cmdk__item-cat">${s.cat}</span>` : ""}`;
      item.addEventListener("click", () => {
        cmdk.close();
        selectSkill(s.id);
      });
      paletteItems.push({ el: item, skill: s });
      cmdkResults.appendChild(item);
    });
  }

  if (actionHits.length) {
    const group = document.createElement("p");
    group.className = "cmdk__group";
    group.textContent = "actions";
    cmdkResults.appendChild(group);
    actionHits.forEach((a) => {
      const item = document.createElement("button");
      item.className = "cmdk__item";
      item.type = "button";
      item.innerHTML = `<span>${a.label}</span><span class="cmdk__item-cat">${a.cat}</span>`;
      item.addEventListener("click", () => {
        cmdk.close();
        if (a.label.startsWith("install")) $("#install-btn").click();
      });
      paletteItems.push({ el: item, skill: null });
      cmdkResults.appendChild(item);
    });
  }

  if (!paletteItems.length) {
    const empty = document.createElement("p");
    empty.className = "cmdk__group";
    empty.textContent = "no matches";
    cmdkResults.appendChild(empty);
  }
  setActive(0);
}

function setActive(i) {
  paletteActive = (i + paletteItems.length) % paletteItems.length;
  paletteItems.forEach((p, idx) => {
    p.el.classList.toggle("is-active", idx === paletteActive);
    p.el.setAttribute("aria-selected", String(idx === paletteActive));
  });
}

$("#searchpill").addEventListener("click", openPalette);

document.addEventListener("keydown", (e) => {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
    e.preventDefault();
    cmdk.open ? cmdk.close() : openPalette();
  }
});

cmdkInput.addEventListener("input", () => {
  buildPalette(cmdkInput.value);
});

cmdkInput.addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown") {
    e.preventDefault();
    setActive(paletteActive + 1);
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    setActive(paletteActive - 1);
  } else if (e.key === "Enter") {
    e.preventDefault();
    const item = paletteItems[paletteActive];
    if (!item) return;
    cmdk.close();
    if (item.skill) selectSkill(item.skill.id);
    else $("#install-btn").click();
  }
});

/* ── install dialog ── */
const installDialog = $("#install-dialog");
const installForm = $("#install-form");
const installInput = $("#install-input");
const installField = $("#install-field");
const installHelper = $("#install-helper");
const installError = $("#install-error");
const installSubmit = $("#install-submit");
const installCancel = $("#install-cancel");

$("#install-btn").addEventListener("click", () => {
  installField.classList.remove("is-error");
  installHelper.hidden = false;
  installError.hidden = true;
  installInput.setAttribute("aria-invalid", "false");
  installDialog.showModal();
  installInput.focus();
});

installCancel.addEventListener("click", () => installDialog.close());

installForm.addEventListener("submit", (e) => {
  e.preventDefault();
  const value = installInput.value.trim();
  if (!value) {
    installField.classList.add("is-error");
    installHelper.hidden = true;
    installError.hidden = false;
    installInput.setAttribute("aria-invalid", "true");
    return;
  }
  installField.classList.remove("is-error");
  installHelper.hidden = false;
  installError.hidden = true;
  installInput.setAttribute("aria-invalid", "false");

  installSubmit.dataset.state = "loading";
  installSubmit.disabled = true;
  installInput.disabled = true;
  installField.classList.add("is-loading");

  setTimeout(() => {
    const name = value.split("/").pop().replace(/\.git$/, "").replace(/[^a-z0-9_]/gi, "_").toLowerCase() || "imported_skill";
    SKILLS.push({
      id: name,
      name,
      cat: null,
      ver: "0.1.0",
      status: "enabled",
      desc: `installed from ${value}.`,
      author: "you",
      source: "registry",
      scope: "ask",
      last: "just now",
      usage: "0 tasks",
      builtin: false
    });
    installSubmit.dataset.state = "success";
    installSubmit.querySelector(".btn__label").textContent = "installed";
    setTimeout(() => {
      installDialog.close();
      installSubmit.dataset.state = "";
      installSubmit.querySelector(".btn__label").textContent = "install";
      installSubmit.disabled = false;
      installInput.disabled = false;
      installField.classList.remove("is-loading");
      installInput.value = "";
      state.status = "all";
      state.cat = "all";
      renderMeter();
      selectSkill(name);
      liveRegion.textContent = `${name} installed`;
    }, 500);
  }, 700);
});

/* ── boot ── */
renderMeter();
renderList();
renderDetail(state.selectedId);
