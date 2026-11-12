//! Termarium – interaktywny wizualizer procesów TUI.
//! Rybki = procesy (rozmiar ~ RAM, prędkość ~ CPU), fale = globalne CPU.

use std::{
    collections::HashMap,
    io::{self, BufWriter, Write},
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal::{
        self, BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use sysinfo::{Components, ProcessesToUpdate, System};

const FRAME: Duration = Duration::from_millis(50); // ~20 FPS
const REFRESH: Duration = Duration::from_millis(1000); // odświeżanie sysinfo
const OCEAN_ROWS: usize = 4;
const MAX_FISH: usize = 14;
const MIB: u64 = 1024 * 1024;

// Poziomy wypełnienia komórki w ósemkach: indeks 0 = pusto, 8 = pełny blok.
const LEVELS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
// Pionowy pasek w Braille'u do miniwykresu CPU w stopce.
const BRAILLE_BAR: [char; 8] = [' ', '⣀', '⣄', '⣤', '⣦', '⣶', '⣷', '⣿'];
const BUBBLES: [char; 4] = ['⠂', '⠄', '⠐', '⠠'];

// ---------------------------------------------------------------- terminal

/// Przywraca terminal nawet przy panice (Drop działa przy unwindingu).
struct TerminalGuard;

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), ResetColor, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

// ------------------------------------------------------------------ bufor

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    fg: Color,
}

const EMPTY: Cell = Cell { ch: ' ', fg: Color::Reset };

struct Frame {
    w: usize,
    h: usize,
    cells: Vec<Cell>,
}

impl Frame {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, cells: vec![EMPTY; w * h] }
    }

    fn put(&mut self, x: i32, y: i32, ch: char, fg: Color) {
        if x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h {
            self.cells[y as usize * self.w + x as usize] = Cell { ch, fg };
        }
    }

    fn text(&mut self, x: i32, y: i32, s: &str, fg: Color) {
        for (i, ch) in s.chars().enumerate() {
            self.put(x + i as i32, y, ch, fg);
        }
    }

    /// Wysyła klatkę do terminala; kolejne komórki o tym samym kolorze
    /// są łączone w jeden `Print`, a całość owinięta w synchronized update.
    fn flush_to<W: Write>(&self, out: &mut W) -> io::Result<()> {
        queue!(out, BeginSynchronizedUpdate)?;
        for y in 0..self.h {
            queue!(out, MoveTo(0, y as u16))?;
            let row = &self.cells[y * self.w..(y + 1) * self.w];
            let mut x = 0;
            while x < row.len() {
                let fg = row[x].fg;
                let mut run = String::new();
                while x < row.len() && row[x].fg == fg {
                    run.push(row[x].ch);
                    x += 1;
                }
                queue!(out, SetForegroundColor(fg), Print(run))?;
            }
        }
        queue!(out, ResetColor, EndSynchronizedUpdate)?;
        out.flush()
    }
}

// ------------------------------------------------------------------ rybki

struct Fish {
    pid: u32,
    name: String,
    mem: u64,
    cpu: f32,
    x: f32,
    dir: f32, // +1.0 w prawo, -1.0 w lewo
}

fn mirror(s: &str) -> String {
    s.chars()
        .rev()
        .map(|c| match c {
            '<' => '>',
            '>' => '<',
            '(' => ')',
            ')' => '(',
            other => other,
        })
        .collect()
}

impl Fish {
    fn body(&self) -> String {
        let right = if self.mem < 300 * MIB {
            "><>"
        } else if self.mem < 1024 * MIB {
            "><((º>"
        } else {
            "><(((º>"
        };
        if self.dir > 0.0 {
            right.to_string()
        } else {
            mirror(right)
        }
    }

    fn color(&self) -> Color {
        if self.mem < 300 * MIB {
            Color::Yellow
        } else if self.mem < 1024 * MIB {
            Color::Magenta
        } else {
            Color::Red
        }
    }

    /// Prędkość w komórkach na sekundę – rośnie wraz z CPU procesu.
    fn speed(&self) -> f32 {
        (3.0 + self.cpu * 0.5).min(60.0)
    }

    fn step(&mut self, dt: f32, width: usize) {
        let bw = self.body().chars().count() as f32;
        self.x += self.dir * self.speed() * dt;
        let max_x = (width as f32 - bw).max(0.0);
        if self.x <= 0.0 {
            self.x = 0.0;
            self.dir = 1.0;
        } else if self.x >= max_x {
            self.x = max_x;
            self.dir = -1.0;
        }
    }
}

