use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::{
    cursor::{Hide, Show},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame, Terminal,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::IsTerminal;
use std::{
    borrow::Cow,
    fs,
    io::{self, BufRead, Read, Write},
    net::TcpStream,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

static NEXT_ID: AtomicU32 = AtomicU32::new(1);
fn next_id() -> u32 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}
static SIMPLE_MODE: AtomicBool = AtomicBool::new(false);
static MEMORY_MODE: AtomicBool = AtomicBool::new(false);

// ── data model ────────────────────────────────────────────────────────────────

/// Represents a single command entry in the history, including its input, output, and repetition state.
#[derive(Debug)]
struct Entry {
    id: u32,
    cmd: String,
    cursor_pos: usize, // byte offset
    sel_anchor: Option<usize>, // byte offset of selection anchor; None = no selection
    output: String,
    output_sel: Option<(usize, usize)>, // (anchor, cursor) byte offsets in output
    is_err: bool,
    repeat_delay: Option<Duration>,
    repeat_token: u32,
    flash: bool,
    cancel: Option<Arc<AtomicBool>>,
    is_pending: bool,
    is_waiting: bool,
    exec_id: u32,
}

impl Default for Entry {
    fn default() -> Self {
        Self {
            id: next_id(),
            cmd: String::new(),
            cursor_pos: 0,
            sel_anchor: None,
            output: String::new(),
            output_sel: None,
            is_err: false,
            repeat_delay: None,
            repeat_token: 0,
            flash: false,
            cancel: None,
            is_pending: false,
            is_waiting: false,
            exec_id: 0,
        }
    }
}
#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
enum CommandResult {
    Output { output: String },
    Error { error: String },
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct MemoryData {
    active_command_index: usize,
    commands: Vec<String>,
    #[serde(default)]
    results: Vec<CommandResult>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct HostEntry {
    addr: String,
    count: u32,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct HostsData {
    hosts: Vec<HostEntry>,
}
fn ensure_private_dir(path: &std::path::Path) -> io::Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn write_private_file(path: &std::path::Path, content: &str) -> io::Result<()> {
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn dqcon_dir() -> Option<PathBuf> {
    let base = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    let mut path = PathBuf::from(base);
    path.push(".dqcon");
    let _ = ensure_private_dir(&path);
    Some(path)
}

fn load_json_file<T: DeserializeOwned + Default>(path: Option<PathBuf>) -> T {
    path.and_then(|path| fs::read_to_string(path).ok())
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

fn save_json_file<T: Serialize>(path: Option<PathBuf>, data: &T) -> io::Result<()> {
    if let Some(path) = path {
        if let Some(parent) = path.parent() {
            let _ = ensure_private_dir(parent);
        }
        let content = serde_json::to_string_pretty(data)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        write_private_file(&path, &content)?;
    }
    Ok(())
}

fn memory_to_entries(mem: MemoryData) -> (Vec<Entry>, usize) {
    let mut loaded_entries = Vec::new();
    for (i, c) in mem.commands.into_iter().enumerate() {
        let mut e = Entry::default();
        e.cmd = c;
        e.cursor_pos = e.cmd.len();
        if let Some(res) = mem.results.get(i) {
            match res {
                CommandResult::Output { output } => {
                    e.output = output.clone();
                    e.is_err = false;
                }
                CommandResult::Error { error } => {
                    e.output = error.clone();
                    e.is_err = true;
                }
            }
        }
        loaded_entries.push(e);
    }
    if loaded_entries.is_empty() {
        loaded_entries.push(Entry::default());
    }
    ensure_tail(&mut loaded_entries);
    let cur = mem.active_command_index.min(loaded_entries.len() - 1);
    (loaded_entries, cur)
}

fn entries_to_memory(entries: &[Entry], cur: usize) -> MemoryData {
    let mut commands = Vec::new();
    let mut results = Vec::new();
    for e in entries.iter() {
        if !e.cmd.is_empty() {
            commands.push(e.cmd.clone());
            results.push(if e.is_err {
                CommandResult::Error {
                    error: e.output.clone(),
                }
            } else {
                CommandResult::Output {
                    output: e.output.clone(),
                }
            });
        }
    }
    MemoryData {
        active_command_index: cur,
        commands,
        results,
    }
}

fn save_entries_memory_if_enabled(
    host: &str,
    port: &str,
    user: &str,
    entries: &[Entry],
    cur: usize,
) {
    if MEMORY_MODE.load(Ordering::Relaxed) {
        let mem = entries_to_memory(entries, cur);
        let _ = save_memory(host, port, user, &mem);
    }
}

fn get_memory_path(host: &str, port: &str, user: &str) -> Option<PathBuf> {
    let mut path = dqcon_dir()?;
    path.push("memory");
    let _ = ensure_private_dir(&path);
    path.push(format!("{}_{}_{}.json", host, port, user));
    Some(path)
}

fn load_memory(host: &str, port: &str, user: &str) -> MemoryData {
    load_json_file(get_memory_path(host, port, user))
}

fn save_memory(host: &str, port: &str, user: &str, data: &MemoryData) -> io::Result<()> {
    save_json_file(get_memory_path(host, port, user), data)
}

fn get_hosts_path() -> Option<PathBuf> {
    let mut path = dqcon_dir()?;
    path.push("hosts.json");
    Some(path)
}

fn load_hosts() -> HostsData {
    load_json_file(get_hosts_path())
}

fn save_hosts(data: &HostsData) -> io::Result<()> {
    save_json_file(get_hosts_path(), data)
}

fn update_host_usage(addr: &str) {
    let normalized = normalize_addr(addr.to_string());
    let (nh, np, nu, _) = parse_conn_info(&normalized);
    let mut data = load_hosts();
    if let Some(entry) = data.hosts.iter_mut().find(|h| {
        let (eh, ep, eu, _) = parse_conn_info(&h.addr);
        nh == eh && np == ep && nu == eu
    }) {
        entry.addr = normalized;
        entry.count += 1;
    } else {
        data.hosts.push(HostEntry {
            addr: normalized,
            count: 1,
        });
    }
    data.hosts.sort_by(|a, b| b.count.cmp(&a.count));
    let _ = save_hosts(&data);
}


fn delete_host(addr: &str) {
    let mut data = load_hosts();
    data.hosts.retain(|h| h.addr != addr);
    let _ = save_hosts(&data);
}

fn mask_addr(addr: &str) -> String {
    let parts: Vec<&str> = addr.split(':').collect();
    if parts.len() > 3 {
        let mut masked = parts[0..3].join(":");
        masked.push(':');
        let pwd_len = parts[3..].join(":").len();
        masked.push_str(&"*".repeat(pwd_len));
        masked
    } else {
        addr.to_string()
    }
}

/// Internal application events used for communication between the background threads and the main UI loop.
#[derive(Debug)]
enum AppEvent {
    Term(Event),
    QueryStart {
        id: u32,
        token: u32,
        exec_id: u32,
    },
    Result {
        id: u32,
        token: u32,
        exec_id: u32,
        text: String,
        is_err: bool,
        should_flash: bool,
    },
    FlashOff(u32),
    WaitTimeout(u32, u32, u32),
    ClipboardPaste(String),
}

struct HostSelectState {
    input: String,
    cursor: usize,
    selected: i32, // -1 for input field, 0..N for hosts list
    hosts: Vec<HostEntry>,
    error: Option<String>,
}

impl HostSelectState {
    fn empty() -> Self {
        Self {
            input: String::new(),
            cursor: 0,
            selected: -1,
            hosts: Vec::new(),
            error: None,
        }
    }

    fn with_recent_hosts() -> Self {
        Self {
            hosts: load_hosts().hosts,
            ..Self::empty()
        }
    }

    fn selected_addr(&self) -> Option<String> {
        if self.selected == -1 {
            let input = self.input.trim();
            if input.is_empty() {
                None
            } else {
                Some(input.to_string())
            }
        } else if self.selected >= 0 && self.selected < self.hosts.len() as i32 {
            Some(self.hosts[self.selected as usize].addr.clone())
        } else {
            None
        }
    }

    fn select_prev(&mut self) {
        self.error = None;
        let min = -1;
        let max = self.hosts.len() as i32 - 1;
        if self.selected > min {
            self.selected -= 1;
        } else {
            self.selected = max;
        }
    }

    fn select_next(&mut self) {
        self.error = None;
        let min = -1;
        let max = self.hosts.len() as i32 - 1;
        if self.selected < max {
            self.selected += 1;
        } else {
            self.selected = min;
        }
    }

    fn delete_selected_host(&mut self) {
        if self.selected >= 0 && self.selected < self.hosts.len() as i32 {
            let addr = self.hosts[self.selected as usize].addr.clone();
            delete_host(&addr);
            self.hosts = load_hosts().hosts;
            if self.selected >= self.hosts.len() as i32 {
                self.selected = self.hosts.len() as i32 - 1;
            }
        }
    }

    fn insert_char(&mut self, c: char) {
        self.error = None;
        self.selected = -1;
        self.input.insert(self.cursor, c);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        self.error = None;
        self.selected = -1;
        if self.cursor > 0 {
            self.cursor -= 1;
            self.input.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        self.error = None;
        self.selected = -1;
        if self.cursor < self.input.len() {
            self.input.remove(self.cursor);
        }
    }

    fn move_left(&mut self) {
        self.error = None;
        self.selected = -1;
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn move_right(&mut self) {
        self.error = None;
        self.selected = -1;
        if self.cursor < self.input.len() {
            self.cursor += 1;
        }
    }
}

const REPEAT_SECONDS: &[u64] = &[1, 2, 5, 10];
const HOST_POPUP_WIDTH: u16 = 60;
const HOST_POPUP_MAX_HEIGHT: u16 = 18;
const HOST_FIXED_INNER_ROWS: u16 = 5;

// ── network ───────────────────────────────────────────────────────────────────

/// Executes a synchronous network query to the specified address.
/// Sends a header containing the user (and optionally password) followed by the command.
fn query(addr: &str, user: &str, pwd: &str, cmd: &[u8]) -> io::Result<Vec<u8>> {
    use std::net::ToSocketAddrs;
    let addrs = addr.to_socket_addrs()?.filter(|sa| sa.is_ipv4());
    let mut last_err = io::Error::new(io::ErrorKind::AddrNotAvailable, "could not resolve address");

    for sa in addrs {
        let res = TcpStream::connect_timeout(&sa, Duration::from_secs(10));
        match res {
            Ok(mut conn) => {
                let header = if pwd.is_empty() {
                    format!("{}\x00", user)
                } else {
                    format!("{}:{}\x00", user, pwd)
                };
                let mut pkt = Vec::with_capacity(header.len() + cmd.len() + 1);
                pkt.extend_from_slice(header.as_bytes());
                pkt.extend_from_slice(cmd);
                pkt.push(0);
                conn.write_all(&pkt)?;
                conn.shutdown(std::net::Shutdown::Write)?;
                let mut buf = Vec::new();
                conn.read_to_end(&mut buf)?;
                return Ok(buf);
            }
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

// ── repeater ──────────────────────────────────────────────────────────────────

/// Spawns a background thread to execute a query once or repeatedly.
/// Communicates progress and results back to the main loop via the provided mpsc sender.
fn start_query(
    addr: String,
    user: String,
    pwd: String,
    id: u32,
    token: u32,
    cmd: String,
    delay: Option<Duration>,
    tx: mpsc::Sender<AppEvent>,
    cancel: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        let send_result = |text: String, is_err: bool, should_flash: bool, exec_id: u32| -> bool {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            tx.send(AppEvent::Result {
                id,
                token,
                exec_id,
                text,
                is_err,
                should_flash,
            })
            .is_ok()
        };

        let mut exec_id = 0u32;
        loop {
            exec_id = exec_id.wrapping_add(1);
            let current_exec = exec_id;
            let _ = tx.send(AppEvent::QueryStart {
                id,
                token,
                exec_id: current_exec,
            });

            let tx_timeout = tx.clone();
            let cancel_timeout = cancel.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                if !cancel_timeout.load(Ordering::Relaxed) {
                    let _ = tx_timeout.send(AppEvent::WaitTimeout(id, token, current_exec));
                }
            });

            let (text, is_err) = do_query(&addr, &user, &pwd, &cmd);
            if !send_result(text, is_err, delay.is_some(), current_exec) {
                break;
            }

            if let Some(d) = delay {
                thread::sleep(d);
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
            } else {
                break;
            }
        }
    });
}

/// Wrapper for `query` that handles string conversions, response cleaning, and error formatting.
/// It strips protocol headers, carriage returns, and null bytes from the output.
fn do_query(addr: &str, user: &str, pwd: &str, cmd: &str) -> (String, bool) {
    match query(addr, user, pwd, cmd.as_bytes()) {
        Ok(bytes) => {
            let header = if pwd.is_empty() {
                format!("{}\x00", user)
            } else {
                format!("{}:{}\x00", user, pwd)
            };
            let mut start = 0;
            if bytes.starts_with(header.as_bytes()) {
                start = header.len();
            }
            let text = String::from_utf8_lossy(&bytes[start..])
                .replace('\r', "")
                .replace('\0', "");
            let trimmed = text.trim_end_matches('\n').to_string();
            (trimmed, false)
        }
        Err(e) => (e.to_string(), true),
    }
}

/// Reads text from the system clipboard using platform-specific tools.
/// Works in WSL (powershell.exe), macOS (pbpaste), and Linux (xclip/xsel/wl-paste).
fn read_clipboard() -> Option<String> {
    use std::process::Command;
    let candidates: &[(&str, &[&str])] = &[
        (
            "powershell.exe",
            &["-NoProfile", "-NonInteractive", "-Command", "Get-Clipboard"],
        ),
        ("pbpaste", &[]),
        ("xclip", &["-selection", "clipboard", "-o"]),
        ("xsel", &["--clipboard", "--output"]),
        ("wl-paste", &["--no-newline"]),
    ];
    for &(cmd, args) in candidates {
        if let Ok(output) = Command::new(cmd).args(args).output() {
            if output.status.success() {
                let text = String::from_utf8_lossy(&output.stdout).to_string();
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
    None
}

/// Writes text to the system clipboard using platform-specific tools.
/// Works in WSL (clip.exe), macOS (pbcopy), and Linux (xclip/xsel/wl-copy).
fn write_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let candidates: &[(&str, &[&str])] = &[
        ("clip.exe", &[]),
        ("pbcopy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("wl-copy", &[]),
    ];

    for &(cmd, args) in candidates {
        if let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok() {
                return;
            }
        }
    }
}

/// Stops any active background repetition for the given entry and resets its UI state.
fn stop_repeat(entry: &mut Entry) {
    if let Some(cancel) = entry.cancel.take() {
        cancel.store(true, Ordering::Relaxed);
    }
    entry.repeat_delay = None;
    entry.flash = false;
    entry.is_pending = false;
    entry.is_waiting = false;
}

fn stop_all_repeats(entries: &mut [Entry]) {
    for entry in entries.iter_mut() {
        stop_repeat(entry);
    }
}

fn clear_entry(entry: &mut Entry) {
    stop_repeat(entry);
    entry.output.clear();
    entry.is_err = false;
    entry.cmd.clear();
    entry.cursor_pos = 0;
}

fn delete_current_entry(entries: &mut Vec<Entry>, cur: &mut usize) {
    stop_repeat(&mut entries[*cur]);
    if entries.len() == 1 {
        entries[0] = Entry::default();
    } else {
        entries.remove(*cur);
        if *cur >= entries.len() {
            *cur = entries.len().saturating_sub(1);
        }
    }
}

fn move_to_next_entry(entries: &mut [Entry], cur: &mut usize) {
    if *cur + 1 < entries.len() {
        *cur += 1;
        entries[*cur].cursor_pos = entries[*cur].cmd.len();
    }
}

// ── selection helpers ────────────────────────────────────────────────────────

/// Returns the (start, end) byte range of the current selection, if any.
fn selected_range(e: &Entry) -> Option<(usize, usize)> {
    e.sel_anchor.map(|anchor| {
        let a = anchor.min(e.cursor_pos);
        let b = anchor.max(e.cursor_pos);
        (a, b)
    }).filter(|(a, b)| a != b)
}

fn output_selected_range(e: &Entry) -> Option<(usize, usize)> {
    e.output_sel.map(|(a, b)| {
        let start = a.min(b);
        let end = a.max(b);
        (start, end)
    }).filter(|(a, b)| a != b)
}

/// Returns the selected output text slice, if any.
fn output_selected_text<'a>(e: &'a Entry) -> Option<&'a str> {
    output_selected_range(e).map(|(a, b)| &e.output[a..b])
}

/// Returns the selected text slice, if any.
fn selected_text<'a>(e: &'a Entry) -> Option<&'a str> {
    selected_range(e).map(|(a, b)| &e.cmd[a..b])
}

/// Deletes the selected text, positions cursor at the start of the range,
/// and clears the anchor. Returns true if a selection was deleted.
fn delete_selection(e: &mut Entry) -> bool {
    if let Some((a, b)) = selected_range(e) {
        e.cmd.drain(a..b);
        e.cursor_pos = a;
        e.sel_anchor = None;
        true
    } else {
        e.sel_anchor = None;
        false
    }
}

/// Clears selection without modifying text.
fn clear_selection(e: &mut Entry) {
    e.sel_anchor = None;
}

/// Ensures a selection anchor exists. If none, sets it to the current cursor position.
fn start_or_extend_selection(e: &mut Entry) {
    if e.sel_anchor.is_none() {
        e.sel_anchor = Some(e.cursor_pos);
    }
}


fn move_cursor_left(entry: &mut Entry, by_word: bool) {
    if by_word {
        let mut pos = entry.cursor_pos;
        while pos > 0 && entry.cmd.as_bytes()[pos - 1] == b' ' {
            pos -= 1;
        }
        while pos > 0 && entry.cmd.as_bytes()[pos - 1] != b' ' {
            pos -= 1;
        }
        entry.cursor_pos = pos;
    } else if entry.cursor_pos > 0 {
        entry.cursor_pos -= prev_char_len(&entry.cmd, entry.cursor_pos);
    }
}

fn move_cursor_right(entry: &mut Entry, by_word: bool) {
    if by_word {
        let mut pos = entry.cursor_pos;
        let bytes = entry.cmd.as_bytes();
        while pos < bytes.len() && bytes[pos] != b' ' {
            pos += 1;
        }
        while pos < bytes.len() && bytes[pos] == b' ' {
            pos += 1;
        }
        entry.cursor_pos = pos;
    } else if entry.cursor_pos < entry.cmd.len() {
        entry.cursor_pos += next_char_len(&entry.cmd, entry.cursor_pos);
    }
}

fn backspace_entry(entry: &mut Entry) {
    if entry.cursor_pos > 0 {
        let sz = prev_char_len(&entry.cmd, entry.cursor_pos);
        let pos = entry.cursor_pos - sz;
        entry.cmd.remove(pos);
        entry.cursor_pos = pos;
    }
}

fn delete_entry_char(entry: &mut Entry) {
    if entry.cursor_pos < entry.cmd.len() {
        entry.cmd.remove(entry.cursor_pos);
    }
}

fn start_entry_query(
    entry: &mut Entry,
    addr: &str,
    user: &str,
    pwd: &str,
    delay: Option<Duration>,
    tx: &mpsc::Sender<AppEvent>,
) {
    entry.repeat_delay = delay;
    entry.repeat_token = entry.repeat_token.wrapping_add(1);
    let token = entry.repeat_token;
    let cancel = Arc::new(AtomicBool::new(false));
    entry.cancel = Some(cancel.clone());
    start_query(
        addr.to_string(),
        user.to_string(),
        pwd.to_string(),
        entry.id,
        token,
        entry.cmd.clone(),
        delay,
        tx.clone(),
        cancel,
    );
}

fn request_clipboard_paste(tx: &mpsc::Sender<AppEvent>) {
    let tx_clip = tx.clone();
    thread::spawn(move || {
        if let Some(text) = read_clipboard() {
            let _ = tx_clip.send(AppEvent::ClipboardPaste(text));
        }
    });
}

fn request_clipboard_copy(text: String) {
    thread::spawn(move || {
        write_clipboard(&text);
    });
}

/// Inserts a newline character at the current cursor position within an entry.
fn insert_newline(e: &mut Entry) {
    let pos = e.cursor_pos;
    e.cmd.insert(pos, '\n');
    e.cursor_pos += 1;
}

/// Inserts text at the current cursor position and normalizes pasted newlines.
fn insert_text(e: &mut Entry, text: &str) {
    if text.is_empty() {
        return;
    }

    let normalized = if text.contains('\r') {
        Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        Cow::Borrowed(text)
    };

    // Strip trailing newlines – terminal copy typically appends one.
    let cleaned = normalized.trim_end_matches('\n');
    if cleaned.is_empty() {
        return;
    }

    let pos = e.cursor_pos;
    e.cmd.insert_str(pos, cleaned);
    e.cursor_pos += cleaned.len();
}

// ── layout helpers ────────────────────────────────────────────────────────────

struct WrappedLine {
    text: String,
    start: usize,
    end: usize,
}

/// Splits a string into lines that fit within specified character budgets for the first and subsequent lines.
/// Tracks byte offsets for each visual line.
fn wrap_lines(text: &str, first_budget: usize, cont_budget: usize) -> Vec<WrappedLine> {
    if text.is_empty() {
        return vec![WrappedLine {
            text: String::new(),
            start: 0,
            end: 0,
        }];
    }
    let mut lines = Vec::new();
    let mut pos = 0;
    let mut budget = first_budget;

    while pos < text.len() {
        let mut end = pos;
        let mut hit_newline = false;
        for (idx, ch) in text[pos..].char_indices() {
            if ch == '\n' {
                hit_newline = true;
                end = pos + idx;
                break;
            }
            if idx > budget {
                break;
            }
            end = pos + idx;
        }
        if !hit_newline && text[pos..].len() <= budget {
            end = text.len();
        }
        if end <= pos && !hit_newline {
            // safety for zero budget or huge char
            end = pos + next_char_len(text, pos);
        }
        lines.push(WrappedLine {
            text: text[pos..end].to_string(),
            start: pos,
            end,
        });
        pos = end;
        if hit_newline {
            pos += 1; // skip \n
        }
        budget = cont_budget;
    }
    if text.ends_with('\n') {
        lines.push(WrappedLine {
            text: String::new(),
            start: text.len(),
            end: text.len(),
        });
    }
    lines
}

/// Compatibility wrapper for existing code.
fn split_lines(text: &str, first_budget: usize, cont_budget: usize) -> Vec<String> {
    wrap_lines(text, first_budget, cont_budget)
        .into_iter()
        .map(|wl| wl.text)
        .collect()
}#[derive(Debug, Clone, Copy)]
enum HitTarget {
    Command { entry_idx: usize, offset: usize },
    Output { entry_idx: usize, offset: usize },
}

fn hit_test(
    col: u16,
    row: u16,
    entries: &[Entry],
    scroll_offset: usize,
    width: usize,
    prompt_len: usize,
) -> Option<HitTarget> {
    let mut current_row = 0;
    let target_row = row as usize + scroll_offset;
    let target_col = col as usize;

    for (i, entry) in entries.iter().enumerate() {
        // Command
        let cmd_lines = wrap_lines(&entry.cmd, width.saturating_sub(prompt_len), width);
        for (li, wl) in cmd_lines.iter().enumerate() {
            if current_row == target_row {
                let start_col = if li == 0 { prompt_len } else { 0 };
                if target_col >= start_col {
                    let mut off = 0;
                    let mut c = start_col;
                    for ch in wl.text.chars() {
                        if c >= target_col {
                            break;
                        }
                        off += ch.len_utf8();
                        c += 1;
                    }
                    return Some(HitTarget::Command {
                        entry_idx: i,
                        offset: wl.start + off,
                    });
                }
                return None;
            }
            current_row += 1;
        }

        // Output
        if !entry.output.is_empty() {
            let output_lines = wrap_lines(&entry.output, width, width);
            for wl in output_lines.iter() {
                if current_row == target_row {
                    let mut off = 0;
                    let mut c = 0;
                    for ch in wl.text.chars() {
                        if c >= target_col {
                            break;
                        }
                        off += ch.len_utf8();
                        c += 1;
                    }
                    return Some(HitTarget::Output {
                        entry_idx: i,
                        offset: wl.start + off,
                    });
                }
                current_row += 1;
            }
        }
    }
    None
}

// ── rendering ─────────────────────────────────────────────────────────────────

struct RenderState<'a> {
    lines: Vec<Line<'a>>,
    cursor: Option<(u16, u16)>, // (col, row) 0-indexed
}

enum EntryView<'a> {
    Borrowed(&'a [Entry]),
    Owned(Vec<Entry>),
}

impl EntryView<'_> {
    fn as_slice(&self) -> &[Entry] {
        match self {
            EntryView::Borrowed(entries) => entries,
            EntryView::Owned(entries) => entries.as_slice(),
        }
    }
}

/// Constructs the complete list of text lines to be rendered in the TUI based on the current state.
fn build_render<'a>(
    entries: &[Entry],
    cur: usize,
    prompt: &'a str,
    width: usize,
) -> RenderState<'a> {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let mut cursor: Option<(u16, u16)> = None;
    let prompt_len = prompt.len();

    // No header

    for (i, entry) in entries.iter().enumerate() {
        let row_start = lines.len();
        let prompt_color = if entry.is_waiting {
            Color::DarkGray
        } else if i == cur {
            Color::Cyan
        } else {
            Color::Blue
        };
        let cmd_color = if entry.is_waiting {
            Color::DarkGray
        } else {
            Color::Reset
        };
        let p_span = Span::styled(prompt, Style::default().fg(prompt_color));

        let cmd_lines = wrap_lines(&entry.cmd, width.saturating_sub(prompt_len), width);
        let sel = if i == cur { selected_range(entry) } else { None };
        let sel_style = Style::default().fg(Color::Black).bg(Color::White);

        for (li, wl) in cmd_lines.iter().enumerate() {
            let line_start = wl.start;
            let line_end = wl.end;
            let cl = &wl.text;

            let cmd_spans = if let Some((sa, sb)) = sel {
                let sel_begin = sa.max(line_start);
                let sel_end = sb.min(line_end);
                if sel_begin < sel_end {
                    let before = &cl[..sel_begin - line_start];
                    let selected = &cl[sel_begin - line_start..sel_end - line_start];
                    let after = &cl[sel_end - line_start..];
                    let mut spans = Vec::new();
                    if !before.is_empty() {
                        spans.push(Span::styled(before.to_string(), Style::default().fg(cmd_color)));
                    }
                    spans.push(Span::styled(selected.to_string(), sel_style));
                    if !after.is_empty() {
                        spans.push(Span::styled(after.to_string(), Style::default().fg(cmd_color)));
                    }
                    spans
                } else {
                    vec![Span::styled(cl.clone(), Style::default().fg(cmd_color))]
                }
            } else {
                vec![Span::styled(cl.clone(), Style::default().fg(cmd_color))]
            };

            if li == 0 {
                let mut spans = vec![p_span.clone()];
                spans.extend(cmd_spans);
                lines.push(Line::from(spans));
            } else {
                lines.push(Line::from(cmd_spans));
            }
        }

        if i == cur {
            let cursor_lines = split_lines(
                &entry.cmd[..entry.cursor_pos],
                width.saturating_sub(prompt_len),
                width,
            );
            let row_off = cursor_lines.len().saturating_sub(1);
            let col = if row_off == 0 {
                prompt_len + cursor_lines[0].len()
            } else {
                cursor_lines.last().map(|s| s.len()).unwrap_or(0)
            };
            cursor = Some((col as u16, (row_start + row_off) as u16));
        }

        if !entry.output.is_empty() {
            let color = if entry.is_waiting {
                Color::DarkGray
            } else if entry.is_err {
                Color::Red
            } else if entry.flash {
                Color::Yellow
            } else {
                Color::White
            };

            let output_sel = entry.output_sel.map(|(sa, sb)| (sa.min(sb), sa.max(sb)));

            for wl in wrap_lines(&entry.output, width, width) {
                let line_start = wl.start;
                let line_end = wl.end;
                let cl = &wl.text;

                let output_spans = if let Some((sa, sb)) = output_sel {
                    let sel_begin = sa.max(line_start);
                    let sel_end = sb.min(line_end);
                    if sel_begin < sel_end {
                        let before = &cl[..sel_begin - line_start];
                        let selected = &cl[sel_begin - line_start..sel_end - line_start];
                        let after = &cl[sel_end - line_start..];
                        let mut spans = Vec::new();
                        if !before.is_empty() {
                            spans.push(Span::styled(before.to_string(), Style::default().fg(color)));
                        }
                        spans.push(Span::styled(selected.to_string(), sel_style));
                        if !after.is_empty() {
                            spans.push(Span::styled(after.to_string(), Style::default().fg(color)));
                        }
                        spans
                    } else {
                        vec![Span::styled(cl.clone(), Style::default().fg(color))]
                    }
                } else {
                    vec![Span::styled(cl.clone(), Style::default().fg(color))]
                };
                lines.push(Line::from(output_spans));
            }
        }
    }
    RenderState { lines, cursor }
}

fn build_host_render(state: &HostSelectState, visible_host_rows: usize) -> RenderState<'_> {
    let mut lines = Vec::new();
    let mut cursor = None;
    let content_width = HOST_POPUP_WIDTH as usize - 4; // popup_width - 2 (borders) - 2 (padding)
    let label_width = 9; // "Address: "
    let input_display_width = content_width - label_width;

    lines.push(Line::raw(" ")); // Empty line with padding at top

    let masked = mask_addr(&state.input);

    // Calculate input scroll
    let mut scroll = 0;
    if state.cursor >= input_display_width {
        scroll = state.cursor - input_display_width + 1;
    }

    let visible_input: String = masked
        .chars()
        .skip(scroll)
        .take(input_display_width)
        .collect();

    lines.push(Line::from(vec![
        Span::raw(" "),
        Span::styled("Address: ", Style::default().fg(Color::Cyan)),
        Span::raw(visible_input),
    ]));

    if state.selected == -1 {
        // label_width (9) + state.cursor - scroll + 1 (left padding)
        cursor = Some(((label_width + state.cursor - scroll + 1) as u16, 1u16));
    }

    if let Some(ref err) = state.error {
        let mut err_msg = format!("Error: {}", err);
        if err_msg.len() > content_width {
            err_msg.truncate(content_width - 3);
            err_msg.push_str("...");
        }
        lines.push(Line::from(vec![
            Span::raw(" "),
            Span::styled(err_msg, Style::default().fg(Color::Red)),
        ]));
    } else {
        lines.push(Line::raw(" "));
    }
    lines.push(Line::raw(" Recently used (Ctrl+D to delete):"));

    let host_scroll = host_list_scroll(state, visible_host_rows);
    for (i, host) in state
        .hosts
        .iter()
        .enumerate()
        .skip(host_scroll)
        .take(visible_host_rows)
    {
        let style = if state.selected == i as i32 {
            Style::default().fg(Color::Black).bg(Color::White)
        } else {
            Style::default()
        };

        let mut addr = mask_addr(&host.addr);
        if addr.len() > content_width {
            addr.truncate(content_width - 3);
            addr.push_str("...");
        }

        // Add 1 space padding left, and fill up to content_width + 1 (for right padding)
        let display = format!(" {:<width$} ", addr, width = content_width);
        lines.push(Line::from(Span::styled(display, style)));
    }

    RenderState { lines, cursor }
}

