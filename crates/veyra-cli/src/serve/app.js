"use strict";

// ---- token: arrives in the URL fragment, never in a request ----
let token = "";
// Returns true when the fragment carried a (new) token.
function takeToken() {
  const m = location.hash.match(/token=([0-9a-f]+)/);
  if (m) {
    token = m[1];
    try { sessionStorage.setItem("veyra-token", token); } catch (_) {}
    history.replaceState(null, "", location.pathname + "#/history");
    return true;
  }
  if (!token) {
    try { token = sessionStorage.getItem("veyra-token") || ""; } catch (_) {}
  }
  return false;
}
takeToken();

class TokenError extends Error {}

async function api(path) {
  const res = await fetch("/api/" + path, { headers: { "X-Veyra-Token": token } });
  if (res.status === 403) throw new TokenError("This viewer link has expired.");
  const type = res.headers.get("content-type") || "";
  const body = type.includes("json") ? await res.json() : await res.text();
  if (!res.ok) throw new Error((body && body.error) || res.statusText);
  return body;
}

// ---- tiny DOM helper: text only, never innerHTML ----
function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

function ago(seconds) {
  const d = Math.max(0, Date.now() / 1000 - seconds);
  if (d < 45) return "just now";
  const units = [[60, "minute"], [3600, "hour"], [86400, "day"], [2592000, "month"], [31536000, "year"]];
  let pick = units[0];
  for (const u of units) if (d >= u[0] * 0.75) pick = u;
  const n = Math.round(d / pick[0]);
  return `${n} ${pick[1]}${n === 1 ? "" : "s"} ago`;
}
const fullDate = (s) => new Date(s * 1000).toLocaleString();

function copyButton(text) {
  return h("button", {
    class: "copy", type: "button", "aria-label": `Copy ${text}`,
    onclick: async (e) => {
      try { await navigator.clipboard.writeText(text); e.target.textContent = "Copied"; }
      catch (_) { e.target.textContent = "Copy failed"; }
      setTimeout(() => { e.target.textContent = "Copy"; }, 1500);
    },
  }, "Copy");
}
const command = (text) => h("span", {}, h("code", { class: "cmd" }, text), copyButton(text));

const main = document.getElementById("main");
function show(...nodes) { main.replaceChildren(...nodes); }
function needLink() {
  return h("div", { class: "page" }, h("h2", {}, "Open the viewer from the terminal"),
    h("p", { class: "lede" }, "This page needs the address veyra serve printed, which includes a one-time key. If you restarted veyra serve, open the new link it shows."));
}
function failure(err) {
  if (err instanceof TokenError) return needLink();
  return h("div", { class: "page" }, h("p", { class: "error" }, err.message || String(err)));
}

// ---- sidebar ----
async function loadSummary() {
  try {
    const s = await api("summary");
    document.title = `${s.name}: Veyra viewer`;
    document.getElementById("repo-name").textContent = s.name;
    document.getElementById("repo-branch").textContent = s.branch ? s.branch : (s.head ? `detached at ${s.head.short}` : "no commits yet");
    const st = s.status;
    const items = [];
    if (s.in_progress) items.push(h("li", { class: "alert" }, `${s.in_progress} in progress`));
    if (st.conflicted) items.push(h("li", { class: "alert" }, `${st.conflicted} conflicted`));
    if (st.staged) items.push(h("li", {}, `${st.staged} staged`));
    if (st.modified) items.push(h("li", {}, `${st.modified} changed`));
    if (st.untracked) items.push(h("li", {}, `${st.untracked} untracked`));
    if (!items.length) items.push(h("li", {}, "Working tree clean"));
    document.getElementById("repo-status").replaceChildren(...items);
    document.getElementById("side-foot").replaceChildren(
      `Veyra ${s.version}`, h("br"),
      s.signing ? "Signing on" : "Signing off", h("br"),
      `AI: ${s.ai}`,
    );
    return s;
  } catch (e) {
    document.getElementById("repo-name").textContent = e instanceof TokenError ? "Link expired" : "Can't reach Veyra";
    document.getElementById("repo-status").replaceChildren(h("li", { class: "alert" }, e instanceof TokenError ? "Open the new link from veyra serve" : e.message));
  }
}

