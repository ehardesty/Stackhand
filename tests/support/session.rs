use std::fs;
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, NativePtySystem, PtySize, PtySystem,
};

const INITIALLY_SELECTED_PROCESS: &str = "pty";

pub const WAIT: Duration = Duration::from_secs(25);
static NEXT_PROJECT: AtomicUsize = AtomicUsize::new(0);

pub struct Project {
    pub root: PathBuf,
    pub config: PathBuf,
    identities: Vec<(u32, String)>,
}

impl Project {
    pub fn new() -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "stackhand-session-{}-{suffix}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        let script = root.join("process.pl");
        fs::write(
            &script,
            r#"use strict;
use warnings;
my ($root, $name) = @ARGV;
$SIG{$_} = 'IGNORE' for qw(HUP INT TERM);
open my $pid_file, '>', "$root/$name.pid" or die $!;
print $pid_file $$;
close $pid_file;
if ($name !~ /-descendant$/) {
    my $child = fork();
    defined $child or die $!;
    if ($child == 0) { exec $^X, $0, $root, "$name-descendant"; die $!; }
    if ($name eq 'pty') {
        system('stty', 'raw', '-echo') == 0 or die 'stty failed';
        $| = 1;
        print "session-ready\n";
        open my $input, '>>', "$root/input" or die $!;
        binmode $input;
        while (sysread(STDIN, my $data, 1)) { syswrite($input, $data); }
        exit;
    }
}
sleep 1 while 1;
"#,
        )
        .unwrap();
        let config = root.join("stackhand.yaml");
        let mut yaml = String::from("version: 1\nprocesses:\n");
        for (mode, input) in [
            (INITIALLY_SELECTED_PROCESS, "focused"),
            ("pipe", "disabled"),
        ] {
            yaml.push_str(&format!("  {mode}:\n    command: [perl, '{}', '{}', {mode}]\n    terminal:\n      mode: {mode}\n      input: {}\n", script.display(), root.display(), input));
        }
        fs::write(&config, yaml).unwrap();
        Self {
            root,
            config,
            identities: Vec::new(),
        }
    }

    pub fn capture_trees(&mut self) {
        for name in ["pty", "pty-descendant", "pipe", "pipe-descendant"] {
            let path = self.root.join(format!("{name}.pid"));
            wait_until(
                || fs::read_to_string(&path).is_ok_and(|s| s.parse::<u32>().is_ok()),
                "Process PID file",
            );
            let pid = fs::read_to_string(path).unwrap().parse().unwrap();
            let identity = process_identity(pid).expect("fixture Process is alive");
            assert!(
                identity.contains(self.root.to_str().unwrap()),
                "wrong Process identity: {identity}"
            );
            self.identities.push((pid, identity));
        }
    }

    pub fn assert_no_start(&self) {
        thread::sleep(Duration::from_secs(2));
        for name in ["pty", "pty-descendant", "pipe", "pipe-descendant"] {
            assert!(
                !self.root.join(format!("{name}.pid")).exists(),
                "unexpected Process start: {name}"
            );
        }
    }

    pub fn assert_trees_gone(&self) {
        wait_until(
            || {
                self.identities
                    .iter()
                    .all(|(pid, identity)| process_identity(*pid).as_ref() != Some(identity))
            },
            "all owned Process Trees to exit",
        );
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        kill_every_recorded_fixture_process(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn kill_every_recorded_fixture_process(root: &Path) {
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "pid")
                && let Ok(pid) = fs::read_to_string(entry.path())
                    .unwrap_or_default()
                    .parse::<u32>()
                && process_identity(pid)
                    .is_some_and(|identity| identity.contains(root.to_str().unwrap()))
            {
                unsafe {
                    libc::kill(pid as i32, libc::SIGKILL);
                }
            }
        }
    }
}

fn process_identity(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart=", "-o", "command="])
        .output()
        .unwrap();
    let identity = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!identity.is_empty()).then_some(identity)
}