/// Calculates the required scroll offset to keep the cursor visible on screen.
fn calculate_scroll(rs: &RenderState<'_>, area_height: u16, current_scroll: u16) -> u16 {
    let mut scroll = current_scroll;
    if let Some((_, row)) = rs.cursor {
        if row < scroll {
            scroll = row;
        } else if row >= scroll + area_height {
            scroll = row - area_height + 1;
        }
    }
    scroll
}

/// Creates a centered rectangle for popups based on percentage of width/height or absolute size.
fn popup_rect(width: u16, height: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(r);

    let w = width.min(r.width);
    let margin = (r.width - w) / 2;

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(margin),
            Constraint::Length(w),
            Constraint::Min(0),
        ])
        .split(popup_layout[1])[1]
}

fn item_scroll(selected: usize, visible_height: u16) -> u16 {
    if visible_height > 0 && selected as u16 >= visible_height {
        selected as u16 - visible_height + 1
    } else {
        0
    }
}

fn host_popup_height(area_height: u16, host_count: usize) -> u16 {
    let max_height = HOST_POPUP_MAX_HEIGHT.min(area_height);
    let fixed_height = HOST_FIXED_INNER_ROWS.saturating_add(2);
    if max_height <= fixed_height {
        max_height
    } else {
        let visible_hosts = host_count.min((max_height - fixed_height) as usize) as u16;
        fixed_height + visible_hosts
    }
}

