use std::fmt;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use chrono::Local;
use sysinfo::System;

// ============================================================
// ÉTAPE 1 — Modélisation des données
// ============================================================

#[derive(Debug, Clone)]
pub struct CpuInfo {
    pub usage_percent: f32,
    pub core_count: usize,
}

impl fmt::Display for CpuInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CPU: {:.1}% ({} cores)", self.usage_percent, self.core_count)
    }
}

#[derive(Debug, Clone)]
pub struct MemInfo {
    pub total_kb: u64,
    pub used_kb: u64,
    pub free_kb: u64,
}

impl fmt::Display for MemInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Memory: {}MB used / {}MB total ({}MB free)", 
               self.used_kb / 1024, self.total_kb / 1024, self.free_kb / 1024)
    }
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub cpu_usage: f32,
    pub mem_kb: u64,
}

impl fmt::Display for ProcessInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:6} {:20} {:5.1}% {}MB", 
               self.pid, self.name, self.cpu_usage, self.mem_kb / 1024)
    }
}

#[derive(Debug, Clone)]
pub struct SystemSnapshot {
    pub cpu: CpuInfo,
    pub mem: MemInfo,
    pub processes: Vec<ProcessInfo>,
    pub timestamp: String,
}

impl fmt::Display for SystemSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "=== System Snapshot: {} ===", self.timestamp)?;
        writeln!(f, "{}", self.cpu)?;
        writeln!(f, "{}", self.mem)?;
        writeln!(f, "\nTop 5 Processes:")?;
        writeln!(f, "PID     Name                 CPU%   Memory")?;
        writeln!(f, "-------------------------------------------")?;
        for process in &self.processes {
            writeln!(f, "{}", process)?;
        }
        Ok(())
    }
}

// ============================================================
// ÉTAPE 2 — Collecte réelle et gestion d'erreurs
// ============================================================

#[derive(Debug)]
pub enum SysWatchError {
    CollectionError(String),
}

impl fmt::Display for SysWatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SysWatchError::CollectionError(msg) => write!(f, "Collection error: {}", msg),
        }
    }
}

impl std::error::Error for SysWatchError {}

fn collect_snapshot() -> Result<SystemSnapshot, SysWatchError> {
    let mut sys = System::new_all();
    sys.refresh_all();

    // Collect CPU info
    let cpu_usage = sys.global_cpu_info().cpu_usage();
    let core_count = sys.cpus().len();

    // Collect memory info
    let total_memory = sys.total_memory();
    let used_memory = sys.used_memory();
    let free_memory = sys.available_memory();

    // Collect processes and get top 5 by CPU usage
    let mut processes: Vec<ProcessInfo> = sys
        .processes()
        .values()
        .map(|p| ProcessInfo {
            pid: p.pid().as_u32(),
            name: p.name().to_string(),
            cpu_usage: p.cpu_usage(),
            mem_kb: p.memory(),
        })
        .collect();

    // Sort by CPU usage descending and take top 5
    processes.sort_by(|a, b| b.cpu_usage.partial_cmp(&a.cpu_usage).unwrap());
    processes.truncate(5);

    // Create timestamp
    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();

    Ok(SystemSnapshot {
        cpu: CpuInfo {
            usage_percent: cpu_usage,
            core_count,
        },
        mem: MemInfo {
            total_kb: total_memory,
            used_kb: used_memory,
            free_kb: free_memory,
        },
        processes,
        timestamp,
    })
}

// ============================================================
// ÉTAPE 3 — Formatage des réponses réseau
// ============================================================

fn create_ascii_bar(value: f32, max: f32) -> String {
    let filled = ((value / max) * 20.0) as usize;
    let empty = 20 - filled;
    "█".repeat(filled) + &"░".repeat(empty)
}