/// Odbudowuje listę rybek z top-N procesów wg RAM, zachowując pozycje znanych PID-ów.
fn rebuild_fish(fishes: &mut Vec<Fish>, sys: &System, n: usize, width: usize) {
    let mut procs: Vec<_> = sys
        .processes()
        .values()
        // Na Linuksie sysinfo zwraca też wątki – pomijamy, by nie dublować pamięci.
        .filter(|p| p.thread_kind().is_none())
        .collect();
    procs.sort_by(|a, b| b.memory().cmp(&a.memory()));

    let old: HashMap<u32, (f32, f32)> = fishes.iter().map(|f| (f.pid, (f.x, f.dir))).collect();
    *fishes = procs
        .into_iter()
        .take(n)
        .map(|p| {
            let pid = p.pid().as_u32();
            let (x, dir) = old.get(&pid).copied().unwrap_or((
                (pid as usize * 7 % width.max(1)) as f32,
                if pid % 2 == 0 { 1.0 } else { -1.0 },
            ));
            Fish {
                pid,
                name: p.name().to_string_lossy().into_owned(),
                mem: p.memory(),
                cpu: p.cpu_usage(),
                x,
                dir,
            }
        })
        .collect();
}

// ----------------------------------------------------------------- bąbelki

struct Bubble {
    x: f32,
    y: f32,
    v: f32,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn unit(&mut self) -> f32 {
        (self.next() % 10_000) as f32 / 10_000.0
    }
}

// ------------------------------------------------------------------ pomiary

fn cpu_temperature() -> Option<f32> {
    let comps = Components::new_with_refreshed_list();
    let mut best: Option<f32> = None;
    let mut fallback: Option<f32> = None;
    for c in comps.list() {
        let t = c.temperature();
        if !t.is_finite() || t <= 0.0 {
            continue;
        }
        let label = c.label().to_lowercase();
        if label.contains("cpu")
            || label.contains("package")
            || label.contains("tctl")
            || label.contains("core")
        {
            best = Some(best.map_or(t, |b| b.max(t)));
        }
        fallback = Some(fallback.map_or(t, |b| b.max(t)));
    }
    best.or(fallback)
}

fn braille_bar(pct: f32, width: usize) -> String {
    let filled = (pct.clamp(0.0, 100.0) / 100.0 * width as f32).round() as usize;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(width - filled))
}

// ------------------------------------------------------------------ render

struct Stats {
    cpu: f32,
    temp: Option<f32>,
    used: u64,
    total: u64,
}

fn render(
    frame: &mut Frame,
    fishes: &[Fish],
    bubbles: &[Bubble],
    stats: &Stats,
    cpu_smooth: f32,
    t: f32,
) {
    let (w, h) = (frame.w as i32, frame.h as i32);
    let ocean_top = h - 1 - OCEAN_ROWS as i32; // pierwszy wiersz oceanu
    let water_h = ocean_top.max(1);

    // Bąbelki
    for b in bubbles {
        let ch = BUBBLES[(b.y as usize + b.x as usize) % BUBBLES.len()];
        frame.put(b.x as i32, b.y as i32, ch, Color::DarkCyan);
    }

    // Rybki + podpisy
    let slots = ((water_h / 2).max(1)) as usize;
    for (i, f) in fishes.iter().enumerate() {
        let y = ((i % slots) * 2) as i32;
        frame.text(f.x as i32, y, &f.body(), f.color());
        let name: String = f.name.chars().take(16).collect();
        let label = format!("{} | {} MB RAM | {:.0}% CPU", name, f.mem / MIB, f.cpu);
        let lx = (f.x as i32).min(w - label.chars().count() as i32).max(0);
        frame.text(lx, y + 1, &label, Color::Grey);
    }

    // Fale: wysokość i prędkość rosną z obciążeniem CPU
    let load = cpu_smooth.clamp(0.0, 100.0) / 100.0;
    let total_eighths = (OCEAN_ROWS * 8) as f32;
    let base = 6.0 + load * 14.0;
    let amp = 2.0 + load * 5.0;
    let wave_speed = 1.5 + load * 5.0;
    for x in 0..w {
        let xf = x as f32;
        let e = base
            + (xf * 0.35 + t * wave_speed).sin() * amp
            + (xf * 0.13 - t * 0.7).sin() * 2.0;
        let e = e.clamp(2.0, total_eighths) as i32;
        for r in 0..OCEAN_ROWS as i32 {
            let cell8 = (e - r * 8).clamp(0, 8);
            if cell8 == 0 {
                continue;
            }
            let is_top = e - (r + 1) * 8 <= 0;
            let color = if is_top { Color::Cyan } else { Color::Blue };
            frame.put(x, h - 2 - r, LEVELS[cell8 as usize], color);
        }
    }

    // Stopka
    let temp = stats.temp.map_or("n/a".to_string(), |t| format!("{t:.0}°C"));
    let footer = format!(
        " CPU {:5.1}% {} │ CPU temp {} │ RAM {}/{} MB ({:.0}%) │ q: wyjście ",
        stats.cpu,
        braille_bar(stats.cpu, 10),
        temp,
        stats.used / MIB,
        stats.total / MIB,
        if stats.total > 0 { stats.used as f64 / stats.total as f64 * 100.0 } else { 0.0 },
    );
    frame.text(0, h - 1, &footer, Color::Green);
}

