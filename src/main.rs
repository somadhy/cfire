use rand::Rng;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
fn enable_windows_virtual_terminal() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return;
        }
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) == 0 {
            return;
        }
        let _ = SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
    }
}

#[derive(Clone, Copy)]
struct Cell {
    heat: u8,
}

fn main() {
    #[cfg(windows)]
    enable_windows_virtual_terminal();

    // ── Auto-detect terminal size ───────────────────────────────
    let (tc, tr) = terminal_size::terminal_size()
        .map(|(w, h)| (w.0 as usize, h.0 as usize))
        .unwrap_or((80, 24));
    let cols = tc.max(40);
    let rows = tr.max(16);

    // Reserve ~5 lines so the shell prompt doesn't vanish.
    let draw_rows = rows.saturating_sub(5).max(10);

    // ── Hidden cursor + clear ────────────────────────────────────
    {
        let mut out = io::stdout().lock();
        write!(out, "\x1b[?25l\x1b[H\x1b[2J").ok();
        out.flush().ok();
    }

    let mut grid = vec![vec![Cell { heat: 0 }; cols]; draw_rows];
    let mut next_grid = vec![vec![Cell { heat: 0 }; cols]; draw_rows];

    // ── Fuel bed shape ──────────────────────────────────────────
    let fuel_profile = build_fuel_profile(cols, draw_rows);
    let fuel_profile = Arc::new(fuel_profile);

    // ── Key watcher ──────────────────────────────────────────────
    let quit = Arc::new(AtomicBool::new(false));
    let quit_t = quit.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 1];
        loop {
            if let Ok(n) = io::stdin().lock().read(&mut buf) {
                if n > 0 && (buf[0] as char == 'q' || buf[0] as char == 'Q') {
                    quit_t.store(true, Ordering::Relaxed);
                    return;
                }
            }
            thread::sleep(Duration::from_millis(50));
        }
    });

    // ── Raw-mode on Unix ────────────────────────────────────────
    #[cfg(unix)]
    let saved_termios = setup_raw_unix();

    // ── Main loop ───────────────────────────────────────────────
    let target_frame = Duration::from_millis(90);

    for _ in 0.. {
        if quit.load(Ordering::Relaxed) {
            break;
        }
        let t = Instant::now();

        update_grid(
            &grid,
            &mut next_grid,
            &fuel_profile,
            cols,
            draw_rows,
            &mut rand::thread_rng(),
        );
        std::mem::swap(&mut grid, &mut next_grid);

        let mut out = io::stdout().lock();
        render(&mut out, &grid, cols, draw_rows, &mut rand::thread_rng());

        let elapsed = t.elapsed();
        if elapsed < target_frame {
            thread::sleep(target_frame - elapsed);
        }
    }

    // ── Restore terminal ────────────────────────────────────────
    #[cfg(unix)]
    restore_unix(saved_termios);

    {
        let mut out = io::stdout().lock();
        write!(out, "\x1b[?25h\x1b[H\x1b[2J").ok();
        out.flush().ok();
    }
    println!("cfire stopped  (q to quit)");
}

// ======================== Fuel profile ==========================

#[derive(Clone)]
struct FuelProfile {
    max_heat: Vec<u8>,
}

fn build_fuel_profile(cols: usize, _draw_rows: usize) -> FuelProfile {
    let center = cols as f64 / 2.0;
    let spread = cols as f64 / 5.0;
    let max_fuel_heat: f64 = 220.0;

    let mut max_heat = vec![0u8; cols];
    for i in 0..cols {
        let x = i as f64;
        let h1 = (-0.4 * ((x - center) / spread).powi(2)).exp() * max_fuel_heat;
        let h2 = (-0.8 * ((x - center * 0.72) / (spread * 0.6)).powi(2)).exp() * max_fuel_heat * 0.65;
        let h3 = (-0.8 * ((x - center * 1.28) / (spread * 0.6)).powi(2)).exp() * max_fuel_heat * 0.65;
        let combined = h1 + h2 + h3;
        max_heat[i] = combined.clamp(0.0, 255.0) as u8;
    }
    FuelProfile { max_heat }
}

// ======================== Cellular automaton ====================

const EMBER_DEPTH: usize = 4;

fn update_grid(
    prev: &[Vec<Cell>],
    next: &mut [Vec<Cell>],
    profile: &FuelProfile,
    cols: usize,
    rows: usize,
    rng: &mut impl Rng,
) {
    // ── Ember bed (bottom EMBER_DEPTH rows) ──┬────────────────────
    let ember_start = rows.saturating_sub(EMBER_DEPTH);
    for y in ember_start..rows {
        for x in 0..cols {
            let max_h = profile.max_heat[x] as i32;
            if max_h < 10 {
                next[y][x].heat = 0;
                continue;
            }
            let current = prev[y][x].heat as i32;
            let flicker = rng.gen_range(-15..20) as i32;
            let drift = (max_h - current) / 4;
            next[y][x].heat = (current + drift.max(0) + flicker)
                .clamp(0, max_h)
                .max(0) as u8;
        }
    }

    // ── Fire zone (everything above embers) ──┬───────────────────
    for y in (0..ember_start).rev() {
        for x in 0..cols {
            next[y][x].heat = compute_heat(prev, x, y, cols, rows, rng);
        }
    }
}

