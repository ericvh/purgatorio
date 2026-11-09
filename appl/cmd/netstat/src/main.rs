//! netstat: list conversations under `/net/tcp` and `/net/udp`.

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, _args: Vec<String>) -> Result<(), Error> {
    println!(
        "{:<6} {:<6} {:<24} {:<24} status",
        "proto", "conv", "local", "remote"
    );
    for proto in ["tcp", "udp"] {
        let Ok(dir) = std::fs::read_dir(format!("/net/{proto}")) else {
            continue;
        };
        let mut names: Vec<String> = dir
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
            .collect();
        names.sort();
        for name in names {
            let base = format!("/net/{proto}/{name}");
            let local = read_trim(&format!("{base}/local"));
            let remote = read_trim(&format!("{base}/remote"));
            let status = read_trim(&format!("{base}/status"));
            println!("{proto:<6} {name:<6} {local:<24} {remote:<24} {status}");
        }
    }
    Ok(())
}

fn read_trim(path: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}