// --------------------------------------------------------------------- main

fn is_quit(ev: &Event) -> bool {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => {
            matches!(k.code, KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc)
                || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
        }
        _ => false,
    }
}

fn main() -> io::Result<()> {
    let _guard = TerminalGuard::new()?;
    let mut out = BufWriter::new(io::stdout());

    let mut sys = System::new();
    sys.refresh_cpu_usage();
    sys.refresh_memory();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    std::thread::sleep(Duration::from_millis(250)); // minimalny odstęp dla pomiaru CPU

    let mut fishes: Vec<Fish> = Vec::new();
    let mut bubbles: Vec<Bubble> = Vec::new();
    let mut rng = Rng(0x5EED_F155_u64 ^ std::process::id() as u64);
    let mut stats = Stats { cpu: 0.0, temp: None, used: 0, total: 0 };
    let mut cpu_smooth = 0.0f32;
    let mut last_refresh = Instant::now() - REFRESH;
    let start = Instant::now();
    let mut last = Instant::now();
    let mut bubble_timer = 0.0f32;

    loop {
        let frame_start = Instant::now();
        let dt = (frame_start - last).as_secs_f32();
        last = frame_start;
        let t = (frame_start - start).as_secs_f32();

        let (tw, th) = terminal::size()?;
        let (w, h) = (tw as usize, th as usize);

        // Odświeżanie danych systemowych raz na sekundę
        if last_refresh.elapsed() >= REFRESH {
            last_refresh = Instant::now();
            sys.refresh_cpu_usage();
            sys.refresh_memory();
            sys.refresh_processes(ProcessesToUpdate::All, true);
            stats = Stats {
                cpu: sys.global_cpu_usage(),
                temp: cpu_temperature(),
                used: sys.used_memory(),
                total: sys.total_memory(),
            };
            let water_h = h.saturating_sub(1 + OCEAN_ROWS).max(1);
            let n = MAX_FISH.min((water_h / 2).max(1) * 2);
            rebuild_fish(&mut fishes, &sys, n, w);
        }

        cpu_smooth += (stats.cpu - cpu_smooth) * 0.08;

        if w < 30 || h < 10 {
            // Zbyt małe okno – pokaż komunikat zamiast rysować akwarium.
            let mut frame = Frame::new(w.max(1), h.max(1));
            frame.text(0, 0, "Za male okno", Color::Red);
            frame.flush_to(&mut out)?;
        } else {
            for f in fishes.iter_mut() {
                f.step(dt, w);
            }

            // Bąbelki: spawn z dna wody, unoszą się w górę
            let ocean_top = (h - 1 - OCEAN_ROWS) as f32;
            bubble_timer += dt;
            if bubble_timer > 0.35 {
                bubble_timer = 0.0;
                bubbles.push(Bubble {
                    x: rng.unit() * w as f32,
                    y: ocean_top,
                    v: 2.0 + rng.unit() * 3.0,
                });
            }
            for b in bubbles.iter_mut() {
                b.y -= b.v * dt;
                b.x += (t * 2.0 + b.y).sin() * 0.3 * dt * 10.0;
            }
            bubbles.retain(|b| b.y >= 0.0);

            let mut frame = Frame::new(w, h);
            render(&mut frame, &fishes, &bubbles, &stats, cpu_smooth, t);
            frame.flush_to(&mut out)?;
        }

        // Czekaj do następnej klatki, reagując na klawisze
        let elapsed = frame_start.elapsed();
        if elapsed < FRAME && event::poll(FRAME - elapsed)? {
            if is_quit(&event::read()?) {
                break;
            }
        }
        while event::poll(Duration::ZERO)? {
            if is_quit(&event::read()?) {
                return Ok(());
            }
        }
    }
    Ok(())
}
