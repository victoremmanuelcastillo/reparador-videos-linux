const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const VIDEO_EXTENSIONS = ["mp4", "mov", "mkv", "ts", "avi", "m4v", "webm", "flv", "wmv", "3gp"];

const dropzone = document.getElementById("dropzone");
const destDirLabel = document.getElementById("destDirLabel");
const chooseDestBtn = document.getElementById("chooseDestBtn");
const resetDestBtn = document.getElementById("resetDestBtn");
const watchDirLabel = document.getElementById("watchDirLabel");
const watchToggleBtn = document.getElementById("watchToggleBtn");
const queueListEl = document.getElementById("queueList");
const summaryPanel = document.getElementById("summaryPanel");
const summaryText = document.getElementById("summaryText");
const repairAllBtn = document.getElementById("repairAllBtn");
const clearQueueBtn = document.getElementById("clearQueueBtn");
const openResultsBtn = document.getElementById("openResultsBtn");

let destDir = null; // null = misma carpeta que cada video (default)
let watchDir = null; // null = vigilancia desactivada
let queue = []; // { id, path, name, status, percent, outputPath, error }
let processing = false;
let nextId = 1;

function basename(path) {
  return path.split(/[\\/]/).pop();
}

function escapeHtml(s) {
  return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

function updateDestLabel() {
  if (destDir) {
    destDirLabel.textContent = destDir;
    resetDestBtn.classList.remove("hidden");
  } else {
    destDirLabel.textContent = "Misma carpeta que cada video";
    resetDestBtn.classList.add("hidden");
  }
}

function pendingCount() {
  return queue.filter((f) => f.status === "pending" || f.status === "error").length;
}

function updateActionButtons() {
  const n = pendingCount();
  repairAllBtn.disabled = processing || n === 0;
  repairAllBtn.textContent = processing ? "Reparando…" : n > 0 ? `Reparar (${n})` : "Reparar";

  clearQueueBtn.classList.toggle("hidden", processing || queue.length === 0);
  openResultsBtn.classList.toggle("hidden", !queue.some((f) => f.status === "done"));

  const allSettled = queue.length > 0 && queue.every((f) => f.status === "done" || f.status === "error");
  if (allSettled && !processing) {
    const doneCount = queue.filter((f) => f.status === "done").length;
    const errCount = queue.filter((f) => f.status === "error").length;
    summaryText.textContent = errCount
      ? `${doneCount} reparado(s), ${errCount} con error`
      : `${doneCount} video(s) reparado(s)`;
    summaryPanel.classList.remove("hidden");
  } else {
    summaryPanel.classList.add("hidden");
  }
}

function statusBadgeHtml(item) {
  switch (item.status) {
    case "pending":
      return `<span class="text-slate-500">En espera</span>`;
    case "processing":
      return `<span class="text-indigo-400">${Math.round(item.percent)}%</span>`;
    case "done":
      return `<span class="text-emerald-400">Listo</span>`;
    case "error":
      return `<span class="text-red-400" title="${escapeHtml(item.error || "")}">Error</span>`;
    default:
      return "";
  }
}

function renderQueue() {
  queueListEl.innerHTML = "";
  queueListEl.classList.toggle("hidden", queue.length === 0);

  for (const item of queue) {
    const row = document.createElement("div");
    row.className = "flex items-center gap-2 rounded-lg bg-slate-900/60 border border-slate-700 px-2.5 py-2 text-xs";
    row.dataset.id = item.id;

    const name = document.createElement("span");
    name.className = "flex-1 min-w-0 truncate text-slate-200";
    name.title = item.path;
    name.textContent = item.name;
    row.appendChild(name);

    const status = document.createElement("span");
    status.id = `status-${item.id}`;
    status.className = "shrink-0";
    status.innerHTML = statusBadgeHtml(item);
    row.appendChild(status);

    if (item.status === "error") {
      const retryBtn = document.createElement("button");
      retryBtn.textContent = "Reintentar";
      retryBtn.className = "shrink-0 text-indigo-400 hover:text-indigo-300 font-medium";
      retryBtn.addEventListener("click", () => {
        item.status = "pending";
        item.error = null;
        renderQueue();
        updateActionButtons();
      });
      row.appendChild(retryBtn);
    }

    if (item.status === "pending" || item.status === "error") {
      const removeBtn = document.createElement("button");
      removeBtn.textContent = "×";
      removeBtn.title = "Quitar";
      removeBtn.className = "shrink-0 text-slate-500 hover:text-slate-300 text-sm leading-none px-1";
      removeBtn.addEventListener("click", () => {
        queue = queue.filter((f) => f.id !== item.id);
        renderQueue();
        updateActionButtons();
      });
      row.appendChild(removeBtn);
    }

    queueListEl.appendChild(row);
  }
}

function addFiles(paths) {
  const existing = new Set(queue.map((f) => f.path));
  for (const p of paths) {
    if (existing.has(p)) continue;
    queue.push({ id: `f${nextId++}`, path: p, name: basename(p), status: "pending", percent: 0, error: null, outputPath: null });
    existing.add(p);
  }
  renderQueue();
  updateActionButtons();
}

async function pickFiles() {
  const paths = await invoke("plugin:dialog|open", {
    options: {
      multiple: true,
      directory: false,
      filters: [{ name: "Video", extensions: VIDEO_EXTENSIONS }],
    },
  });
  if (!paths) return;
  addFiles(Array.isArray(paths) ? paths : [paths]);
}

function updateWatchLabel() {
  if (watchDir) {
    watchDirLabel.textContent = watchDir;
    watchDirLabel.classList.remove("text-red-400");
    watchToggleBtn.textContent = "Detener";
  } else {
    watchDirLabel.textContent = "Desactivada";
    watchDirLabel.classList.remove("text-red-400");
    watchToggleBtn.textContent = "Activar…";
  }
}

async function toggleWatch() {
  if (watchDir) {
    await invoke("stop_watch");
    watchDir = null;
    updateWatchLabel();
    return;
  }
  const dir = await invoke("plugin:dialog|open", { options: { multiple: false, directory: true } });
  if (!dir) return;
  try {
    await invoke("start_watch", { folder: dir });
    watchDir = dir;
    updateWatchLabel();
  } catch (err) {
    watchDirLabel.textContent = typeof err === "string" ? err : "No se pudo activar la vigilancia.";
    watchDirLabel.classList.add("text-red-400");
  }
}

async function pickDestDir() {
  const dir = await invoke("plugin:dialog|open", {
    options: { multiple: false, directory: true },
  });
  if (dir) {
    destDir = dir;
    updateDestLabel();
  }
}

async function processQueue() {
  if (processing) return;
  processing = true;
  updateActionButtons();

  const pending = queue.filter((f) => f.status === "pending" || f.status === "error");
  for (const item of pending) {
    item.status = "processing";
    item.percent = 0;
    item.error = null;
    renderQueue();

    try {
      const result = await invoke("repair_video", {
        id: item.id,
        inputPath: item.path,
        outputDir: destDir,
      });
      item.status = "done";
      item.percent = 100;
      item.outputPath = result.output_path;
    } catch (err) {
      item.status = "error";
      item.error = typeof err === "string" ? err : JSON.stringify(err);
    }
    renderQueue();
    updateActionButtons();
  }

  processing = false;
  updateActionButtons();
}

listen("repair-progress", (event) => {
  const { id, percent } = event.payload;
  const item = queue.find((f) => f.id === id);
  if (!item || item.status !== "processing") return;
  item.percent = percent;
  const badge = document.getElementById(`status-${id}`);
  if (badge) badge.textContent = `${Math.round(percent)}%`;
});

dropzone.addEventListener("click", pickFiles);
chooseDestBtn.addEventListener("click", pickDestDir);
watchToggleBtn.addEventListener("click", toggleWatch);
resetDestBtn.addEventListener("click", () => {
  destDir = null;
  updateDestLabel();
});
repairAllBtn.addEventListener("click", processQueue);
clearQueueBtn.addEventListener("click", () => {
  queue = queue.filter((f) => f.status === "processing");
  renderQueue();
  updateActionButtons();
});
openResultsBtn.addEventListener("click", async () => {
  const done = [...queue].reverse().find((f) => f.status === "done");
  if (done && done.outputPath) {
    await invoke("plugin:opener|reveal_item_in_dir", { path: done.outputPath });
  }
});

listen("open-file", (event) => {
  addFiles([event.payload]);
});

invoke("take_pending_open_file").then((path) => {
  if (path) addFiles([path]);
});

window.__TAURI__.window.getCurrentWindow().onDragDropEvent((event) => {
  if (event.payload.type === "over") {
    dropzone.classList.add("border-indigo-400", "bg-indigo-500/5");
  } else if (event.payload.type === "drop") {
    dropzone.classList.remove("border-indigo-400", "bg-indigo-500/5");
    const paths = event.payload.paths;
    if (paths && paths.length > 0) addFiles(paths);
  } else {
    dropzone.classList.remove("border-indigo-400", "bg-indigo-500/5");
  }
});

updateDestLabel();
updateWatchLabel();
updateActionButtons();
