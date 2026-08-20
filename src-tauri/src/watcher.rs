use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::tray;

const RESULTS_SUBDIR: &str = "reparados";

#[derive(Default)]
pub struct WatchState(Mutex<Option<(RecommendedWatcher, String)>>);

#[derive(Clone, Serialize)]
struct WatchResult {
    name: String,
    ok: bool,
    detail: String,
}

fn is_own_output(path: &Path) -> bool {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.ends_with("_reparado"))
        .unwrap_or(false)
}

async fn wait_until_stable(path: &Path) -> bool {
    let mut last: Option<u64> = None;
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        let size = meta.len();
        if size > 0 && Some(size) == last {
            return true;
        }
        last = Some(size);
    }
    last.unwrap_or(0) > 0
}

async fn auto_repair(
    app: AppHandle,
    path: PathBuf,
    out_dir: PathBuf,
    in_progress: Arc<Mutex<HashSet<PathBuf>>>,
) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if wait_until_stable(&path).await {
        let _ = std::fs::create_dir_all(&out_dir);
        let job_id = format!("watch-{name}");
        let result = crate::repair_video(
            app.clone(),
            job_id,
            path.to_string_lossy().to_string(),
            Some(out_dir.to_string_lossy().to_string()),
        )
        .await;

        let payload = match result {
            Ok(outcome) => WatchResult { name: name.clone(), ok: true, detail: outcome.output_path },
            Err(e) => WatchResult { name: name.clone(), ok: false, detail: e },
        };
        tray::set_tooltip(
            &app,
            &format!("Reparador de Videos — {}: {}", if payload.ok { "reparado" } else { "error" }, payload.name),
        );
        let _ = app.emit("watch-repaired", payload);
    }

    in_progress.lock().unwrap().remove(&path);
}

#[tauri::command]
pub fn start_watch(app: AppHandle, state: State<'_, WatchState>, folder: String) -> Result<(), String> {
    let folder_path = PathBuf::from(&folder);
    if !folder_path.is_dir() {
        return Err("La carpeta no existe.".to_string());
    }
    let out_dir = folder_path.join(RESULTS_SUBDIR);

    let (tx, rx) = channel::<notify::Result<Event>>();
    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(tx).map_err(|e| format!("No se pudo iniciar el vigilante: {e}"))?;
    watcher
        .watch(&folder_path, RecursiveMode::NonRecursive)
        .map_err(|e| format!("No se pudo vigilar la carpeta: {e}"))?;

    let in_progress: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    let app_thread = app.clone();
    let out_dir_thread = out_dir.clone();

    std::thread::spawn(move || {
        for res in rx {
            let Ok(event) = res else { continue };
            if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                continue;
            }
            for path in event.paths {
                if !path.is_file() || is_own_output(&path) || path.starts_with(&out_dir_thread) {
                    continue;
                }
                let mut set = in_progress.lock().unwrap();
                if !set.insert(path.clone()) {
                    continue;
                }
                drop(set);

                tauri::async_runtime::spawn(auto_repair(
                    app_thread.clone(),
                    path,
                    out_dir_thread.clone(),
                    in_progress.clone(),
                ));
            }
        }
    });

    tray::set_tooltip(&app, &format!("Vigilando: {folder}"));
    *state.0.lock().unwrap() = Some((watcher, folder));
    Ok(())
}

#[tauri::command]
pub fn stop_watch(app: AppHandle, state: State<'_, WatchState>) {
    *state.0.lock().unwrap() = None;
    tray::set_tooltip(&app, "Reparador de Videos");
}