fn host_visible_rows(popup_height: u16) -> usize {
    popup_height
        .saturating_sub(2)
        .saturating_sub(HOST_FIXED_INNER_ROWS) as usize
}

fn host_list_scroll(state: &HostSelectState, visible_rows: usize) -> usize {
    if state.selected < 0 || visible_rows == 0 {
        return 0;
    }

    let max_scroll = state.hosts.len().saturating_sub(visible_rows);
    (item_scroll(state.selected as usize, visible_rows as u16) as usize).min(max_scroll)
}

fn draw_main_and_scroll(
    frame: &mut Frame,
    rs: &RenderState<'_>,
    scroll: u16,
    manual_scroll: bool,
) -> u16 {
    let area = frame.area();
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let main_height = main_layout[0].height;
    let mut draw_scroll = scroll;

    if !manual_scroll {
        draw_scroll = calculate_scroll(rs, main_height, scroll);
    }

    let max_scroll = rs.lines.len() as u16;
    if draw_scroll > max_scroll {
        draw_scroll = max_scroll;
    }

    let para = Paragraph::new(rs.lines.clone()).scroll((draw_scroll, 0));
    frame.render_widget(para, main_layout[0]);

    let status_text = " [Ctrl+H] Connect | [Ctrl+R] Repeat | [Ctrl+N] New Line | [Ctrl+C] Copy | [Ctrl+X] Cut/Exit | [Ctrl+V] Paste | [Ctrl+Q] Help ";
    let status =
        Paragraph::new(status_text).style(Style::default().fg(Color::Black).bg(Color::Cyan));
    frame.render_widget(status, main_layout[1]);

    draw_scroll
}