// ---- history ----
const historyState = { commits: [], more: false, selected: null };

function marks(c) {
  return h("span", { class: "marks" },
    c.assisted && c.assisted.length ? h("span", { class: "mark ai", title: `Assisted by ${c.assisted.join(", ")}` }, "AI-assisted") : null,
    c.intents && c.intents.length ? h("span", { class: "mark intent", title: c.intents.join(", ") }, "intent") : null,
    c.structural && c.structural.length ? h("span", { class: "mark structural", title: c.structural.join(", ") }, "auto-merged") : null,
    c.attested ? h("span", { class: "mark", title: "Has attestations" }, "attested") : null,
    c.noted ? h("span", { class: "mark", title: "Has a note" }, "note") : null,
  );
}
function refChips(refs) {
  if (!refs || !refs.length) return null;
  return h("span", { class: "refs" }, refs.map((r) => h("span", { class: `ref ${r.kind}` }, r.kind === "tag" ? `tag ${r.name}` : r.name)));
}

async function viewHistory(selectId) {
  if (!historyState.commits.length) {
    const page = await api("log?limit=100");
    historyState.commits = page.commits;
    historyState.more = page.more;
  }
  const list = h("ul", { class: "commits", role: "listbox", "aria-label": "Commits" });
  const detail = h("div", { class: "pane", id: "detail" });
  const renderRows = () => {
    list.replaceChildren(...historyState.commits.map((c) => h("li", {
      class: "commit-row", role: "option", tabindex: "0", "data-id": c.id,
      "aria-selected": c.id === historyState.selected ? "true" : "false",
      onclick: () => select(c.id),
      onkeydown: (e) => {
        if (e.key === "Enter") select(c.id);
        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          e.preventDefault();
          const sib = e.key === "ArrowDown" ? e.currentTarget.nextElementSibling : e.currentTarget.previousElementSibling;
          if (sib) { sib.focus(); select(sib.dataset.id); }
        }
      },
    },
      h("div", { class: `lane${c.parents.length > 1 ? " merge" : ""}`, "aria-hidden": "true" }),
      h("div", {},
        h("div", { class: "commit-summary" }, refChips(c.refs), c.summary),
        h("div", { class: "commit-meta" }, h("code", {}, c.short), h("span", {}, c.author), h("span", { title: fullDate(c.time) }, ago(c.time)), marks(c)),
      ),
    )));
  };
  const select = async (id) => {
    historyState.selected = id;
    for (const row of list.children) row.setAttribute("aria-selected", row.dataset.id === id ? "true" : "false");
    detail.replaceChildren(h("div", { class: "page" }, h("p", { class: "empty" }, "Loading…")));
    try { detail.replaceChildren(renderCommit(await api(`commit/${id}`))); }
    catch (e) { detail.replaceChildren(failure(e)); }
  };
  const moreButton = h("button", {
    class: "more", type: "button",
    onclick: async () => {
      const page = await api(`log?skip=${historyState.commits.length}&limit=100`);
      historyState.commits.push(...page.commits);
      historyState.more = page.more;
      renderRows();
      moreButton.hidden = !historyState.more;
    },
  }, "Load older commits");
  moreButton.hidden = !historyState.more;
  renderRows();
  show(h("div", { class: "split" },
    h("div", { class: "pane" }, h("div", { class: "list-head" }, h("h2", {}, "History")), list, moreButton),
    detail,
  ));
  const first = selectId || historyState.selected || (historyState.commits[0] && historyState.commits[0].id);
  if (first) select(first);
  else detail.replaceChildren(h("div", { class: "page" }, h("p", { class: "empty" }, "No commits yet. Make one with veyra commit.")));
}

// Commit bodies are hard-wrapped at ~72 columns; join wrapped lines back into paragraphs and
// list items so they reflow to the pane's width.
function reflow(text) {
  const blocks = [];
  for (const para of text.split(/\n\s*\n/)) {
    const lines = para.split("\n");
    const isItem = (l) => /^\s*([-*•]|\d+[.)])\s+/.test(l);
    if (!lines.some(isItem)) { blocks.push(h("p", { class: "body-text" }, lines.map((l) => l.trim()).join(" "))); continue; }
    const items = [];
    for (const l of lines) {
      if (isItem(l) || !items.length) items.push(l.replace(/^\s*([-*•]|\d+[.)])\s+/, ""));
      else items[items.length - 1] += " " + l.trim();
    }
    blocks.push(h("ul", { class: "body-list" }, items.map((i) => h("li", {}, i))));
  }
  return blocks;
}

