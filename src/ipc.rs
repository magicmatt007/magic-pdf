//! Communication between magic-pdf windows (processes) on one computer.
//!
//! Each instance listens on a random local TCP port (127.0.0.1 only) and registers
//! the port plus a random secret token in a file in the user's data directory. Other
//! instances read that file to send requests: open a file as a tab, take over a tab
//! moved from another window, or report the window position (for drag and drop).
//! Requests without the token are ignored, so other users cannot send commands.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(3);

/// A tab handed over from another window.
#[derive(Clone, Debug, PartialEq)]
pub struct TabTransfer {
    /// File holding the document (a temporary copy if it had unsaved changes).
    pub file: PathBuf,
    /// Where the document belongs (shown as the tab's path; saving goes there).
    pub original: PathBuf,
    pub page: usize,
    /// The document has unsaved changes.
    pub dirty: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// Open a file as a new tab.
    Open(PathBuf),
    /// Take over a tab from another window.
    Take(TabTransfer),
}

/// What this window tells others about itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WindowInfo {
    /// Window content area in screen coordinates, if the platform reports it
    /// (Wayland does not).
    pub rect: Option<[f32; 4]>,
    pub title: String,
}

/// Another running magic-pdf.
#[derive(Clone, Debug, PartialEq)]
pub struct Peer {
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

/// Held while a magic-pdf that was started with files opens its window, until that
/// window can receive files. Several files opened at once from Explorer start one
/// process each, at the same moment: the first opens the window, the others wait
/// for it and hand their files over, so they all end up as tabs of one window.
const STARTUP_LOCK: &str = "starting";
/// A lock this old is left over from a start that failed.
const STARTUP_STALE: Duration = Duration::from_secs(10);

/// Tries to become the process that opens the window. False if another one is
/// opening its window right now.
pub fn claim_startup() -> bool {
    claim_startup_in(registry_dir())
}

fn claim_startup_in(dir: Option<PathBuf>) -> bool {
    let Some(dir) = dir else { return true };
    if std::fs::create_dir_all(&dir).is_err() {
        return true;
    }
    let path = dir.join(STARTUP_LOCK);
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = write!(f, "{}", std::process::id());
                return true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let age = std::fs::metadata(&path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
                if age.is_some_and(|a| a < STARTUP_STALE) {
                    return false;
                }
                let _ = std::fs::remove_file(&path);
            }
            // Without a usable lock, carry on as before.
            Err(_) => return true,
        }
    }
    true
}

/// Gives the lock up (if this process holds it) once the window receives files.
pub fn release_startup() {
    release_startup_in(registry_dir());
}

fn release_startup_in(dir: Option<PathBuf>) {
    let Some(path) = dir.map(|d| d.join(STARTUP_LOCK)) else { return };
    if std::fs::read_to_string(&path).is_ok_and(|pid| pid.trim() == std::process::id().to_string()) {
        let _ = std::fs::remove_file(&path);
    }
}

fn registry_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "magic-pdf").map(|d| d.data_local_dir().join("instances"))
}

/// A token nobody else can guess: hashes from std's randomly seeded hasher.
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut token = String::new();
    for i in 0..4u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        token += &format!("{:016x}", h.finish());
    }
    token
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n")
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Encodes a request as one line: `<token>\t<COMMAND>\t<args…>`.
fn encode(token: &str, command: &str, args: &[String]) -> String {
    let mut line = format!("{token}\t{command}");
    for a in args {
        line.push('\t');
        line += &escape(a);
    }
    line.push('\n');
    line
}

fn decode(line: &str) -> (String, String, Vec<String>) {
    let mut parts = line.trim_end_matches(['\r', '\n']).split('\t');
    let token = parts.next().unwrap_or_default().to_string();
    let command = parts.next().unwrap_or_default().to_string();
    (token, command, parts.map(unescape).collect())
}

fn parse_message(command: &str, args: &[String]) -> Option<Message> {
    match (command, args) {
        ("OPEN", [path]) => Some(Message::Open(PathBuf::from(path))),
        ("TAKE", [file, original, page, dirty]) => Some(Message::Take(TabTransfer {
            file: PathBuf::from(file),
            original: PathBuf::from(original),
            page: page.parse().ok()?,
            dirty: dirty == "1",
        })),
        _ => None,
    }
}

