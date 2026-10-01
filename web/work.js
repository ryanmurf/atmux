/* Local ledger mirror: no browser credentials or direct platform calls. */
(function () {
  "use strict";
  function workRows(jobs, source = "", state = "") {
    if (!Array.isArray(jobs)) return [];
    return jobs.slice(0, 500).filter(job => job && typeof job.item?.goal === "string")
      .map(job => ({ goal: job.item.goal.slice(0, 500), source: String(job.item.source || "ledger").slice(0, 100),
        state: String(job.message?.jobState || "UNKNOWN").slice(0, 50),
        id: String(job.message?.jobId || job.message?.id || ""),
        pane: typeof job.assignment?.pane === "string" ? job.assignment.pane : null,
        session: String(job.assignment?.name || job.assignment?.session_key || ""),
        blocked: String(job.blocked || "").slice(0, 1000) }))
      .filter(row => (!source || row.source === source) && (!state || row.state === state));
  }
  if (typeof module !== "undefined") module.exports = { workRows };
  if (typeof document === "undefined") return;
  const dialog = document.getElementById("work-dialog");
  const button = document.getElementById("work-open");
  if (!dialog || !button) return;
  const status = document.getElementById("work-status");
  const jobs = document.getElementById("work-jobs");
  const source = document.getElementById("work-source");
  const state = document.getElementById("work-state");
  let entries = [];
  let fetching = false;
  function render() {
    jobs.replaceChildren();
    for (const row of workRows(entries, source.value, state.value)) {
      const article = document.createElement("article"); article.className = "work-job";
      const title = document.createElement("strong"); title.textContent = row.goal;
      const detail = document.createElement("p"); detail.textContent = `${row.source} · ${row.state}${row.blocked ? " · " + row.blocked : ""}`;
      article.append(title, detail);
      if (row.pane) {
        const link = document.createElement("a"); const url = new URL(location.href);
        url.searchParams.delete("view"); url.searchParams.delete("machine"); url.searchParams.set("session", row.pane);
        link.href = url.href; link.textContent = row.session || "Assigned session"; article.append(link);
      }
      jobs.append(article);
    }
    if (!jobs.children.length) jobs.textContent = "No jobs match these filters.";
  }
  function options(select, values) {
    const previous = select.value; const first = select.options[0].cloneNode(true);
    select.replaceChildren(first);
    for (const value of [...new Set(values)].sort()) {
      const option = document.createElement("option"); option.value = value; option.textContent = value; select.append(option);
    }
    select.value = values.includes(previous) ? previous : "";
  }
  async function refresh() {
    if (fetching) return; fetching = true;
    try {
      const response = await fetch("/api/v1/work", { signal: AbortSignal.timeout(10000) });
      if (!response.ok) throw new Error("Work is unavailable");
      const view = await response.json(); entries = Array.isArray(view.jobs) ? view.jobs.slice(0, 500) : [];
      status.textContent = !view.enabled ? "Intake is disabled." : view.stopped ? "Intake is stopped." : view.dry_run ? "Dry run: decisions only." : "Channel ledger jobs";
      const rows = workRows(entries); options(source, rows.map(r => r.source)); options(state, rows.map(r => r.state)); render();
    } catch (_) { status.textContent = "Work is unavailable. Try refreshing."; }
    finally { fetching = false; }
  }
  button.addEventListener("click", () => { dialog.showModal(); refresh(); });
  document.getElementById("work-close").addEventListener("click", () => dialog.close());
  document.getElementById("work-refresh").addEventListener("click", refresh);
  source.addEventListener("change", render); state.addEventListener("change", render);
  setInterval(() => { if (dialog.open && !document.hidden) refresh(); }, 15000);
})();
