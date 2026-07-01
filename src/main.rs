use walkdir::WalkDir;
use blake3;
use serde_json::json;
use chrono::{DateTime, Local, Duration};
use std::fs::{File, read_to_string};
use std::io::{Read, Write, stdout, BufRead, BufReader};
use std::path::Path;
use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::process::{Command, Stdio};
use std::collections::HashSet;
use std::time::Instant;
use regex::Regex;

#[cfg(windows)]
fn set_utf8_console() {
    let _ = Command::new("chcp").arg("65001").status();
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

// ✅ robocopyでバックアップ＋進捗解析
fn backup_with_robocopy(target_dir: &str, backup_dir: &str) {
    println!("{}", json!({"type":"info","message":"Robocopy backup started"}));
    stdout().flush().unwrap();

    let mut cmd = Command::new("robocopy");
    cmd.arg(target_dir)
        .arg(backup_dir)
        .args(&["/E", "/COPY:DAT", "/R:1", "/W:1", "/MT:8", "/ETA", "/V"]);

    let mut child = match cmd.stdout(Stdio::piped()).spawn() {
        Ok(c) => c,
        Err(e) => {
            println!("{}", json!({"type":"error","message":format!("Failed to start robocopy: {}", e)}));
            return;
        }
    };

    let stdout_pipe = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout_pipe);

    // robocopy進捗解析用正規表現
    let re = Regex::new(r"(\d+)%.*ETA\s+(\d{2}:\d{2}:\d{2})").unwrap();

    for line in reader.lines() {
        if let Ok(l) = line {
            if let Some(caps) = re.captures(&l) {
                let percent: u32 = caps[1].parse().unwrap_or(0);
                let eta = &caps[2];
                println!("{}", json!({
                    "type": "copy",
                    "percent": percent,
                    "eta": eta,
                    "remaining": eta // 簡易的にETAをremainingとして使用
                }));
                stdout().flush().unwrap();
            }
        }
    }

    let status = child.wait().unwrap();
    println!("{}", json!({"type":"copy_done","message":format!("Robocopy exited with code {}", status.code().unwrap_or(-1))}));
    stdout().flush().unwrap();
}

fn main() {
    #[cfg(windows)]
    set_utf8_console();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("Usage: file_scanner <target_dir> <previous.txt> <current.txt> <backup_dir> [--auto-backup]");
        std::process::exit(1);
    }

    let target_dir = &args[1];
    let prev_file = &args[2];
    let curr_file = &args[3];
    let backup_dir = &args[4];
    let auto_backup = args.contains(&"--auto-backup".to_string());

    // デバッグ出力
    println!("{}", json!({"type":"info","message":format!("Args: {:?}", args)}));
    stdout().flush().unwrap();

    let files: Vec<_> = WalkDir::new(target_dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .collect();

    let total = files.len();
    println!("INFO: Total files = {}", total);
    stdout().flush().unwrap();

    let counter = Arc::new(AtomicUsize::new(0));
    let start_time = Instant::now();

    let results: Vec<String> = files.par_iter().map(|entry| {
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
            let progress = json!({
                "type": "scan",
                "done": done,
                "total": total,
                "percent": percent * 100.0,
                "eta": eta_time.format("%H:%M:%S").to_string(),
                "remaining": format_duration(Duration::seconds(remaining_secs as i64))
            });
            println!("{}", progress);
            stdout().flush().unwrap();
        }

        format!("{}\n{}\nSIZE:{}\nHASH:{}",
            entry.path().display(),
            modified.format("%Y-%m-%d %H:%M:%S"),
            metadata.len(),
            hash)
    }).collect();

    // Save current file list
    let mut curr_out = File::create(curr_file).unwrap();
    for info in &results {
        writeln!(curr_out, "{}", info).unwrap();
    }

    // Compare previous and current
    let prev_set: HashSet<String> = if Path::new(prev_file).exists() {
        read_to_string(prev_file).unwrap().lines().map(|s| s.to_string()).collect()
    } else {
        HashSet::new()
    };
    let curr_set: HashSet<String> = results.iter().map(|s| s.clone()).collect();

    let added: Vec<_> = curr_set.difference(&prev_set).cloned().collect();
    let removed: Vec<_> = prev_set.difference(&curr_set).cloned().collect();

    // Save diff log
    let log_file = format!("log_{}.txt", Local::now().format("%Y%m%d"));
    let mut log_out = File::create(&log_file).unwrap();

    for path in &added {
        let file_path = path.split('\n').next().unwrap();
        writeln!(log_out, "追加: {}", file_path).unwrap();
        println!("{}", json!({"type":"diff","action":"added","file":file_path}));
    }
    for path in &removed {
        let file_path = path.split('\n').next().unwrap();
        writeln!(log_out, "削除: {}", file_path).unwrap();
        println!("{}", json!({"type":"diff","action":"removed","file":file_path}));
    }

    // ✅ Auto Backupモード（robocopy）
    if auto_backup {
        println!("{}", json!({"type":"info","message":"Auto backup flag detected"}));
        backup_with_robocopy(target_dir, backup_dir);
    } else {
        println!("{}", json!({"type":"info","message":"Auto backup flag NOT detected"}));
    }

    println!("{}", json!({"type": "done"}));
    stdout().flush().unwrap();
}