fn message_args(m: &Message) -> (&'static str, Vec<String>) {
    match m {
        Message::Open(p) => ("OPEN", vec![p.to_string_lossy().into_owned()]),
        Message::Take(t) => (
            "TAKE",
            vec![
                t.file.to_string_lossy().into_owned(),
                t.original.to_string_lossy().into_owned(),
                t.page.to_string(),
                if t.dirty { "1" } else { "0" }.to_string(),
            ],
        ),
    }
}

fn format_window(info: &WindowInfo) -> String {
    let rect = match info.rect {
        Some([x, y, w, h]) => format!("{x} {y} {w} {h}"),
        None => "-".into(),
    };
    format!("{rect}\t{}\n", escape(&info.title))
}

fn parse_window(line: &str) -> WindowInfo {
    let (rect, title) = line.trim_end().split_once('\t').unwrap_or((line.trim_end(), ""));
    let nums: Vec<f32> = rect.split(' ').filter_map(|n| n.parse().ok()).collect();
    WindowInfo { rect: (nums.len() == 4).then(|| [nums[0], nums[1], nums[2], nums[3]]), title: unescape(title) }
}

/// This instance's listener and registry entry (removed on drop).
pub struct Server {
    pub messages: Receiver<Message>,
    pub window: Arc<Mutex<WindowInfo>>,
    registry_file: Option<PathBuf>,
    port: u16,
    token: String,
}

impl Server {
    /// Starts listening. `wake` is called when a message arrives.
    pub fn start(wake: impl Fn() + Send + 'static) -> std::io::Result<Server> {
        Self::start_in(registry_dir(), wake)
    }

    fn start_in(dir: Option<PathBuf>, wake: impl Fn() + Send + 'static) -> std::io::Result<Server> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let token = random_token();
        let window = Arc::new(Mutex::new(WindowInfo::default()));
        let (tx, rx) = std::sync::mpsc::channel();
        let (t, w) = (token.clone(), window.clone());
        std::thread::Builder::new().name("ipc".into()).spawn(move || serve(listener, t, w, tx, wake))?;
        let registry_file = dir.and_then(|dir| {
            std::fs::create_dir_all(&dir).ok()?;
            let file = dir.join(std::process::id().to_string());
            write_private(&file, &format!("{port} {token}\n")).ok()?;
            Some(file)
        });
        Ok(Server { messages: rx, window, registry_file, port, token })
    }

    /// Marks this window as the most recently used one (it then receives files
    /// opened from the desktop).
    pub fn touch(&self) {
        if let Some(f) = &self.registry_file {
            let _ = write_private(f, &format!("{} {}\n", self.port, self.token));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(f) = &self.registry_file {
            let _ = std::fs::remove_file(f);
        }
    }
}

fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)
}

fn serve(listener: TcpListener, token: String, window: Arc<Mutex<WindowInfo>>, tx: Sender<Message>, wake: impl Fn()) {
    for stream in listener.incoming().flatten() {
        let _ = stream.set_read_timeout(Some(TIMEOUT));
        let _ = stream.set_write_timeout(Some(TIMEOUT));
        let mut reader = BufReader::new(&stream);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        let (t, command, args) = decode(&line);
        if t != token {
            continue;
        }
        let mut stream = &stream;
        if command == "WINDOW" {
            let info = window.lock().map(|w| w.clone()).unwrap_or_default();
            let _ = stream.write_all(format_window(&info).as_bytes());
            continue;
        }
        match parse_message(&command, &args) {
            Some(m) => {
                let _ = stream.write_all(b"OK\n");
                if tx.send(m).is_err() {
                    return;
                }
                wake();
            }
            None => {
                let _ = stream.write_all(b"ERR\n");
            }
        }
    }
}

fn request(peer: &Peer, command: &str, args: &[String]) -> std::io::Result<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], peer.port));
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.write_all(encode(&peer.token, command, args).as_bytes())?;
    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply)?;
    Ok(reply)
}

impl Peer {
    pub fn send(&self, m: &Message) -> bool {
        let (command, args) = message_args(m);
        request(self, command, &args).is_ok_and(|r| r.starts_with("OK"))
    }

    pub fn window(&self) -> Option<WindowInfo> {
        request(self, "WINDOW", &[]).ok().map(|r| parse_window(&r))
    }
}

