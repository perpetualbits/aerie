// SPDX-License-Identifier: GPL-3.0-or-later
//
// constellation — spike for aerie's unifying "constellation face".
//
// Live /proc process-groups become nodes in one semantically-zoomable graph:
// backbone edges are lineage (parent comm -> child comm), node size = CPU,
// heat = memory, pulse = strain. A --stall injector fakes a periodic system
// stall so we can feel the notice -> materialize -> dive arc before wiring the
// real Instruments subsystem. Standalone: mullion + crossterm only.
//
// Run:  cargo run --bin constellation [--stall]
// Keys: q / Esc / Ctrl-C  quit

use anyhow::Result;
use crossterm::event::Event;
use mullion::backend::CrosstermBackend;
use mullion::capabilities::Capabilities;
use mullion::input::{KeyCode, KeyModifiers};
use mullion::style::{Color, Style};
use mullion::{Buffer, EventReader, Rect, Terminal};
use std::io;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(33); // ~30 fps

const HELP: &str = "\
constellation — aerie unifying-face spike

USAGE: constellation [--stall]
  --stall   drive the fake periodic-stall injector on startup
  -h,--help show this help

KEYS: q/Esc quit\n";

struct State {
    stall: bool,
    t: f32, // seconds since start
}

impl State {
    fn new(stall: bool) -> Self {
        State { stall, t: 0.0 }
    }

    fn advance(&mut self, dt: f32) {
        self.t += dt;
    }

    fn render(&self, buf: &mut Buffer) {
        let area = buf.area;
        let msg = format!("constellation spike — t={:.1}s stall={}", self.t, self.stall);
        buf.set_string(area.x + 1, area.y, &msg, Style::default().fg(Color::White));
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return Ok(());
    }
    let stall = args.iter().any(|a| a == "--stall");

    let mut state = State::new(stall);
    let mut backend = CrosstermBackend::new(io::stdout());
    backend.apply_capabilities(&Capabilities::detect());
    let mut terminal = Terminal::new(backend)?;
    terminal.enter()?;
    let input = EventReader::new();
    let mut last = Instant::now();

    let result: Result<()> = (|| {
        'frames: loop {
            let frame_start = Instant::now();
            let dt = frame_start.duration_since(last).as_secs_f32().min(0.1);
            last = frame_start;

            for ev in input.drain() {
                if let Event::Key(key) = ev {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break 'frames,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break 'frames
                        }
                        _ => {}
                    }
                }
            }

            state.advance(dt);
            terminal.draw(|buf| state.render(buf))?;
            std::thread::sleep(FRAME.saturating_sub(frame_start.elapsed()));
        }
        Ok(())
    })();

    terminal.leave()?;
    result
}

#[derive(Debug, Clone)]
struct ProcStat {
    pid: u32,
    ppid: u32,
    comm: String,
    cpu_jiffies: u64,
}

fn parse_stat(line: &str) -> Option<ProcStat> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    if close <= open {
        return None;
    }
    let pid: u32 = line[..open].trim().parse().ok()?;
    let comm = line[open + 1..close].to_string();
    // Remainder starts at field 3 (state). 0-based within remainder:
    //   state=0, ppid=1, ... utime=10, stime=11
    let rest: Vec<&str> = line[close + 1..].split_whitespace().collect();
    if rest.len() < 12 {
        return None;
    }
    let ppid: u32 = rest[1].parse().ok()?;
    let utime: u64 = rest[10].parse().ok()?;
    let stime: u64 = rest[11].parse().ok()?;
    Some(ProcStat { pid, ppid, comm, cpu_jiffies: utime + stime })
}

fn parse_statm_resident(line: &str) -> Option<u64> {
    line.split_whitespace().nth(1)?.parse().ok()
}

#[derive(Debug, Clone)]
struct ProcSample {
    pid: u32,
    ppid: u32,
    comm: String,
    cpu_jiffies: u64,
    rss_pages: u64,
}

fn sample_procs() -> Vec<ProcSample> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let base = entry.path();
        let Ok(stat_line) = std::fs::read_to_string(base.join("stat")) else { continue };
        let Some(st) = parse_stat(&stat_line) else { continue };
        let rss_pages = std::fs::read_to_string(base.join("statm"))
            .ok()
            .and_then(|s| parse_statm_resident(&s))
            .unwrap_or(0);
        out.push(ProcSample {
            pid: st.pid,
            ppid: st.ppid,
            comm: st.comm,
            cpu_jiffies: st.cpu_jiffies,
            rss_pages,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stat_with_parens_in_comm() {
        // Real-shaped line: comm "(a b)c" contains spaces and parens.
        // fields: 1 pid, 2 comm, 3 state, 4 ppid, ... 14 utime, 15 stime
        let line = "1234 ((a b)c) S 1000 1234 1234 0 -1 0 0 0 0 40 60 0 0 20 0 1 0";
        let s = parse_stat(line).expect("should parse");
        assert_eq!(s.pid, 1234);
        assert_eq!(s.ppid, 1000);
        assert_eq!(s.comm, "(a b)c");
        assert_eq!(s.cpu_jiffies, 100); // utime 40 + stime 60
    }

    #[test]
    fn rejects_garbage_stat() {
        assert!(parse_stat("not a stat line").is_none());
    }

    #[test]
    fn parses_statm_resident() {
        // size resident shared ... — we want the 2nd field.
        assert_eq!(parse_statm_resident("5000 1234 200 10 0 300 0"), Some(1234));
        assert_eq!(parse_statm_resident("bad"), None);
    }
}