fn draw(
    frame: &mut Frame,
    entries: &[Entry],
    cur: usize,
    menu_open: bool,
    help_open: bool,
    host_menu_open: bool,
    host_menu_state: &HostSelectState,
    menu_items: &[String],
    menu_sel: usize,
    prompt: &str,
    scroll: u16,
    manual_scroll: bool,
) -> u16 {
    let area = frame.area();
    let width = area.width as usize;
    let rs = build_render(entries, cur, prompt, width);

    let draw_scroll = draw_main_and_scroll(frame, &rs, scroll, manual_scroll);

    if menu_open {
        let desired_height = (menu_items.len() as u16 + 2).min(area.height);
        let area_menu = popup_rect(20, desired_height, area);
        frame.render_widget(Clear, area_menu);
        let block = Block::default()
            .title(Span::styled(
                " Repeat ",
                Style::default().add_modifier(Modifier::REVERSED),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));

        let mut lines = Vec::new();
        let menu_content_width = 20 - 4; // popup_width - 2 (borders) - 2 (padding)
        for (i, item) in menu_items.iter().enumerate() {
            let style = if i == menu_sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let display = format!(" {:<width$} ", item, width = menu_content_width);
            lines.push(Line::from(Span::styled(display, style)));
        }

        let menu_scroll = item_scroll(menu_sel, area_menu.height.saturating_sub(2));

        let menu_para = Paragraph::new(lines).block(block).scroll((menu_scroll, 0));
        frame.render_widget(menu_para, area_menu);
    }

    if help_open {
        let help_items = [
            " Up/Down:        Navigate history     ",
            " Enter:          Run command          ",
            " Shift+Arrows:   Select text          ",
            " Shift+Home/End: Select to boundary   ",
            " Mouse Left:     Select result text   ",
            " Mouse Right:    Paste from clipboard ",
            " Ctrl+C:         Copy selection / all ",
            " Ctrl+X:         Cut selection / Exit ",
            " Ctrl+V:         Paste                ",
            " Ctrl+N:         Insert newline       ",
            " Ctrl+R:         Repeat command       ",
            " Ctrl+D:         Delete command       ",
            " Ctrl+H:         Select connection    ",
            " Ctrl+Q:         Show this help       ",
        ];
        let desired_height = (help_items.len() as u16 + 2).min(area.height);
        let area_help = popup_rect(40, desired_height, area);
        frame.render_widget(Clear, area_help);

        let block = Block::default()
            .title(Span::styled(
                " Help ",
                Style::default().add_modifier(Modifier::REVERSED),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));

        let mut lines = Vec::new();
        for item in help_items.iter() {
            lines.push(Line::from(Span::styled(*item, Style::default())));
        }

        let help_para = Paragraph::new(lines).block(block);
        frame.render_widget(help_para, area_help);
    }

    if host_menu_open {
        draw_host_popup(frame, area, host_menu_state, " Select Connection ");
    } else if let Some((col, row)) = rs.cursor {
        let main_height = area.height.saturating_sub(1);
        if row >= draw_scroll && row < draw_scroll + main_height {
            frame.set_cursor_position((col.min(area.width.saturating_sub(1)), row - draw_scroll));
            let _ = execute!(io::stderr(), Show);
        } else {
            let _ = execute!(io::stderr(), Hide);
        }
    } else {
        let _ = execute!(io::stderr(), Hide);
    }
    draw_scroll
}