function renderCommit(c) {
  const body = c.message.split("\n").slice(1).join("\n").replace(/^\n+/, "");
  const trailerKeys = new Set(c.trailers.map((t) => t[0]));
  const bodyWithoutTrailers = body.split("\n").filter((l) => !trailerKeys.has(l.split(":")[0])).join("\n").trim();
  const sig = c.signature;
  const sigText = sig.status === "good" ? `Verified (${sig.format}${sig.signer ? `, ${sig.signer}` : ""})`
    : sig.status === "bad" ? `Bad signature${sig.signer ? ` from ${sig.signer}` : ""}` : "Not signed";

  const files = h("ul", { class: "files" }, c.files.map((f) => {
    const diffBox = h("div");
    const head = h("button", {
      class: "file-head", type: "button", "aria-expanded": "false",
      onclick: async () => {
        const open = head.getAttribute("aria-expanded") === "true";
        head.setAttribute("aria-expanded", open ? "false" : "true");
        if (open) { diffBox.replaceChildren(); return; }
        try { diffBox.replaceChildren(renderDiff(await api(`diff/${c.id}?path=${encodeURIComponent(f.path)}`))); }
        catch (e) { diffBox.replaceChildren(h("p", { class: "error" }, e.message)); }
      },
    },
      h("span", { class: "file-status" }, f.status),
      h("span", { class: "file-path" }, f.path),
      f.binary ? h("span", { class: "pill" }, "binary") : h("span", { class: "counts" }, h("span", { class: "a" }, `+${f.added}`), " ", h("span", { class: "d" }, `−${f.removed}`)),
    );
    const symbolItem = (s) => h("li", {},
      h("span", { class: `change-${s.change.replace(/ /g, "-")}` }, s.change), " ", s.kind, " ",
      h("span", { class: "sym" }, s.old_name ? `${s.old_name} → ${s.name}` : s.name));
    let syms = null;
    if (f.symbols.length) {
      const LIMIT = 8;
      const list = h("ul", { class: "symbols" }, f.symbols.slice(0, LIMIT).map(symbolItem));
      if (f.symbols.length > LIMIT) {
        const rest = f.symbols.length - LIMIT;
        const more = h("li", {}, h("button", { type: "button", class: "copy", onclick: () => { more.remove(); list.append(...f.symbols.slice(LIMIT).map(symbolItem)); } }, `Show ${rest} more`));
        list.append(more);
      }
      syms = list;
    } else if (f.formatting_only) {
      syms = h("ul", { class: "symbols" }, h("li", {}, "Formatting only"));
    }
    return h("li", { class: "file" }, head, syms, diffBox);
  }));

  return h("div", { class: "page detail" },
    h("h1", {}, c.summary),
    refChips(c.refs),
    bodyWithoutTrailers ? h("div", { class: "body" }, reflow(bodyWithoutTrailers)) : null,
    h("dl", { class: "facts" },
      h("dt", {}, "Author"), h("dd", {}, `${c.author} <${c.email}>`),
      h("dt", {}, "Date"), h("dd", {}, `${fullDate(c.time)} (${ago(c.time)})`),
      h("dt", {}, "Commit"), h("dd", {}, h("code", {}, c.id)),
      c.parents.length ? [h("dt", {}, c.parents.length > 1 ? "Parents" : "Parent"), h("dd", {}, c.parents.map((p, i) => [i ? ", " : "", h("a", { href: `#/history/${p.id}` }, h("code", {}, p.short))]))] : null,
      h("dt", {}, "Signature"), h("dd", { class: `status ${sig.status}` }, sigText),
      c.assisted.length ? [h("dt", {}, "AI-assisted"), h("dd", {}, c.assisted.join(", "))] : null,
      c.structural.length ? [h("dt", {}, "Auto-merged"), h("dd", {}, `Veyra merged ${c.structural.join(", ")} symbol by symbol`)] : null,
    ),
    c.intents.length ? h("section", { class: "block" }, h("h3", {}, "Intents"),
      h("ul", { class: "rows" }, c.intents.map((i) => h("li", {},
        h("div", { class: "row-title" }, i.title || i.id),
        h("div", { class: "row-sub" }, i.title ? `${i.id}, ${i.status}${i.objective ? `: ${i.objective}` : ""}` : "Not found in .veyra/intents"),
      )))) : null,
    c.attestations.length ? h("section", { class: "block" }, h("h3", {}, "Attestations"),
      h("ul", { class: "rows" }, c.attestations.map((a) => h("li", {},
        h("div", { class: "row-title" }, a.type),
        h("div", { class: "row-sub" }, a.signed ? `Signed by ${a.signer}` : "Unsigned", a.predicate && a.predicate.command ? `, ran ${a.predicate.command}` : ""),
      )))) : null,
    c.note ? h("section", { class: "block" }, h("h3", {}, "Note"), h("p", { class: "note" }, c.note)) : null,
    h("section", { class: "block" }, h("h3", {}, `${c.files.length} ${c.files.length === 1 ? "file" : "files"} changed`), files),
  );
}

