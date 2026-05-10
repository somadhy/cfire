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

/// Raw-ish stdin so `Read` returns each key without waiting for Enter (required for `q`).
#[cfg(windows)]
fn setup_windows_stdin() -> Option<u32> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        STD_INPUT_HANDLE,
    };
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) == 0 {
            return None;
        }
        let old = mode;
        let new_mode = mode & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT);
        if SetConsoleMode(h, new_mode) == 0 {
            return None;
        }
        Some(old)
    }
}

#[cfg(windows)]
fn restore_windows_stdin(old: Option<u32>) {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE};
    if let Some(m) = old {
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            if h != INVALID_HANDLE_VALUE {
                let _ = SetConsoleMode(h, m);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Cell {
    heat: u8,
}

fn print_usage() {
    println!(
        "\
Usage: cfire [OPTIONS]

Options:
    -L, --lock    Screen lock (cmatrix -L style): no q to quit; Ctrl+C ignored;
                  stop from another session: kill <PID>
    -h, --help    Print this help"
    );
}

#[cfg(unix)]
fn lock_ignore_signals() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
    }
}

#[cfg(windows)]
fn lock_ignore_ctrl_c() {
    use windows_sys::Win32::Foundation::TRUE;
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn handler(_ctrl_type: u32) -> i32 {
        TRUE
    }

    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), TRUE);
    }
}