fn draw_host_popup(
    frame: &mut Frame,
    area: Rect,
    state: &HostSelectState,
    title: &str,
) {
    let desired_height = host_popup_height(area.height, state.hosts.len());
    let area_host = popup_rect(HOST_POPUP_WIDTH, desired_height, area);
    let rs_host = build_host_render(state, host_visible_rows(area_host.height));
    frame.render_widget(Clear, area_host);

    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default().add_modifier(Modifier::REVERSED),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(rs_host.lines).block(block);
    frame.render_widget(para, area_host);

    if let Some((col, row)) = rs_host.cursor {
        frame.set_cursor_position((area_host.x + 1 + col, area_host.y + 1 + row));
        let _ = execute!(io::stderr(), Show);
    } else {
        let _ = execute!(io::stderr(), Hide);
    }
}

fn draw_host_select(
    frame: &mut Frame,
    state: &HostSelectState,
    entries: &[Entry],
    cur: usize,
    prompt: &str,
) {
    let area = frame.area();

    // Draw the background preview
    let rs = build_render(entries, cur, prompt, area.width as usize);

    // For preview, we always want to follow the cursor from zero
    let _ = draw_main_and_scroll(frame, &rs, 0, false);

    // Draw the host popup
    draw_host_popup(frame, area, state, " Select Connection ");
}

fn run_host_selection(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
) -> io::Result<Option<String>> {
    let mut state = HostSelectState::with_recent_hosts();

    loop {
        let (p_entries, p_cur, p_prompt) = if state.selected != -1 {
            let addr_str = state.hosts[state.selected as usize].addr.clone();
            let (h, p, u, _) = parse_conn_info(&addr_str);
            let mem = if MEMORY_MODE.load(Ordering::Relaxed) {
                load_memory(&h, &p, &u)
            } else {
                MemoryData::default()
            };
            let (ents, c) = memory_to_entries(mem);
            let prompt = cmd_prompt(&addr_str);
            (ents, c, prompt)
        } else {
            (vec![Entry::default()], 0, String::new())
        };

        terminal.draw(|f| draw_host_select(f, &state, &p_entries, p_cur, &p_prompt))?;

        if let Event::Key(KeyEvent {
            code,
            modifiers,
            kind,
            ..
        }) = crossterm::event::read()?
        {
            if kind == KeyEventKind::Release {
                continue;
            }
            match code {
                KeyCode::Char('x') if modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
                KeyCode::Esc => return Ok(None),
                KeyCode::Up => state.select_prev(),
                KeyCode::Down => state.select_next(),
                KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                    state.delete_selected_host()
                }
                KeyCode::Enter => {
                    if let Some(addr) = state.selected_addr() {
                        if let Err(e) = validate_addr(&addr) {
                            state.error = Some(e);
                        } else {
                            return Ok(Some(addr));
                        }
                    }
                }
                KeyCode::Char(c) => {
                    state.insert_char(c);
                }
                KeyCode::Backspace => state.backspace(),
                KeyCode::Delete => state.delete(),
                KeyCode::Left => state.move_left(),
                KeyCode::Right => state.move_right(),
                _ => {}
            }
        }
    }
}

fn switch_host(
    selected_addr: String,
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    addr: &mut String,
    host: &mut String,
    port: &mut String,
    user: &mut String,
    pwd: &mut String,
    network_addr: &mut String,
    prompt: &mut String,
    entries: &mut Vec<Entry>,
    cur: &mut usize,
    scroll_offset: &mut u16,
) -> io::Result<()> {
    let normalized = normalize_addr(selected_addr);
    let (h, p, u, pw) = parse_conn_info(&normalized);

    if h == *host && p == *port && u == *user {
        *addr = normalized;
        *pwd = pw;
        update_host_usage(addr);
        return Ok(());
    }

    save_entries_memory_if_enabled(host, port, user, entries, *cur);

    *addr = normalized;
    *host = h;
    *port = p;
    *user = u;
    *pwd = pw;
    update_host_usage(addr);
    *network_addr = if port.is_empty() {
        host.clone()
    } else {
        format!("{}:{}", host, port)
    };
    *prompt = cmd_prompt(addr);

    stop_all_repeats(entries);

    if MEMORY_MODE.load(Ordering::Relaxed) {
        let mem = load_memory(host, port, user);
        let (loaded, c) = memory_to_entries(mem);
        *entries = loaded;
        *cur = c;
    } else {
        *entries = vec![Entry::default()];
        *cur = 0;
    }
    *scroll_offset = 0;
    terminal.clear()?;
    Ok(())
}

// ── menu helpers ──────────────────────────────────────────────────────────────

/// Returns the list of options available in the "Repeat" menu for a specific entry.
fn menu_options_for(entries: &[Entry], cur: usize) -> Vec<String> {
    let mut items = Vec::new();
    if entries[cur].cmd.trim().is_empty() {
        if entries.iter().any(|e| e.repeat_delay.is_some()) {
            items.push("stop all".to_string());
        }
    } else {
        items = REPEAT_SECONDS
            .iter()
            .map(|s| format!("every {} sec", s))
            .collect();
        if entries[cur].repeat_delay.is_some() {
            items.push("stop".to_string());
        }
        if entries.iter().any(|e| e.repeat_delay.is_some()) {
            items.push("stop all".to_string());
        }
    }
    items
}

fn apply_menu_choice(
    entries: &mut Vec<Entry>,
    cur: usize,
    choice: &str,
    addr: &str,
    user: &str,
    pwd: &str,
    tx: &mpsc::Sender<AppEvent>,
) {
    if cur >= entries.len() {
        return;
    }
    stop_repeat(&mut entries[cur]);
    if choice == "stop" {
        return;
    }
    if choice == "stop all" {
        stop_all_repeats(entries);
        return;
    }
    for &sec in REPEAT_SECONDS {
        let lbl = format!("every {} sec", sec);
        if lbl == choice {
            let delay = Duration::from_secs(sec);
            start_entry_query(&mut entries[cur], addr, user, pwd, Some(delay), tx);
            return;
        }
    }
}

fn apply_menu_choice_and_advance(
    entries: &mut Vec<Entry>,
    cur: &mut usize,
    choice: &str,
    addr: &str,
    user: &str,
    pwd: &str,
    tx: &mpsc::Sender<AppEvent>,
) {
    apply_menu_choice(entries, *cur, choice, addr, user, pwd, tx);
    ensure_tail(entries);
    move_to_next_entry(entries, cur);
}

// ── main ──────────────────────────────────────────────────────────────────────

/// Normalizes and parses the connection address into (host, port, user, pwd).
/// Treats empty hostnames or "0" as "localhost" and default user as "qcon".
fn parse_conn_info(addr: &str) -> (String, String, String, String) {
    let parts: Vec<&str> = addr.split(':').collect();
    let mut host = parts.get(0).copied().unwrap_or("localhost").to_string();
    if host.is_empty() || host == "0" {
        host = "localhost".to_string();
    }
    let port = parts.get(1).copied().unwrap_or("").to_string();
    let mut user = parts.get(2).copied().unwrap_or("qcon").to_string();
    if user.is_empty() {
        user = "qcon".to_string();
    }
    let pwd = parts.get(3).copied().unwrap_or("").to_string();

    (host, port, user, pwd)
}

/// Normalizes the connection address by treating empty hostnames or "0" as "localhost".
fn normalize_addr(addr: String) -> String {
    if let Some(colon) = addr.find(':') {
        let host = &addr[..colon];
        let rest = &addr[colon..];
        if host.is_empty() || host == "0" {
            return format!("localhost{}", rest);
        }
    } else if addr == "0" || addr.is_empty() {
        return "localhost".to_string();
    }
    addr
}

/// Centralized validation logic for connection addresses.
/// Enforces 'host:port' format.
fn validate_addr(addr: &str) -> Result<(), String> {
    if !addr.contains(':') {
        return Err("Missing port (format: host:port)".to_string());
    }
    let parts: Vec<&str> = addr.split(':').collect();
    if parts.len() < 2 || parts[1].is_empty() {
        return Err("Missing port number".to_string());
    }
    Ok(())
}