function renderDiff(text) {
  const lines = text.replace(/\n$/, "").split("\n");
  return h("pre", { class: "diff" }, lines.map((l) => {
    let cls = "";
    if (l.startsWith("@@")) cls = "hunk";
    else if (l.startsWith("+++") || l.startsWith("---") || l.startsWith("diff ") || l.startsWith("index ")) cls = "meta";
    else if (l.startsWith("+")) cls = "add";
    else if (l.startsWith("-")) cls = "del";
    return h("span", { class: cls }, l || " ");
  }));
}

// ---- journal: the undo position, on a rail ----
async function viewJournal() {
  const j = await api("journal");
  const byId = Object.fromEntries(j.operations.map((o) => [o.id, o]));
  const undo = j.undo ? byId[j.undo] : null;
  const redo = j.redo ? byId[j.redo] : null;
  const rail = h("ol", { class: "rail", "aria-label": "Operations, newest first" });
  let placed = false;
  const here = () => h("li", { class: "here", "aria-label": "Current position" },
    h("strong", {}, "You are here"),
    undo ? h("p", {}, "Below this line is what veyra undo reverses next.") : h("p", {}, "Nothing left to undo."));
  for (const o of j.operations) {
    if (!placed && undo && o.id === undo.id) { rail.append(here()); placed = true; }
    rail.append(h("li", { class: `op kind-${o.kind}${o.undone ? " undone" : ""}` },
      h("div", { class: "op-line" },
        h("span", { class: "op-summary" }, o.summary || o.kind),
        h("span", { class: "op-kind" }, o.kind),
        h("span", { class: "op-time", title: fullDate(o.time) }, ago(o.time)),
        o.undone ? h("span", { class: "op-time" }, "undone") : null,
      ),
      h("div", { class: "op-command" }, o.command),
      o.files ? h("div", { class: "row-sub" }, `${o.files} uncommitted ${o.files === 1 ? "file" : "files"} snapshotted first`) : null,
    ));
  }
  if (!placed) rail.prepend(here());
  show(h("div", { class: "page" },
    h("h2", {}, "Journal"),
    h("p", { class: "lede" }, "Every operation that changed this repository, including plain git commands Veyra noticed. Undo walks down this line; redo walks back up."),
    h("div", { class: "next" },
      h("div", { class: "next-item" }, h("span", { class: "label" }, "Next undo"),
        undo ? [h("span", {}, undo.summary), command("veyra undo")] : h("span", { class: "row-sub" }, "nothing to undo")),
      h("div", { class: "next-item" }, h("span", { class: "label" }, "Next redo"),
        redo ? [h("span", {}, redo.summary), command("veyra redo")] : h("span", { class: "row-sub" }, "nothing to redo")),
    ),
    j.operations.length ? rail : h("p", { class: "empty" }, "No operations yet. Veyra records every command that changes the repository."),
  ));
}