pub fn wait_until(mut predicate: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + WAIT;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

pub struct Terminal {
    master: Option<Box<dyn MasterPty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    reader: Option<JoinHandle<()>>,
    stop_reader: Arc<AtomicBool>,
    pause_reader: Arc<AtomicBool>,
    output: Arc<Mutex<Vec<u8>>>,
    #[cfg(target_os = "linux")]
    stopped_output: Option<OwnedFd>,
    pub child: Box<dyn Child + Send + Sync>,
}

impl Terminal {
    pub fn start(project: &Project, arguments: &[&str], redirection: Option<&str>) -> Self {
        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = if let Some(redirection) = redirection {
            let mut command = CommandBuilder::new("/bin/sh");
            command.args(["-c", &format!("exec \"$@\" {redirection}"), "session-test"]);
            command.arg(env!("CARGO_BIN_EXE_stackhand"));
            command
        } else {
            CommandBuilder::new(env!("CARGO_BIN_EXE_stackhand"))
        };
        command.env("TERM", "xterm-256color");
        command.arg(&project.config);
        command.args(arguments);
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);

        let fd = pair.master.as_raw_fd().unwrap();
        let mut input = pair.master.try_clone_reader().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&output);
        let stop_reader = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stop_reader);
        let pause_reader = Arc::new(AtomicBool::new(false));
        let pause = Arc::clone(&pause_reader);
        let reader = thread::spawn(move || {
            let mut buffer = [0; 8192];
            while !stop.load(Ordering::Acquire) {
                if pause.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                let mut poll = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut poll, 1, 20) } <= 0 {
                    continue;
                }
                match input.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => captured.lock().unwrap().extend_from_slice(&buffer[..count]),
                }
            }
        });
        let writer = pair.master.take_writer().unwrap();
        Self {
            master: Some(pair.master),
            writer: Some(writer),
            reader: Some(reader),
            stop_reader,
            pause_reader,
            output,
            #[cfg(target_os = "linux")]
            stopped_output: None,
            child,
        }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        let writer = self.writer.as_mut().unwrap();
        writer.write_all(bytes).unwrap();
        writer.flush().unwrap();
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    pub fn pause_output_and_resize(&mut self) {
        self.pause_reader.store(true, Ordering::Release);
        #[cfg(target_os = "linux")]
        {
            let master = self.master.as_ref().unwrap().as_raw_fd().unwrap();
            let slave = unsafe {
                libc::ioctl(
                    master,
                    libc::TIOCGPTPEER,
                    libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
                )
            };
            assert!(slave >= 0, "{}", std::io::Error::last_os_error());
            let slave = unsafe { OwnedFd::from_raw_fd(slave) };
            assert_eq!(unsafe { libc::tcflow(slave.as_raw_fd(), libc::TCOOFF) }, 0);
            self.stopped_output = Some(slave);
        }
        self.master
            .as_ref()
            .unwrap()
            .resize(PtySize {
                rows: 200,
                cols: 500,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
    }

    pub fn hangup(&mut self) {
        self.stop_reader.store(true, Ordering::Release);
        self.reader.take().unwrap().join().unwrap();
        self.writer.take();
        self.master.take();
    }

    pub fn wait(&mut self) -> ExitStatus {
        let mut status = None;
        wait_until(
            || {
                status = self.child.try_wait().unwrap();
                status.is_some()
            },
            "Stackhand to exit",
        );
        self.resume_output();
        if let Some(reader) = self.reader.take() {
            wait_until(|| reader.is_finished(), "terminal output reader to finish");
            reader.join().unwrap();
        }
        status.unwrap()
    }

    fn resume_output(&mut self) {
        #[cfg(target_os = "linux")]
        if let Some(slave) = self.stopped_output.take() {
            unsafe { libc::tcflow(slave.as_raw_fd(), libc::TCOON) };
        }
        self.pause_reader.store(false, Ordering::Release);
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.resume_output();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stop_reader.store(true, Ordering::Release);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