/// Ensures there is always an empty command entry at the end of the history.
fn ensure_tail(entries: &mut Vec<Entry>) {
    if entries.last().map(|e| !e.cmd.is_empty()).unwrap_or(true) {
        entries.push(Entry::default());
    }
}

/// Generates a formatted command prompt string based on the connection address.
fn cmd_prompt(addr: &str) -> String {
    // Show up to the first two parts (host:port) in the prompt if possible
    let parts: Vec<&str> = addr.split(':').collect();
    if parts.len() >= 2 {
        let host = parts[0];
        let port = parts[1];
        if port.chars().all(|c| c.is_ascii_digit()) {
            if host == "localhost" {
                return format!(":{}>", port);
            }
            return format!("{}:{}>", host, port);
        }
    }
    format!("{}>", addr)
}

/// Runs a simple synchronous REPL mode that reads from stdin and prints to stdout.
fn run_simple_repl(addr: &str, user: &str, pwd: &str) -> io::Result<()> {
    let prompt = cmd_prompt(addr);
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    loop {
        print!("{}", prompt);
        stdout.flush()?;

        let mut line = String::new();
        let bytes_read = stdin.lock().read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }

        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        if cmd == r"\\" {
            break;
        }

        let (text, is_err) = do_query(addr, user, pwd, cmd);
        if is_err {
            eprintln!("{}", text);
        } else {
            println!("{}", text);
        }
    }
    Ok(())
}