// ---- checkpoints ----
async function viewCheckpoints() {
  const cps = await api("checkpoints");
  show(h("div", { class: "page" },
    h("h2", {}, "Checkpoints"),
    h("p", { class: "lede" }, "Snapshots of your working tree, including uncommitted and untracked files. Veyra takes one before any command that touches your files; restoring one is itself undoable."),
    cps.length ? h("ul", { class: "rows" }, cps.map((c) => h("li", {},
      h("div", { class: "row-title" }, c.source),
      h("div", { class: "row-sub" }, `${ago(c.time)}, ${c.automatic ? "automatic" : "taken by hand"}${c.files ? `, ${c.files} uncommitted ${c.files === 1 ? "file" : "files"}` : ""}`),
      h("div", { class: "row-sub" }, command(`veyra checkpoint restore ${c.short}`)),
    ))) : h("p", { class: "empty" }, "No checkpoints yet. veyra checkpoint takes one now."),
  ));
}

// ---- intents ----
async function viewIntents() {
  const intents = await api("intents");
  show(h("div", { class: "page" },
    h("h2", {}, "Intents"),
    h("p", { class: "lede" }, "Why parts of this code exist, with the guards that keep them true. Checked against your working tree now."),
    intents.length ? h("ul", { class: "rows" }, intents.map((i) => h("li", {},
      h("div", { class: "row-title" }, i.title),
      h("div", { class: "row-sub" }, `${i.id}, ${i.kind}, ${i.status}, since ${i.created}`),
      i.objective ? h("p", { class: "body-text" }, i.objective) : null,
      i.guards.length ? h("ul", { class: "guards" }, i.guards.map((g) => h("li", {}, h("code", {}, g.paths.join(", ")), ` ${g.check}`))) : null,
      i.status !== "active" ? null : (i.violations.length
        ? i.violations.map((v) => h("div", { class: "violation" }, `Violated: ${v}`))
        : (i.guards.length ? h("div", { class: "held" }, "All guards hold") : null)),
    ))) : h("p", { class: "empty" }, "No intents recorded. veyra intent create --title \"…\" records why something exists."),
  ));
}

// ---- impact ----
async function viewImpact(scope) {
  scope = scope || "uncommitted";
  const picker = h("select", { "aria-label": "Change to analyse", onchange: (e) => { location.hash = `#/impact/${e.target.value}`; } },
    ["uncommitted", "staged", "branch"].map((s) => h("option", { value: s, selected: s === scope }, s === "branch" ? "this branch" : `${s} changes`)));
  show(h("div", { class: "page" }, h("h2", {}, "Impact"), h("div", { class: "scope" }, picker), h("p", { class: "empty" }, "Analysing…")));
  let data;
  try { data = await api(`impact?scope=${scope}`); } catch (e) { show(h("div", { class: "page" }, h("h2", {}, "Impact"), h("div", { class: "scope" }, picker), h("p", { class: "error" }, e.message))); return; }
  const r = data.report;
  const emptyText = scope === "branch" ? "This branch has no commits that aren't on the default branch."
    : scope === "staged" ? "Nothing is staged." : "No uncommitted changes to tracked files.";
  const content = !r.changed_files.length ? [h("p", { class: "empty" }, emptyText)] : [
    h("div", { class: `risk ${r.risk.level}` }, `${r.risk.level[0].toUpperCase()}${r.risk.level.slice(1)} risk`),
    r.risk.reasons.length ? h("ul", { class: "reasons" }, r.risk.reasons.map((x) => h("li", {}, x))) : h("p", { class: "row-sub" }, "No risk factors found."),
    h("section", { class: "block" }, h("h3", {}, "Changed"),
      h("ul", { class: "rows" }, r.changed_files.map((f) => h("li", {},
        h("div", { class: "row-title" }, h("code", {}, f.path), " ", h("span", { class: "counts" }, h("span", { class: "a" }, `+${f.added_lines}`), " ", h("span", { class: "d" }, `−${f.removed_lines}`))),
        f.symbols.length ? h("ul", { class: "symbols" }, f.symbols.map((s) => h("li", {}, h("span", { class: `change-${s.change.replace(/ /g, "-")}` }, s.change), ` ${s.kind} `, h("span", { class: "sym" }, s.name)))) : null,
      )))),
    r.dependents.length ? h("section", { class: "block" }, h("h3", {}, "May be affected"),
      h("ul", { class: "rows" }, r.dependents.slice(0, 60).map((d) => h("li", {},
        h("div", {}, h("span", { class: `conf ${d.confidence}` }, d.confidence), h("code", {}, d.path), d.is_test ? h("span", { class: "row-sub" }, "  test") : null),
        h("ul", { class: "evidence" }, d.evidence.slice(0, 4).map((e) => h("li", {}, e))),
      )))) : null,
    h("section", { class: "block" }, h("h3", {}, "Tests to run"),
      r.tests.length ? h("ul", { class: "rows" }, r.tests.map((t) => h("li", {}, h("code", {}, t.path), h("div", { class: "row-sub" }, t.reason))))
        : h("p", { class: "row-sub" }, "None found for this change.")),
    r.co_changes.length ? h("section", { class: "block" }, h("h3", {}, "Usually changed together"),
      h("ul", { class: "rows" }, r.co_changes.map((c) => h("li", {}, h("code", {}, c.path), h("div", { class: "row-sub" }, `${c.together} of ${c.of} commits`))))) : null,
    h("section", { class: "block" }, h("h3", {}, "Limitations"), h("ul", { class: "reasons row-sub" }, r.limitations.map((l) => h("li", {}, l)))),
  ];
  show(h("div", { class: "page" }, h("h2", {}, "Impact"),
    h("p", { class: "lede" }, `What ${data.scope} can affect, with the evidence for each claim. Deterministic: no AI involved.`),
    h("div", { class: "scope" }, picker), ...content));
}