/// Quit from stdin bytes: Latin `q`/`Q`, or UTF-8 Cyrillic `й`/`Й` (same key as `q` in Russian layout).
fn quit_watcher_utf8(quit: Arc<AtomicBool>) {
    let mut pending_d0 = false;
    let mut buf = [0u8; 1];
    loop {
        let n = match io::stdin().lock().read(&mut buf) {
            Ok(n) => n,
            Err(_) => {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        if n == 0 {
            thread::sleep(Duration::from_millis(15));
            continue;
        }
        let b = buf[0];
        if pending_d0 {
            pending_d0 = false;
            if b == 0xB9 || b == 0x99 {
                quit.store(true, Ordering::Relaxed);
                return;
            }
        }
        if b == b'q' || b == b'Q' {
            quit.store(true, Ordering::Relaxed);
            return;
        }
        if b == 0xD0 {
            pending_d0 = true;
        }
    }
}

/// Windows: virtual key `VK_Q` — same physical key as Latin `q` regardless of layout (RU `й`, etc.).
#[cfg(windows)]
fn quit_watcher_windows_console(quit: Arc<AtomicBool>) {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{
        GetStdHandle, ReadConsoleInputW, INPUT_RECORD, KEY_EVENT, STD_INPUT_HANDLE,
    };

    const VK_Q: u16 = 0x51;

    let h = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if h == INVALID_HANDLE_VALUE {
        quit_watcher_utf8(quit);
        return;
    }

    loop {
        let mut rec: INPUT_RECORD = unsafe { std::mem::zeroed() };
        let mut n_read = 0u32;
        let ok = unsafe { ReadConsoleInputW(h, &mut rec, 1, &mut n_read) };
        if ok == 0 || n_read == 0 {
            thread::sleep(Duration::from_millis(15));
            continue;
        }
        if rec.EventType as u32 != KEY_EVENT {
            continue;
        }
        let ke = unsafe { rec.Event.KeyEvent };
        if ke.bKeyDown == 0 {
            continue;
        }
        if ke.wVirtualKeyCode == VK_Q {
            quit.store(true, Ordering::Relaxed);
            return;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }
    let lock_mode = args.iter().any(|a| a == "--lock" || a == "-L");

    #[cfg(unix)]
    if lock_mode {
        lock_ignore_signals();
    }
    #[cfg(windows)]
    if lock_mode {
        lock_ignore_ctrl_c();
    }

    if lock_mode {
        let pid = std::process::id();
        eprintln!(
            "cfire: lock mode — PID {pid}. Exit from another tty/session: kill {pid}   (like cmatrix -L)"
        );
    }

    #[cfg(windows)]
    enable_windows_virtual_terminal();
    #[cfg(windows)]
    let saved_stdin_mode = if !lock_mode {
        setup_windows_stdin()
    } else {
        None
    };

    // ── Auto-detect terminal size ───────────────────────────────
    let (tc, tr) = terminal_size::terminal_size()
        .map(|(w, h)| (w.0 as usize, h.0 as usize))
        .unwrap_or((80, 24));
    let cols = tc.max(40);
    let rows = tr.max(16);

    // Use full terminal height so the fuel bed sits on the bottom row.
    let draw_rows = rows.max(10);

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

    // ── Raw-mode on Unix (only when reading `q` in a background thread) ──
    #[cfg(unix)]
    let saved_termios = if !lock_mode {
        setup_raw_unix()
    } else {
        None
    };

    // ── Key watcher (disabled in --lock: exit only via kill from elsewhere) ──
    let quit = Arc::new(AtomicBool::new(false));
    if !lock_mode {
        let quit_t = quit.clone();
        #[cfg(windows)]
        thread::spawn(move || quit_watcher_windows_console(quit_t));
        #[cfg(not(windows))]
        thread::spawn(move || quit_watcher_utf8(quit_t));
    }

    // ── Main loop ───────────────────────────────────────────────
    let target_frame = Duration::from_millis(90);

    loop {
        if !lock_mode && quit.load(Ordering::Relaxed) {
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
    #[cfg(windows)]
    restore_windows_stdin(saved_stdin_mode);

    {
        let mut out = io::stdout().lock();
        // Reset colors/styles, show cursor, clear screen — leave a blank terminal for the shell prompt.
        write!(out, "\x1b[0m\x1b[?25h\x1b[H\x1b[2J").ok();
        out.flush().ok();
    }
}

// ======================== Fuel profile ==========================

#[derive(Clone)]
struct FuelProfile {
    max_heat: Vec<u8>,
}

fn build_fuel_profile(cols: usize, _draw_rows: usize) -> FuelProfile {
    let center = (cols.saturating_sub(1)) as f64 / 2.0;
    let w = cols as f64;
    // Wide warm bed for coals at the sides + tighter, taller peak so the core reads as a bonfire pile.
    let wide_span = (w * 0.41).max(6.0);
    let peak_span = (w * 0.088).max(2.0);

    let mut max_heat = vec![0u8; cols];
    for i in 0..cols {
        let x = i as f64;
        let dw = (x - center) / wide_span;
        let dp = (x - center) / peak_span;
        let floor = (-0.44 * dw * dw).exp() * 148.0;
        let peak = (-1.05 * dp * dp).exp() * 125.0;
        let h = (floor + peak).min(255.0);
        max_heat[i] = h as u8;
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
            let flicker = rng.gen_range(-8..16) as i32;
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
    let ember_start = rows.saturating_sub(EMBER_DEPTH);
    let fire_h = ember_start.max(1);
    // 0 at the flame base (just above coals), 1 at the top of the terminal
    let rise =
        (ember_start.saturating_sub(1).saturating_sub(y)) as f32 / fire_h as f32;

    let cx = (cols.saturating_sub(1)) as f32 / 2.0;
    // 0 = screen center column, 1 = far left/right (short rim of the fire)
    let edge = ((x as f32 - cx).abs() / (cx + 1.0)).min(1.0);
    // Cone: near the coals (low rise) flames can span wide; toward the top, sides cool faster → tall central spike (campfire, not flat grill).
    let cone_edge = (edge * (0.13 + 0.87 * rise.powf(1.03))).min(1.0);

    let drift_prob =
        (rise as f64 * 0.36 * (edge as f64).powf(0.82)).clamp(0.0, 0.40);
    let sample_x = if rise > 0.18 && rng.gen_bool(drift_prob) {
        let sx = x as isize + rng.gen_range(-1..=1);
        sx.clamp(0, cols as isize - 1) as usize
    } else {
        x
    };

    let below = heat(prev, sample_x, y + 1, cols, rows);
    let left = heat(prev, x.saturating_sub(1), y, cols, rows);
    let right = heat(prev, (x + 1).min(cols - 1), y, cols, rows);

    // Updraft — heat rises
    let updraft = below;
    if updraft < 10 {
        return 0;
    }

    let side_heat = (left as f32 + right as f32) * 0.105 * (1.0 - 0.14 * edge);

    let cooling_base = (3.5 + rise * 21.0 + rng.gen_range(-2.0..4.0)).max(1.7);
    // cone_edge: strongest cooling off-axis and high up → pronounced vertical core
    let cooling = cooling_base * (0.52 + 1.12 * cone_edge * cone_edge);

    let center_lift = (1.0 - edge).powf(2.15);
    let column_boost: f32 = if updraft > 108 {
        rng.gen_range(0.0..0.24) + center_lift * rng.gen_range(0.0..0.78)
    } else {
        0.0
    };

    let new_heat = (updraft as f32 * (1.0 + column_boost)).max(cooling) - cooling + side_heat;
    let new_heat = if new_heat < 0.0 { 0.0 } else { new_heat };

    let die: u8 = (rise * (14.0 + 11.0 * cone_edge)) as u8;
    if die > 0 && rng.gen_range(0..100) < die {
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
    // Avoid FULL BLOCK (█): in many fonts it reads as a flat black tile; use shaded blocks instead.
    let ch = if h > 188 {
        ['▓', '▓', '▒', '▒', '░', '▓'][rng.gen_range(0..6)]
    } else if h > 145 {
        ['▓', '▒', '░', '/', '\\', '|'][rng.gen_range(0..6)]
    } else if h > 72 {
        ['░', '~', '/', '\\', '░'][rng.gen_range(0..5)]
    } else {
        ['░', '·', ':', '░', '·'][rng.gen_range(0..5)]
    };
    (ch, (r, g, b))
}

fn color(h: u8) -> (u8, u8, u8) {
    let t = h as f64 / 255.0;
    let (r, gv, b) = if t < 0.2 {
        let s = t / 0.2;
        // Visible embers without muddy black
        (78.0 + 82.0 * s, 14.0 + 18.0 * s, 1.0 + 5.0 * s)
    } else if t < 0.45 {
        let s = (t - 0.2) / 0.25;
        (155.0 + 85.0 * s, 32.0 + 58.0 * s, 6.0 + 14.0 * s)
    } else if t < 0.7 {
        let s = (t - 0.45) / 0.25;
        (240.0 + 15.0 * s, 90.0 + 88.0 * s, 20.0 + 48.0 * s)
    } else {
        let s = (t - 0.7) / 0.3;
        // Peaks stay orange–amber, not paper-white
        (255.0, 128.0 + 72.0 * s, 28.0 + 48.0 * s)
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