/// Main entry point. Parses CLI arguments and launches either the TUI or the simple REPL.
fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|arg| arg == "-h" || arg == "-help") {
        println!("Usage: dqcon [-h|-v|-m|-s] [[host]:port[:user[:pass]]]");
        println!();
        println!("Options:");
        println!("  -h, -help         Show this help message");
        println!("  -v, -version      Show version info");
        println!("  -m, -memory       Enable persistent session history");
        println!("  -s, -simple       Launche in standard REPL mode instead of the TUI");
        return Ok(());
    }

    if args.iter().any(|arg| arg == "-version" || arg == "-v") {
        println!(
            "DQCON {} {}/{} {}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            env!("BUILD_DATE")
        );
        return Ok(());
    }

    let mut addr = String::new();
    for arg in args.iter().skip(1) {
        if arg == "-s" || arg == "-simple" {
            SIMPLE_MODE.store(true, Ordering::Relaxed);
        } else if arg == "-m" || arg == "-memory" {
            MEMORY_MODE.store(true, Ordering::Relaxed);
        } else if addr.is_empty() {
            addr = arg.clone();
        }
    }

    if SIMPLE_MODE.load(Ordering::Relaxed) && addr.is_empty() {
        eprintln!("Error: Connection address is mandatory in simple mode");
        std::process::exit(1);
    }

    if !addr.is_empty() {
        if let Err(e) = validate_addr(&addr) {
            eprintln!("Error: Connection address {}", e);
            std::process::exit(1);
        }
    }

    if addr.is_empty() {
        enable_raw_mode()?;
        let mut stderr = io::stderr();
        execute!(
            stderr,
            EnterAlternateScreen,
            Hide,
            crossterm::event::EnableMouseCapture,
            EnableBracketedPaste
        )?;
        let mut terminal = Terminal::new(CrosstermBackend::new(stderr))?;

        let selected = run_host_selection(&mut terminal);

        match selected {
            Ok(Some(s)) => addr = normalize_addr(s),
            _ => {
                execute!(
                    io::stderr(),
                    LeaveAlternateScreen,
                    Show,
                    crossterm::event::DisableMouseCapture,
                    DisableBracketedPaste
                )?;
                disable_raw_mode()?;
                std::process::exit(0);
            }
        }
        // We stay in raw mode/alternate screen for TUI if not simple
    } else {
        addr = normalize_addr(addr);
    }

    let (mut host, mut port, mut user, mut pwd) = parse_conn_info(&addr);
    update_host_usage(&addr);
    let mut network_addr = if port.is_empty() {
        host.clone()
    } else {
        format!("{}:{}", host, port)
    };

    if SIMPLE_MODE.load(Ordering::Relaxed) || !io::stdout().is_terminal() {
        if io::stdout().is_terminal() {
            // If we were in alternate screen from host selection, we need to leave it for simple repl
            execute!(
                io::stderr(),
                LeaveAlternateScreen,
                Show,
                crossterm::event::DisableMouseCapture,
                DisableBracketedPaste
            )?;
            disable_raw_mode()?;
        }
        return run_simple_repl(&network_addr, &user, &pwd);
    }
    let mut prompt = cmd_prompt(&addr);

    // Event channel
    let (tx, rx) = mpsc::channel::<AppEvent>();

    // If we didn't go through host selection, we need to setup terminal now
    if !io::stdout().is_terminal()
        || !std::env::args()
            .any(|arg| arg != "-simple" && arg != "-memory" && !arg.starts_with('-'))
    {
        // already handled above
    } else {
        enable_raw_mode()?;
        let mut stderr = io::stderr();
        execute!(
            stderr,
            EnterAlternateScreen,
            Hide,
            crossterm::event::EnableMouseCapture,
            EnableBracketedPaste
        )?;
    }
    let stderr = io::stderr();
    let mut terminal = Terminal::new(CrosstermBackend::new(stderr))?;
    // Note: terminal might have been initialized already if we came from host selection
    // but Terminal::new is fine to call again on the same backend.

    terminal.clear()?;

    // Terminal event reader thread
    let tx_term = tx.clone();
    thread::spawn(move || loop {
        if let Ok(ev) = crossterm::event::read() {
            if tx_term.send(AppEvent::Term(ev)).is_err() {
                break;
            }
        }
    });

    let (mut entries, mut cur) = if MEMORY_MODE.load(Ordering::Relaxed) {
        memory_to_entries(load_memory(&host, &port, &user))
    } else {
        (vec![Entry::default()], 0)
    };

    let mut scroll_offset = 0u16;
    let mut menu_open = false;
    let mut help_open = false;
    let mut host_menu_open = false;
    let mut host_menu_state = HostSelectState::empty();
    let mut menu_sel = 0usize;
    let mut menu_items: Vec<String> = Vec::new();
    let mut manual_scroll = false;

    {
        let width = terminal.size().map(|s| s.width as usize).unwrap_or(80);
        let height = terminal.size().map(|s| s.height).unwrap_or(24);
        let rs = build_render(&entries, cur, &prompt, width);
        scroll_offset = calculate_scroll(&rs, height, scroll_offset);
    }

    terminal.draw(|f| {
        scroll_offset = draw(
            f,
            &entries,
            cur,
            menu_open,
            help_open,
            host_menu_open,
            &host_menu_state,
            &menu_items,
            menu_sel,
            &prompt,
            scroll_offset,
            manual_scroll,
        )
    })?;

    let mut last_char_time = Instant::now();
    'main: loop {
        let ev = match rx.recv() {
            Ok(e) => e,
            Err(_) => break,
        };

        match ev {
            // ── QueryStart ──────────────────────────────────────────────────
            AppEvent::QueryStart { id, token, exec_id } => {
                if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                    if token == e.repeat_token {
                        e.is_pending = true;
                        e.is_waiting = false;
                        e.exec_id = exec_id;
                    }
                }
            }

            // ── Result ──────────────────────────────────────────────────────
            AppEvent::Result {
                id,
                token,
                exec_id,
                text,
                is_err,
                should_flash,
            } => {
                if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                    if token != e.repeat_token {
                        continue;
                    }

                    if exec_id == e.exec_id {
                        e.is_pending = false;
                        e.is_waiting = false;
                        e.output = text;
                        e.is_err = is_err;
                        if should_flash {
                            e.flash = true;
                            let tx2 = tx.clone();
                            thread::spawn(move || {
                                thread::sleep(Duration::from_millis(300));
                                let _ = tx2.send(AppEvent::FlashOff(id));
                            });
                        } else {
                            e.flash = false;
                        }
                    }
                }
            }

            // ── WaitTimeout ──────────────────────────────────────────────────
            AppEvent::WaitTimeout(id, token, exec_id) => {
                if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                    if e.repeat_token == token && e.exec_id == exec_id && e.is_pending {
                        e.is_waiting = true;
                    }
                }
            }

            // ── Flash off ────────────────────────────────────────────────────
            AppEvent::FlashOff(id) => {
                if let Some(e) = entries.iter_mut().find(|e| e.id == id) {
                    e.flash = false;
                }
            }

            // ── Clipboard paste (from right-click) ──────────────────────────
            AppEvent::ClipboardPaste(text) => {
                last_char_time = Instant::now();
                if !menu_open && !help_open {
                    delete_selection(&mut entries[cur]);
                    insert_text(&mut entries[cur], &text);
                }
            }


            // ── Terminal event ───────────────────────────────────────────────
            AppEvent::Term(event) => {
                let (width, prompt_len) = {
                    let size = terminal.size().unwrap_or_default();
                    (size.width as usize, prompt.len())
                };
                match event {
                    Event::Resize(_, _) => {
                        manual_scroll = false;
                    }

                    Event::Mouse(MouseEvent { kind, column, row, .. }) => {
                        if help_open {
                            help_open = false;
                            manual_scroll = false;
                            continue;
                        }

                        if menu_open {
                            match kind {
                                MouseEventKind::ScrollUp => {
                                    menu_sel =
                                        (menu_sel + menu_items.len() - 1) % menu_items.len().max(1);
                                }
                                MouseEventKind::ScrollDown => {
                                    menu_sel = (menu_sel + 1) % menu_items.len().max(1);
                                }
                                _ => {}
                            }
                        } else if host_menu_open {
                            match kind {
                                MouseEventKind::ScrollUp => {
                                    host_menu_state.select_prev();
                                }
                                MouseEventKind::ScrollDown => {
                                    host_menu_state.select_next();
                                }
                                _ => {}
                            }
                        } else {
                            match kind {
                                MouseEventKind::ScrollUp => {
                                    scroll_offset = scroll_offset.saturating_sub(1);
                                    manual_scroll = true;
                                }
                                MouseEventKind::ScrollDown => {
                                    scroll_offset = scroll_offset.saturating_add(1);
                                    manual_scroll = true;
                                }
                                MouseEventKind::Down(MouseButton::Left) => {
                                    if let Some(target) = hit_test(
                                        column,
                                        row,
                                        &entries,
                                        scroll_offset.into(),
                                        width,
                                        prompt_len,
                                    ) {
                                        // Clear all selections first
                                        for e in entries.iter_mut() {
                                            e.output_sel = None;
                                            clear_selection(e);
                                        }

                                        match target {
                                            HitTarget::Output { entry_idx, offset } => {
                                                cur = entry_idx;
                                                entries[cur].output_sel = Some((offset, offset));
                                            }
                                            HitTarget::Command { entry_idx, offset } => {
                                                cur = entry_idx;
                                                let e = &mut entries[cur];
                                                e.sel_anchor = Some(offset);
                                                e.cursor_pos = offset;
                                            }
                                        }
                                    } else {
                                        for e in entries.iter_mut() {
                                            e.output_sel = None;
                                            clear_selection(e);
                                        }
                                    }
                                }
                                MouseEventKind::Drag(MouseButton::Left) => {
                                    if let Some(target) = hit_test(
                                        column,
                                        row,
                                        &entries,
                                        scroll_offset.into(),
                                        width,
                                        prompt_len,
                                    ) {
                                        match target {
                                            HitTarget::Output { entry_idx, offset } => {
                                                if let Some((anchor, _)) = entries[entry_idx].output_sel {
                                                    cur = entry_idx;
                                                    entries[cur].output_sel = Some((anchor, offset));
                                                }
                                            }
                                            HitTarget::Command { entry_idx, offset } => {
                                                cur = entry_idx;
                                                entries[cur].cursor_pos = offset;
                                            }
                                        }
                                    }
                                }
                                MouseEventKind::Down(MouseButton::Right) => {
                                    request_clipboard_paste(&tx);
                                }
                                _ => {}
                            }
                        }
                    }


                    Event::Paste(text) => {
                        last_char_time = Instant::now();
                        if help_open {
                            help_open = false;
                            manual_scroll = false;
                        } else if !menu_open && !host_menu_open {
                            manual_scroll = false;
                            let e = &mut entries[cur];
                            insert_text(e, &text);
                        }
                    }

                    Event::Key(KeyEvent {
                        code,
                        modifiers,
                        kind,
                        ..
                    }) => {
                        if kind == KeyEventKind::Release {
                            continue;
                        }
                        manual_scroll = false;
                        if help_open {
                            help_open = false;
                        } else if host_menu_open {
                            match code {
                                KeyCode::Char('x') if modifiers.contains(KeyModifiers::CONTROL) => {
                                    host_menu_open = false;
                                }
                                KeyCode::Esc => {
                                    host_menu_open = false;
                                }
                                KeyCode::Up => host_menu_state.select_prev(),
                                KeyCode::Down => host_menu_state.select_next(),
                                KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
                                    host_menu_state.delete_selected_host()
                                }
                                KeyCode::Enter => {
                                    if let Some(s) = host_menu_state.selected_addr() {
                                        if let Err(e) = validate_addr(&s) {
                                            host_menu_state.error = Some(e);
                                        } else {
                                            switch_host(
                                                s,
                                                &mut terminal,
                                                &mut addr,
                                                &mut host,
                                                &mut port,
                                                &mut user,
                                                &mut pwd,
                                                &mut network_addr,
                                                &mut prompt,
                                                &mut entries,
                                                &mut cur,
                                                &mut scroll_offset,
                                            )?;
                                            host_menu_open = false;
                                        }
                                    } else {
                                        host_menu_open = false;
                                    }
                                }
                                KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                                    host_menu_state.insert_char(c)
                                }
                                KeyCode::Backspace => host_menu_state.backspace(),
                                KeyCode::Delete => host_menu_state.delete(),
                                KeyCode::Left => host_menu_state.move_left(),
                                KeyCode::Right => host_menu_state.move_right(),
                                _ => {}
                            }
                        } else if menu_open {
                            match code {
                                KeyCode::Up => {
                                    menu_sel =
                                        (menu_sel + menu_items.len() - 1) % menu_items.len().max(1);
                                }
                                KeyCode::Down | KeyCode::Tab => {
                                    menu_sel = (menu_sel + 1) % menu_items.len().max(1);
                                }
                                KeyCode::Enter => {
                                    let choice = menu_items[menu_sel].clone();
                                    menu_open = false;
                                    apply_menu_choice_and_advance(
                                        &mut entries,
                                        &mut cur,
                                        &choice,
                                        &network_addr,
                                        &user,
                                        &pwd,
                                        &tx,
                                    );
                                }
                                KeyCode::Esc => {
                                    menu_open = false;
                                }
                                _ => {}
                            }
                        } else {
                            let e = &mut entries[cur];
                            let has_shift = modifiers.contains(KeyModifiers::SHIFT);
                            let has_ctrl = modifiers.contains(KeyModifiers::CONTROL);
                            match code {
                                // History navigation (clears selection)
                                KeyCode::Up => {
                                    clear_selection(e);
                                    if cur > 0 {
                                        cur -= 1;
                                        entries[cur].cursor_pos = entries[cur].cmd.len();
                                    }
                                }
                                KeyCode::Down => {
                                    clear_selection(e);
                                    if cur + 1 < entries.len() {
                                        cur += 1;
                                        entries[cur].cursor_pos = entries[cur].cmd.len();
                                    }
                                }
                                KeyCode::PageUp => {
                                    clear_selection(e);
                                    cur = cur.saturating_sub(5);
                                    entries[cur].cursor_pos = entries[cur].cmd.len();
                                }
                                KeyCode::PageDown => {
                                    clear_selection(e);
                                    cur = (cur + 5).min(entries.len() - 1);
                                    entries[cur].cursor_pos = entries[cur].cmd.len();
                                }

                                // Cursor movement: Shift extends selection, plain clears it
                                KeyCode::Left => {
                                    if has_shift {
                                        start_or_extend_selection(e);
                                        move_cursor_left(e, has_ctrl);
                                    } else {
                                        // If there's a selection, jump to its start
                                        if let Some((a, _)) = selected_range(e) {
                                            e.cursor_pos = a;
                                            clear_selection(e);
                                        } else {
                                            move_cursor_left(e, has_ctrl);
                                        }
                                    }
                                }
                                KeyCode::Right => {
                                    if has_shift {
                                        start_or_extend_selection(e);
                                        move_cursor_right(e, has_ctrl);
                                    } else {
                                        // If there's a selection, jump to its end
                                        if let Some((_, b)) = selected_range(e) {
                                            e.cursor_pos = b;
                                            clear_selection(e);
                                        } else {
                                            move_cursor_right(e, has_ctrl);
                                        }
                                    }
                                }
                                KeyCode::Home => {
                                    if has_shift {
                                        start_or_extend_selection(e);
                                        e.cursor_pos = 0;
                                    } else {
                                        clear_selection(e);
                                        e.cursor_pos = 0;
                                    }
                                }
                                KeyCode::End => {
                                    if has_shift {
                                        start_or_extend_selection(e);
                                        e.cursor_pos = e.cmd.len();
                                    } else {
                                        clear_selection(e);
                                        e.cursor_pos = e.cmd.len();
                                    }
                                }

                                // Editing (selection-aware)
                                KeyCode::Backspace => {
                                    if !delete_selection(e) {
                                        backspace_entry(e);
                                    }
                                }
                                KeyCode::Delete => {
                                    if !delete_selection(e) {
                                        delete_entry_char(e);
                                    }
                                }
                                KeyCode::Char('n') | KeyCode::Char('N')
                                    if has_ctrl =>
                                {
                                    delete_selection(e);
                                    insert_newline(e);
                                }
                                KeyCode::Char('d') if has_ctrl => {
                                    delete_current_entry(&mut entries, &mut cur);
                                }
                                KeyCode::Char('r') if has_ctrl => {
                                    clear_selection(e);
                                    menu_items = menu_options_for(&entries, cur);
                                    if !menu_items.is_empty() {
                                        menu_sel = 0;
                                        if let Some(d) = entries[cur].repeat_delay {
                                            if let Some(idx) = REPEAT_SECONDS
                                                .iter()
                                                .position(|&s| Duration::from_secs(s) == d)
                                            {
                                                menu_sel = idx;
                                            }
                                        }
                                        menu_open = true;
                                    }
                                }
                                KeyCode::Char('h') if has_ctrl => {
                                    clear_selection(e);
                                    host_menu_state = HostSelectState::with_recent_hosts();
                                    host_menu_open = true;
                                }
                                KeyCode::Char('c') if has_ctrl => {
                                    // Copy selection (output first, then command)
                                    let text = output_selected_text(e)
                                        .map(|s| s.to_string())
                                        .or_else(|| selected_text(e).map(|s| s.to_string()))
                                        .unwrap_or_else(|| e.cmd.clone());
                                    if !text.is_empty() {
                                        request_clipboard_copy(text);
                                    }
                                }
                                KeyCode::Char('x') if has_ctrl => {
                                    // Cut selection, or exit if no selection
                                    if let Some(text) = selected_text(e).map(|s| s.to_string()) {
                                        request_clipboard_copy(text);
                                        delete_selection(e);
                                    } else {
                                        stop_all_repeats(&mut entries);
                                        break 'main;
                                    }
                                }
                                KeyCode::Char('v') if has_ctrl => {
                                    delete_selection(e);
                                    request_clipboard_paste(&tx);
                                }
                                KeyCode::Char('q') if has_ctrl => {
                                    clear_selection(e);
                                    help_open = true;
                                }
                                KeyCode::Char('l') if has_ctrl => {
                                    // Refresh/Clear screen - implicitly handled by the next draw
                                }
                                KeyCode::Char(ch) if !has_ctrl => {
                                    last_char_time = Instant::now();
                                    delete_selection(e);
                                    let s = ch.to_string();
                                    insert_text(e, &s);
                                }


                                // Submit or Newline
                                KeyCode::Enter => {
                                    let now = Instant::now();
                                    let is_paste =
                                        now.duration_since(last_char_time) < Duration::from_millis(30);
                                    if modifiers.contains(KeyModifiers::CONTROL) || is_paste {
                                        delete_selection(e);
                                        insert_newline(e);
                                    } else {
                                        clear_selection(e);
                                        let cmd = e.cmd.trim().to_string();
                                        if cmd == r"\\" {
                                            stop_all_repeats(&mut entries);
                                            break 'main;
                                        }

                                        if cmd.is_empty() {
                                            clear_entry(e);
                                            let is_last = cur == entries.len() - 1;
                                            if is_last {
                                                entries.push(Entry::default());
                                            }
                                        } else {
                                            stop_repeat(e);
                                            e.cmd = cmd;
                                            start_entry_query(
                                                e,
                                                &network_addr,
                                                &user,
                                                &pwd,
                                                None,
                                                &tx,
                                            );
                                            e.cursor_pos = e.cmd.len();
                                            ensure_tail(&mut entries);
                                        }
                                        move_to_next_entry(&mut entries, &mut cur);
                                    }
                                }


                                _ => {}
                            }
                        }
                    }

                    _ => {}
                }
            }
        }

        let (p_entries_view, p_cur, p_prompt) = if host_menu_open && host_menu_state.selected != -1
        {
            let addr_str = host_menu_state.hosts[host_menu_state.selected as usize]
                .addr
                .clone();
            if addr_str == addr {
                // Currently connected host - show live session
                (EntryView::Borrowed(entries.as_slice()), cur, prompt.clone())
            } else {
                let (h, p, u, _) = parse_conn_info(&addr_str);
                let mem = if MEMORY_MODE.load(Ordering::Relaxed) {
                    load_memory(&h, &p, &u)
                } else {
                    MemoryData::default()
                };
                let (ents, c) = memory_to_entries(mem);
                let pr = cmd_prompt(&addr_str);
                (EntryView::Owned(ents), c, pr)
            }
        } else {
            (EntryView::Borrowed(entries.as_slice()), cur, prompt.clone())
        };
        let p_entries = p_entries_view.as_slice();

        // When previewing a different host, start scroll from 0 (same as switch_host does)
        let is_previewing_other = host_menu_open
            && host_menu_state.selected != -1
            && host_menu_state.hosts[host_menu_state.selected as usize].addr != addr;
        let s = if is_previewing_other { 0 } else { scroll_offset };
        let ms = if is_previewing_other { false } else { manual_scroll };
        terminal.draw(|f| {
            let next_scroll = draw(
                f,
                p_entries,
                p_cur,
                menu_open,
                help_open,
                host_menu_open,
                &host_menu_state,
                &menu_items,
                menu_sel,
                &p_prompt,
                s,
                ms,
            );
            // Only update the main scroll offset if we're not previewing another host
            if !is_previewing_other {
                scroll_offset = next_scroll;
            }
        })?;
    }

    // Cleanup handled in the restoration block below

    save_entries_memory_if_enabled(&host, &port, &user, &entries, cur);

    let mut stderr = io::stderr();
    execute!(
        stderr,
        LeaveAlternateScreen,
        Show,
        crossterm::event::DisableMouseCapture,
        DisableBracketedPaste
    )?;
    disable_raw_mode()?;
    let _ = stderr.flush();
    thread::sleep(Duration::from_millis(50));
    Ok(())
}

