//! Reads sensor values and appends them to a CSV file on the DFS mount.

use std::fs::OpenOptions;
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const OUTPUT: &str = "/dfs/projects/telemetry/readings.csv";

fn read_sensor(id: u32) -> (f32, f32) {
    // Stand-in for a real driver.
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let temperature = 18.0 + ((t / 60 + id as u64) % 70) as f32 / 10.0;
    let humidity = 45.0 + ((t / 300 + id as u64) % 25) as f32;
    (temperature, humidity)
}

fn main() -> std::io::Result<()> {
    loop {
        let mut out = OpenOptions::new().create(true).append(true).open(OUTPUT)?;
        for id in 0..4 {
            let (t, h) = read_sensor(id);
            writeln!(out, "{},sensor-{id},{t:.1},{h:.0}", chrono_like_now())?;
        }
        std::thread::sleep(Duration::from_secs(900));
    }
}

fn chrono_like_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}
