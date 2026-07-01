
use walkdir::WalkDir;
use blake3;
use serde::Serialize;
use chrono::{DateTime, Local, Duration};
use std::fs::{File, read_to_string};
use std::io::{Read, Write, BufRead};
use std::path::Path;
use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::process::{Command, Stdio};
use std::collections::HashSet;
use std::time::Instant;
use std::io::stdout;

#[cfg(windows)]
fn set_utf8_console() {
    let _ = Command::new("chcp").arg("65001").status();
}

#[derive(Serialize)]
struct FileInfo {
    path: String,
    modified: String,
    size: u64,
    hash: String,
}

fn compute_hash(path: &Path) -> String {
    let mut hasher = blake3::Hasher::new();
    if let Ok(mut file) = File::open(path) {
        let mut buffer = [0u8; 65536];
        while let Ok(n) = file.read(&mut buffer) {
            if n == 0 { break; }
            hasher.update(&buffer[..n]);
        }
    }
    hasher.finalize().to_hex().to_string()
}

fn format_duration(d: Duration) -> String {
    let secs = d.num_seconds();
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs_rem = secs % 60;
    format!("{:02}:{:02}:{:02}", hours, mins, secs_rem)
}

fn run_backup(dirs: &HashSet<String>, destination: &str, mode: &str) {
    println!("INFO: 差分バックアップ開始 (mode: {})...", mode);
    let total = dirs.len();
    let mut count = 0;
    let start_time = Instant::now();

    for dir in dirs {
        count += 1;

        // JSON進捗出力
        let elapsed = start_time.elapsed().as_secs();
        let percent = count as f64 / total as f64;
        let remaining_secs = if percent > 0.0 {
            (elapsed as f64 / percent) - elapsed as f64
        } else {
            0.0
        };
        let eta_time = Local::now() + Duration::seconds(remaining_secs as i64);
        let progress = serde_json::json!({
            "type": "copy",
            "done": count,
            "total": total,
            "percent": percent * 100.0,
            "eta": eta_time.format("%H:%M:%S").to_string(),
            "remaining": format_duration(Duration::seconds(remaining_secs as i64))
        });
        println!("{}
", progress);
        stdout().flush().unwrap();

        // robocopy実行（標準出力抑制）
        let mut args = vec![
            dir.as_str(), destination,
            "/MIR", "/FFT", "/Z", "/XA:H", "/W:5", "/R:1", "/MT:8",
            "/NP", "/NFL", "/NDL", "/LOG:NUL", "/UNICODE"
        ];
        if mode == "added" { args.push("/XO"); }
        if mode == "removed" { args.push("/PURGE"); }

        let _ = Command::new("robocopy")
            .args(&args)
            .stdout(Stdio::null())
            .spawn()
            .expect("robocopy 実行に失敗しました")
            .wait()
            .expect("robocopy終了待機に失敗しました");
    }
}


fn main() {
    #[cfg(windows)]
    set_utf8_console();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("Usage: file_scanner <target_dir> <previous.txt> <current.txt> <backup_dir>");
        std::process::exit(1);
    }

    let target_dir = &args[1];
    let prev_file = &args[2];
    let curr_file = &args[3];
    let backup_dir = &args[4];

    let files: Vec<_> = WalkDir::new(target_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .collect();

    let total = files.len();
    println!("INFO: Total files = {}", total);
    println!("SCAN_PROGRESS: 0/{} (0.00%) ETA: -- Remaining: --", total);
    stdout().flush().unwrap();

    let counter = Arc::new(AtomicUsize::new(0));
    let start_time = Instant::now();

    let results: Vec<FileInfo> = files.par_iter().map(|entry| {
        let metadata = entry.metadata().unwrap();
        let modified: DateTime<Local> = DateTime::from(metadata.modified().unwrap());
        let hash = compute_hash(entry.path());
        let done = counter.fetch_add(1, Ordering::SeqCst) + 1;

        if done % 100 == 0 || done == total {
            let elapsed = start_time.elapsed().as_secs();
            let percent = done as f64 / total as f64;
            let remaining_secs = if percent > 0.0 {
                (elapsed as f64 / percent) - elapsed as f64
            } else {
                0.0
            };
            let eta_time = Local::now() + Duration::seconds(remaining_secs as i64);
            println!(
                "SCAN_PROGRESS: {}/{} ({:.2}%) ETA: {} Remaining: {}",
                done, total, percent * 100.0,
                eta_time.format("%H:%M:%S"),
                format_duration(Duration::seconds(remaining_secs as i64))
            );
            stdout().flush().unwrap();
        }

        FileInfo {
            path: entry.path().display().to_string(),
            modified: modified.format("%Y-%m-%d %H:%M:%S").to_string(),
            size: metadata.len(),
            hash,
        }
    }).collect();

    println!("SCAN_PROGRESS: {}/{} (100.00%) ETA: -- Remaining: 00:00:00", total, total);
    stdout().flush().unwrap();

    let mut curr_out = File::create(curr_file).unwrap();
    for info in &results {
        writeln!(
            curr_out,
            "{}\n{}\nSIZE:{}\nHASH:{}",
            info.path, info.modified, info.size, info.hash
        ).unwrap();
    }

    let prev_set: HashSet<String> = if Path::new(prev_file).exists() {
        read_to_string(prev_file).unwrap().lines().map(|s| s.to_string()).collect()
    } else {
        HashSet::new()
    };

    let curr_set: HashSet<String> = results.iter()
        .map(|info| format!("{}\n{}\nSIZE:{}\nHASH:{}",
            info.path, info.modified, info.size, info.hash))
        .collect();

    let added: Vec<_> = curr_set.difference(&prev_set).cloned().collect();
    let removed: Vec<_> = prev_set.difference(&curr_set).cloned().collect();

    let mut added_dirs = HashSet::new();
    for path in &added {
        if let Some(parent) = Path::new(path.split('\n').next().unwrap()).parent() {
            added_dirs.insert(parent.display().to_string());
        }
    }

    let mut removed_dirs = HashSet::new();
    for path in &removed {
        if let Some(parent) = Path::new(path.split('\n').next().unwrap()).parent() {
            removed_dirs.insert(parent.display().to_string());
        }
    }

    run_backup(&added_dirs, backup_dir, "added");
    run_backup(&removed_dirs, backup_dir, "removed");

    println!("PROCESS_DONE");
}