fn format_response(snapshot: &SystemSnapshot, command: &str) -> String {
    match command.trim() {
        "cpu" => {
            let bar = create_ascii_bar(snapshot.cpu.usage_percent, 100.0);
            format!("CPU Usage: {:.1}%\n{} ({} cores)\n", 
                   snapshot.cpu.usage_percent, bar, snapshot.cpu.core_count)
        }
        "mem" => {
            let usage_percent = (snapshot.mem.used_kb as f32 / snapshot.mem.total_kb as f32) * 100.0;
            let bar = create_ascii_bar(usage_percent, 100.0);
            format!("Memory: {}MB / {}MB used ({:.1}%)\n{}\nFree: {}MB\n",
                   snapshot.mem.used_kb / 1024,
                   snapshot.mem.total_kb / 1024,
                   usage_percent,
                   bar,
                   snapshot.mem.free_kb / 1024)
        }
        "ps" => {
            let mut result = String::new();
            result.push_str("PID     Name                 CPU%   Memory\n");
            result.push_str("-------------------------------------------\n");
            for process in &snapshot.processes {
                result.push_str(&format!("{}\n", process));
            }
            result
        }
        "all" => {
            format!("{}\n{}\n{}", 
                   format_response(snapshot, "cpu"),
                   format_response(snapshot, "mem"),
                   format_response(snapshot, "ps"))
        }
        "help" => {
            "Available commands:\n\
             cpu  - Show CPU usage and core count\n\
             mem  - Show memory usage statistics\n\
             ps   - Show top 5 processes by CPU usage\n\
             all  - Show all system information\n\
             help - Show this help message\n\
             quit - Disconnect from server\n".to_string()
        }
        "quit" => "Goodbye! Disconnecting...\n".to_string(),
        _ => format!("Unknown command: '{}'\nType 'help' for available commands.\n", command.trim()),
    }
}

// ============================================================
// ÉTAPE 4 — Serveur TCP multi-threadé
// ============================================================

fn handle_client(
    mut stream: TcpStream,
    snapshot: Arc<Mutex<SystemSnapshot>>,
    log_file: Arc<Mutex<std::fs::File>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = stream.peer_addr()?;
    
    // Log connection
    if let Ok(mut file) = log_file.lock() {
        let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
        writeln!(file, "[{}] CONNECT {}", timestamp, addr)?;
    }

    println!("[INFO] Client connecté : {}", addr);

    let reader = BufReader::new(stream.try_clone()?);
    for line in reader.lines() {
        let line = line?;
        let command = line.trim();

        // Log command
        if let Ok(mut file) = log_file.lock() {
            let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");
            writeln!(file, "[{}] CMD {} > {}", timestamp, addr, command)?;
        }

        if command == "quit" {
            let response = format_response(&snapshot.lock().unwrap(), command);
            stream.write_all(response.as_bytes())?;
            break;
        }

        let response = format_response(&snapshot.lock().unwrap(), command);
        stream.write_all(response.as_bytes())?;
    }

    Ok(())
}

fn start_refresh_thread(snapshot: Arc<Mutex<SystemSnapshot>>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        loop {
            match collect_snapshot() {
                Ok(new_snapshot) => {
                    if let Ok(mut snap) = snapshot.lock() {
                        *snap = new_snapshot;
                    }
                }
                Err(e) => eprintln!("[ERROR] Failed to collect snapshot: {}", e),
            }
            thread::sleep(Duration::from_secs(5));
        }
    })
}

// ============================================================
// ÉTAPE 5 (BONUS) — Journalisation fichier
// ============================================================

fn create_log_file() -> Result<std::fs::File, std::io::Error> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .open("syswatch.log")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("[SysWatch] Serveur démarré sur le port 7878");

    // Initialize snapshot
    let initial_snapshot = collect_snapshot().unwrap_or_else(|_| SystemSnapshot {
        cpu: CpuInfo { usage_percent: 0.0, core_count: 0 },
        mem: MemInfo { total_kb: 0, used_kb: 0, free_kb: 0 },
        processes: Vec::new(),
        timestamp: "N/A".to_string(),
    });

    let snapshot = Arc::new(Mutex::new(initial_snapshot));
    let log_file = Arc::new(Mutex::new(create_log_file()?));

    // Start refresh thread
    let _refresh_handle = start_refresh_thread(snapshot.clone());

    // Start TCP server
    let listener = TcpListener::bind("0.0.0.0:7878")?;
    println!("[INFO] En écoute sur 0.0.0.0:7878");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let snapshot_clone = snapshot.clone();
                let log_file_clone = log_file.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, snapshot_clone, log_file_clone) {
                        eprintln!("[ERROR] Client handling error: {}", e);
                    }
                });
            }
            Err(e) => {
                eprintln!("[ERROR] Connection error: {}", e);
            }
        }
    }

    Ok(())
}
