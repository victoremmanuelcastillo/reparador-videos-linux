mod recover;
mod tray;
mod watcher;

use std::path::Path;
use std::process::Stdio;
use std::sync::Mutex;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use watcher::WatchState;

#[derive(Clone, Serialize)]
struct RepairProgress {
    id: String,
    percent: f64,
    stage: Option<String>,
}

#[derive(Serialize)]
struct RepairOutcome {
    output_path: String,
    size_bytes: u64,
    note: Option<String>,
}

/// Fallback para videos sin `moov` (descarga cortada): ffmpeg no puede hacer remux, se reconstruye el
/// indice desde el `mdat` crudo. Ver `recover.rs`.
async fn recover_video(
    app: AppHandle,
    id: String,
    input: String,
    output: std::path::PathBuf,
) -> Result<RepairOutcome, String> {
    let (app_r, id_r, out_r) = (app.clone(), id.clone(), output.clone());
    let recovered = tauri::async_runtime::spawn_blocking(move || {
        let report = |percent: f64, stage: &str| {
            let _ = app_r.emit(
                "repair-progress",
                RepairProgress { id: id_r.clone(), percent, stage: Some(stage.to_string()) },
            );
        };
        recover::rebuild(Path::new(&input), &out_r, &report)
    })
    .await
    .map_err(|e| format!("Fallo la reconstruccion: {e}"))??;

    let meta = std::fs::metadata(&output).map_err(|e| format!("No se genero el archivo de salida: {e}"))?;
    let _ = app.emit("repair-progress", RepairProgress { id, percent: 100.0, stage: None });
    Ok(RepairOutcome {
        output_path: output.to_string_lossy().to_string(),
        size_bytes: meta.len(),
        note: Some(recovered.note),
    })
}

async fn probe_duration_secs(path: &str) -> Option<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
            path,
        ])
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[tauri::command]
async fn repair_video(
    app: AppHandle,
    id: String,
    input_path: String,
    output_dir: Option<String>,
) -> Result<RepairOutcome, String> {
    let input = Path::new(&input_path);
    if !input.is_file() {
        return Err("El archivo no existe.".to_string());
    }

    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .ok_or("Nombre de archivo invalido.")?;
    let ext = input
        .extension()
        .map(|e| e.to_string_lossy().to_string())
        .unwrap_or_else(|| "mp4".to_string());
    let parent = input.parent().unwrap_or_else(|| Path::new("."));
    let out_dir = match output_dir.as_deref() {
        Some(d) if !d.is_empty() => Path::new(d),
        _ => parent,
    };
    let output = out_dir.join(format!("{stem}_reparado.{ext}"));
    let output_str = output.to_string_lossy().to_string();

    let duration_secs = probe_duration_secs(&input_path).await.unwrap_or(0.0);

    let mut child = Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            &input_path,
            "-c",
            "copy",
            "-progress",
            "pipe:1",
            "-nostats",
            &output_str,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("No se pudo iniciar ffmpeg: {e}"))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let app_progress = app.clone();
    let id_progress = id.clone();
    let progress_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(us) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = us.parse::<f64>() {
                    let percent = if duration_secs > 0.0 {
                        ((us / 1_000_000.0) / duration_secs * 100.0).clamp(0.0, 99.0)
                    } else {
                        0.0
                    };
                    let _ = app_progress.emit(
                        "repair-progress",
                        RepairProgress { id: id_progress.clone(), percent, stage: None },
                    );
                }
            }
        }
    });

    let mut stderr_buf = String::new();
    stderr.read_to_string(&mut stderr_buf).await.ok();

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Error esperando a ffmpeg: {e}"))?;
    let _ = progress_task.await;

    if !status.success() {
        if stderr_buf.contains("moov atom not found") {
            let output = out_dir.join(format!("{stem}_reparado.mp4"));
            return recover_video(app, id, input_path, output).await.map_err(|e| {
                format!("El video no tiene indice (moov) y la reconstruccion fallo:\n{e}")
            });
        }
        let tail: Vec<&str> = stderr_buf.lines().rev().take(6).collect();
        let tail: String = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(format!(
            "ffmpeg no pudo reparar el video:\n{}",
            if tail.is_empty() {
                "(sin detalles, revisa que el archivo no este completamente vacio)".to_string()
            } else {
                tail
            }
        ));
    }

    let meta = std::fs::metadata(&output)
        .map_err(|e| format!("No se genero el archivo de salida: {e}"))?;
    if meta.len() == 0 {
        return Err("El archivo reparado quedo vacio. Los datos originales pueden faltar.".to_string());
    }

    let _ = app.emit("repair-progress", RepairProgress { id, percent: 100.0, stage: None });

    Ok(RepairOutcome {
        output_path: output_str,
        size_bytes: meta.len(),
        note: None,
    })
}

#[derive(Default)]
struct PendingOpenFile(Mutex<Option<String>>);

#[tauri::command]
fn take_pending_open_file(state: tauri::State<PendingOpenFile>) -> Option<String> {
    state.0.lock().unwrap().take()
}

fn first_arg_path() -> Option<String> {
    std::env::args().skip(1).find(|a| !a.starts_with('-'))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(path) = argv.into_iter().skip(1).find(|a| !a.starts_with('-')) {
                let _ = app.emit("open-file", path);
            }
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(PendingOpenFile(Mutex::new(first_arg_path())))
        .manage(WatchState::default())
        .invoke_handler(tauri::generate_handler![
            repair_video,
            take_pending_open_file,
            watcher::start_watch,
            watcher::stop_watch,
        ])
        .setup(|app| {
            tray::setup_tray(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