/// Other running instances, most recently used first. Entries of instances that
/// are gone are removed.
pub fn peers() -> Vec<Peer> {
    peers_in(registry_dir())
}

fn peers_in(dir: Option<PathBuf>) -> Vec<Peer> {
    let Some(dir) = dir else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let me = std::process::id();
    let mut found: Vec<(std::time::SystemTime, Peer)> = Vec::new();
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let mut parts = text.split_whitespace();
        let (Some(port), Some(token)) = (parts.next().and_then(|p| p.parse().ok()), parts.next()) else {
            continue;
        };
        let peer = Peer { pid, port, token: token.to_string() };
        // Alive if it answers; stale entries (crashed instances) are cleaned up.
        if peer.window().is_none() {
            let _ = std::fs::remove_file(e.path());
            continue;
        }
        let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
        found.push((modified, peer));
    }
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_round_trip_with_awkward_paths() {
        let t = TabTransfer {
            file: PathBuf::from("/tmp/a\tb\\c\nd.pdf"),
            original: PathBuf::from(r"C:\Users\Me\My Files\x.pdf"),
            page: 12,
            dirty: true,
        };
        let (cmd, args) = message_args(&Message::Take(t.clone()));
        let (token, command, back) = decode(&encode("secret", cmd, &args));
        assert_eq!(token, "secret");
        assert_eq!(parse_message(&command, &back), Some(Message::Take(t)));
        let info = WindowInfo { rect: Some([10.0, 20.0, 800.0, 600.0]), title: "a\tb — magic-pdf".into() };
        assert_eq!(parse_window(&format_window(&info)), info);
        let none = WindowInfo { rect: None, title: String::new() };
        assert_eq!(parse_window(&format_window(&none)), none);
    }

    #[test]
    fn instances_find_and_talk_to_each_other() {
        let dir = std::env::temp_dir().join(format!("magic-pdf-ipc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let server = Server::start_in(Some(dir.clone()), || {}).unwrap();
        *server.window.lock().unwrap() =
            WindowInfo { rect: Some([1.0, 2.0, 3.0, 4.0]), title: "doc.pdf — magic-pdf".into() };
        // Pretend to be another process: the registry entry carries our own pid,
        // so read it directly instead of through `peers_in` (which skips itself).
        let text = std::fs::read_to_string(dir.join(std::process::id().to_string())).unwrap();
        let mut parts = text.split_whitespace();
        let peer =
            Peer { pid: 0, port: parts.next().unwrap().parse().unwrap(), token: parts.next().unwrap().to_string() };
        assert_eq!(peer.window().unwrap().title, "doc.pdf — magic-pdf");
        assert!(peer.send(&Message::Open(PathBuf::from("/x/y.pdf"))));
        assert_eq!(server.messages.recv_timeout(TIMEOUT).unwrap(), Message::Open(PathBuf::from("/x/y.pdf")));
        // A wrong token is ignored.
        let intruder = Peer { token: "guess".into(), ..peer.clone() };
        assert!(!intruder.send(&Message::Open(PathBuf::from("/evil.pdf"))));
        assert!(server.messages.recv_timeout(Duration::from_millis(300)).is_err());
        // The start-up lock: taken once, released only by its holder.
        assert!(claim_startup_in(Some(dir.clone())), "first start takes the lock");
        assert!(!claim_startup_in(Some(dir.clone())), "a second start at the same time waits");
        release_startup_in(Some(dir.clone()));
        assert!(claim_startup_in(Some(dir.clone())), "free again");
        std::fs::write(dir.join(STARTUP_LOCK), "12345").unwrap();
        release_startup_in(Some(dir.clone()));
        assert!(dir.join(STARTUP_LOCK).exists(), "another process's lock is left alone");
        std::fs::remove_file(dir.join(STARTUP_LOCK)).unwrap();
        // A stale entry (no process listening) is cleaned up.
        std::fs::write(dir.join("999999999"), "1 nothing\n").unwrap();
        assert!(peers_in(Some(dir.clone())).is_empty());
        assert!(!dir.join("999999999").exists());
        drop(server);
        assert!(!dir.join(std::process::id().to_string()).exists(), "registry entry removed");
        std::fs::remove_dir_all(&dir).ok();
    }
}