// ---- branches ----
async function viewBranches() {
  const nodes = await api("branches");
  const depth = {};
  const byName = Object.fromEntries(nodes.map((n) => [n.name, n]));
  const d = (n) => (depth[n.name] !== undefined ? depth[n.name] : (depth[n.name] = n.parent && byName[n.parent] ? d(byName[n.parent]) + 1 : 0));
  const order = [];
  const visit = (parent) => nodes.filter((n) => (n.parent || null) === parent).forEach((n) => { order.push(n); visit(n.name); });
  visit(null);
  for (const n of nodes) if (!order.includes(n)) order.push(n);
  show(h("div", { class: "page" },
    h("h2", {}, "Branches"),
    h("p", { class: "lede" }, "Each branch under the branch it grew from, with commits ahead and behind it."),
    order.length ? h("ul", { class: "tree" }, order.map((n) => h("li", {},
      h("span", { class: "indent", style: null, "aria-hidden": "true" }, "   ".repeat(d(n)), d(n) ? "└ " : ""),
      h("span", { class: n.current ? "ref current" : "ref" }, n.name),
      n.parent ? h("span", { class: "ab" }, `${n.ahead} ahead, ${n.behind} behind ${n.parent}`) : null,
      h("div", { class: "row-sub" }, h("code", {}, n.tip), ` ${n.summary || ""}`, n.time ? `, ${ago(n.time)}` : ""),
    ))) : h("p", { class: "empty" }, "No branches yet."),
  ));
}

// ---- routing ----
const views = { history: viewHistory, journal: viewJournal, checkpoints: viewCheckpoints, intents: viewIntents, impact: viewImpact, branches: viewBranches };
async function route() {
  const [, name = "history", arg] = (location.hash.match(/^#\/([a-z]+)(?:\/(.+))?/) || []);
  const view = views[name] ? name : "history";
  for (const a of document.querySelectorAll(".sections a")) {
    if (a.dataset.view === view) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current");
  }
  try { await views[view](arg ? decodeURIComponent(arg) : undefined); }
  catch (e) { show(failure(e)); }
}
window.addEventListener("hashchange", () => {
  if (takeToken()) {
    // A new server (or a new link): forget everything loaded with the old key.
    historyState.commits = [];
    historyState.selected = null;
    loadSummary();
  }
  route();
});
if (!token) {
  show(needLink());
} else {
  loadSummary();
  route();
}