fn compute_heat(
    prev: &[Vec<Cell>],
    x: usize,
    y: usize,
    cols: usize,
    rows: usize,
    rng: &mut impl Rng,
) -> u8 {
    let below = heat(prev, x, y + 1, cols, rows);
    let left = heat(prev, x.saturating_sub(1), y, cols, rows);
    let right = heat(prev, (x + 1).min(cols - 1), y, cols, rows);

    // Updraft — heat rises
    let updraft = below;
    if updraft < 20 {
        return 0;
    }

    // Side heat blending
    let side_heat = (left as f32 + right as f32) * 0.1;

    // Cooling with height
    let height_ratio = y as f32 / rows.max(1) as f32;
    let cooling = (8.0 + height_ratio * 22.0 + rng.gen_range(-2.0..4.0)).max(2.0);

    // Convection column: hot columns rise with less cooling
    let column_boost: f32 = if updraft > 170 {
        rng.gen_range(0.0..0.2)
    } else {
        0.0
    };

    let new_heat = (updraft as f32 * (1.0 + column_boost)).max(cooling) - cooling + side_heat;
    let new_heat = if new_heat < 0.0 { 0.0 } else { new_heat };

    // Random extinction
    let die: u8 = 2 + (height_ratio * 28.0) as u8;
    if rng.gen_range(0..100) < die {
        return 0;
    }

    // Inertia
    let prev_h = heat(prev, x, y, cols, rows) as f32;
    let blended = new_heat * 0.7 + prev_h * 0.3;

    blended.clamp(0.0, 255.0) as u8
}

fn heat(g: &[Vec<Cell>], x: usize, y: usize, c: usize, r: usize) -> u8 {
    if x >= c || y >= r { 0 } else { g[y][x].heat }
}

// ======================== Render ================================

fn render<W: Write>(
    out: &mut W,
    grid: &[Vec<Cell>],
    cols: usize,
    rows: usize,
    rng: &mut impl Rng,
) {
    write!(out, "\x1b[H").ok();
    for y in 0..rows {
        for x in 0..cols {
            let h = grid[y][x].heat;
            if h == 0 {
                write!(out, " ").ok();
                continue;
            }
            let (ch, (r, g, b)) = glyph(h, rng);
            write!(out, "\x1b[38;2;{};{};{}m{}", r, g, b, ch).ok();
        }
        write!(out, "\x1b[0m\x1b[K\n").ok();
    }
    out.flush().ok();
}

fn glyph(h: u8, rng: &mut impl Rng) -> (char, (u8, u8, u8)) {
    let (r, g, b) = color(h);
    let ch = if h > 200 {
        ['█', '▓', '▓', '▒'][rng.gen_range(0..4)]
    } else if h > 150 {
        ['▓', '▒', '░', '/', '\\', '|'][rng.gen_range(0..6)]
    } else if h > 80 {
        ['░', '~', '/', '\\'][rng.gen_range(0..4)]
    } else {
        ['░', '·', ' '][rng.gen_range(0..3)]
    };
    (ch, (r, g, b))
}

fn color(h: u8) -> (u8, u8, u8) {
    let t = h as f64 / 255.0;
    let (r, gv, b) = if t < 0.2 {
        let s = t / 0.2;
        (40.0 + 100.0 * s, 5.0 + 5.0 * s, 0.0)
    } else if t < 0.45 {
        let s = (t - 0.2) / 0.25;
        (140.0 + 85.0 * s, 10.0 + 40.0 * s, 0.0)
    } else if t < 0.7 {
        let s = (t - 0.45) / 0.25;
        (225.0 + 30.0 * s, 50.0 + 130.0 * s, 0.0)
    } else {
        let s = (t - 0.7) / 0.3;
        (255.0, 180.0 + 75.0 * s, 0.0 + 40.0 * s)
    };
    (r as u8, gv as u8, b as u8)
}

// ============ Unix raw-mode helpers (non-blocking stdin) ========

#[cfg(unix)]
fn setup_raw_unix() -> Option<libc::termios> {
    unsafe {
        let mut old = std::mem::zeroed::<libc::termios>();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut old) != 0 {
            return None;
        }
        let mut new = old;
        new.c_lflag &= !(libc::ICANON | libc::ECHO);
        new.c_cc[libc::VMIN] = 0;
        new.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &new);
        Some(old)
    }
}

#[cfg(unix)]
fn restore_unix(old: Option<libc::termios>) {
    if let Some(o) = old {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &o);
        }
    }
}