// ── char helpers ──────────────────────────────────────────────────────────────

fn prev_char_len(s: &str, pos: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = pos;
    loop {
        if i == 0 {
            return pos;
        }
        i -= 1;
        if (bytes[i] & 0xC0) != 0x80 {
            return pos - i;
        }
    }
}

fn next_char_len(s: &str, pos: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = pos + 1;
    while i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
        i += 1;
    }
    i - pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_text_inserts_at_cursor() {
        let mut entry = Entry {
            cmd: "abef".to_string(),
            cursor_pos: 2,
            ..Entry::default()
        };

        insert_text(&mut entry, "cd");

        assert_eq!(entry.cmd, "abcdef");
        assert_eq!(entry.cursor_pos, 4);
    }

    #[test]
    fn insert_text_normalizes_crlf_and_cr() {
        let mut entry = Entry::default();

        insert_text(&mut entry, "one\r\ntwo\rthree");

        assert_eq!(entry.cmd, "one\ntwo\nthree");
        assert_eq!(entry.cursor_pos, "one\ntwo\nthree".len());
    }

    #[test]
    fn insert_text_strips_trailing_newline() {
        let mut entry = Entry::default();

        insert_text(&mut entry, "select * from t\n");

        assert_eq!(entry.cmd, "select * from t");
        assert_eq!(entry.cursor_pos, "select * from t".len());
    }

    #[test]
    fn insert_text_strips_trailing_crlf() {
        let mut entry = Entry::default();

        insert_text(&mut entry, "line1\r\nline2\r\n");

        assert_eq!(entry.cmd, "line1\nline2");
        assert_eq!(entry.cursor_pos, "line1\nline2".len());
    }

    #[test]
    fn insert_text_only_newlines_is_noop() {
        let mut entry = Entry::default();
        entry.cmd = "existing".to_string();
        entry.cursor_pos = 8;

        insert_text(&mut entry, "\n\n\n");

        assert_eq!(entry.cmd, "existing");
        assert_eq!(entry.cursor_pos, 8);
    }

    #[test]
    fn insert_newline_inserts_at_cursor() {
        let mut entry = Entry {
            cmd: "abef".to_string(),
            cursor_pos: 2,
            ..Entry::default()
        };

        insert_newline(&mut entry);

        assert_eq!(entry.cmd, "ab\nef");
        assert_eq!(entry.cursor_pos, 3);
    }

    #[test]
    fn insert_newline_in_empty_command() {
        let mut entry = Entry::default();

        insert_newline(&mut entry);

        assert_eq!(entry.cmd, "\n");
        assert_eq!(entry.cursor_pos, 1);
    }

    #[test]
    fn host_popup_height_is_capped_for_long_recent_list() {
        assert_eq!(host_popup_height(50, 100), HOST_POPUP_MAX_HEIGHT);
    }

    #[test]
    fn host_popup_height_shrinks_for_short_recent_list() {
        assert_eq!(host_popup_height(50, 2), HOST_FIXED_INNER_ROWS + 2 + 2);
    }

    #[test]
    fn host_list_scroll_keeps_selected_host_visible() {
        let mut state = HostSelectState {
            hosts: (0..20)
                .map(|i| HostEntry {
                    addr: format!("localhost:{}:qcon", 5000 + i),
                    count: 1,
                })
                .collect(),
            ..HostSelectState::empty()
        };
        state.selected = 15;

        assert_eq!(host_list_scroll(&state, 5), 11);
    }

    #[test]
    fn test_update_host_usage_with_different_passwords() {
        let temp_id = next_id();
        let temp_dir = std::env::temp_dir().join(format!("dqcon_test_{}", temp_id));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let old_home = std::env::var("HOME").ok();
        let old_userprofile = std::env::var("USERPROFILE").ok();

        std::env::set_var("HOME", &temp_dir);
        std::env::set_var("USERPROFILE", &temp_dir);

        // Initial insert
        update_host_usage("localhost:5000:admin:secret");
        let hosts = load_hosts().hosts;
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].addr, "localhost:5000:admin:secret");
        assert_eq!(hosts[0].count, 1);

        // Update with different password but same host:port:user
        update_host_usage("localhost:5000:admin:newsecret");
        let hosts = load_hosts().hosts;
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].addr, "localhost:5000:admin:newsecret");
        assert_eq!(hosts[0].count, 2);

        // Cleanup env
        if let Some(val) = old_home {
            std::env::set_var("HOME", val);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(val) = old_userprofile {
            std::env::set_var("USERPROFILE", val);
        } else {
            std::env::remove_var("USERPROFILE");
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
