#[macro_use]
extern crate may;
extern crate time;

mod cmd;
mod error;

use std::env;
use std::fs::{File, Metadata, create_dir, read_dir, remove_dir_all};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf, StripPrefixError};
use std::result;
use std::str;

use may::net::{TcpListener, TcpStream};

use cmd::{Command, TransferType};

const CONFIG_FILE: &'static str = "config.toml";
const MONTHS: [&'static str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
                                    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

#[derive(Debug, Clone, Copy)]
#[repr(u32)]
#[allow(dead_code)]
enum ResultCode {
    RestartMarkerReply = 110,
    ServiceReadInXXXMinutes = 120,
    DataConnectionAlreadyOpen = 125,
    FileStatusOk = 150,
    Ok = 200,
    CommandNotImplementedSuperfluousAtThisSite = 202,
    SystemStatus = 211,
    DirectoryStatus = 212,
    FileStatus = 213,
    HelpMessage = 214,
    SystemType = 215,
    ServiceReadyForNewUser = 220,
    ServiceClosingControlConnection = 221,
    DataConnectionOpen = 225,
    ClosingDataConnection = 226,
    EnteringPassiveMode = 227,
    UserLoggedIn = 230,
    RequestedFileActionOkay = 250,
    PATHNAMECreated = 257,
    UserNameOkayNeedPassword = 331,
    NeedAccountForLogin = 332,
    RequestedFileActionPendingFurtherInformation = 350,
    ServiceNotAvailable = 421,
    CantOpenDataConnection = 425,
    ConnectionClosed = 426,
    FileBusy = 450,
    LocalErrorInProcessing = 451,
    InsufficientStorageSpace = 452,
    UnknownCommand = 500,
    InvalidParameterOrArgument = 501,
    CommandNotImplemented = 502,
    BadSequenceOfCommands = 503,
    CommandNotImplementedForThatParameter = 504,
    NotLoggedIn = 530,
    NeedAccountForStoringFiles = 532,
    FileNotFound = 550,
    PageTypeUnknown = 551,
    ExceededStorageAllocation = 552,
    FileNameNotAllowed = 553,
}

#[allow(dead_code)]
struct Client {
    cwd: PathBuf,
    data_port: Option<u16>,
    data_stream: Option<TcpStream>,
    has_quit: bool,
    is_admin: bool,
    name: Option<String>,
    server_root: PathBuf,
    stream: TcpStream,
    transfer_type: TransferType,
}

impl Client {
    fn new(stream: TcpStream, server_root: PathBuf) -> Client {
        Client {
            cwd: PathBuf::from("/"),
            data_port: None,
            data_stream: None,
            has_quit: false,
            is_admin: false,
            name: None,
            server_root,
            stream,
            transfer_type: TransferType::Ascii,
        }
    }

    fn handle_cmd(&mut self, cmd: Command) -> io::Result<()> {
        println!("====> {:?}", cmd);
        match cmd {
            Command::Auth => self.send(ResultCode::CommandNotImplemented, "Not implemented"),
            Command::Cwd(directory) => self.cwd(directory),
            Command::List(path) => self.list(path),
            Command::NoOp => self.send(ResultCode::Ok, "Doing nothing"),
            Command::Pasv => self.pasv(),
            Command::Port(port) => {
                self.data_port = Some(port);
                self.send(ResultCode::Ok, &format!("Data port is now {}", port))
            },
            Command::Pwd => {
                let msg = format!("{}", self.cwd.to_str().unwrap_or("")); // small trick
                if !msg.is_empty() {
                    let message = format!("\"{}\" ", msg);
                    self.send(ResultCode::PATHNAMECreated, &message)
                } else {
                    self.send(ResultCode::FileNotFound, "No such file or directory")
                }
            }
            Command::Quit => self.quit(),
            Command::Syst => self.send(ResultCode::Ok, "I won't tell"),
            Command::Type(typ) => {
                self.transfer_type = typ;
                self.send(ResultCode::Ok, "Transfer type changed successfully")
            },
            Command::User(username) => {
                if username.is_empty() {
                    self.send(ResultCode::InvalidParameterOrArgument, "Invalid username")
                } else {
                    self.name = Some(username.to_owned());
                    self.send(ResultCode::UserLoggedIn, &format!("Welcome {}!", username))
                }
            },
            s => self.send(ResultCode::UnknownCommand, &format!("Not implemented: '{:?}'", s)),
        }
    }

    fn list(&mut self, path: Option<PathBuf>) -> io::Result<()> {
        if self.data_stream.is_some() {
            let path = self.cwd.join(path.unwrap_or_default());
            let directory = PathBuf::from(&path);
            let res = self.complete_path(directory);
            if let Ok(path) = res {
                self.send(ResultCode::DataConnectionAlreadyOpen, "Starting to list directory...")?;
                let mut out = vec![];
                if path.is_dir() {
                    if let Ok(dir) = read_dir(path) {
                        for entry in dir {
                            if let Ok(entry) = entry {
                                if self.is_admin ||
                                   entry.path() != self.server_root.join(CONFIG_FILE) {
                                    add_file_info(entry.path(), &mut out);
                                }
                            }
                        }
                    } else {
                        self.send(ResultCode::InvalidParameterOrArgument, "No such file or directory")?;
                        return Ok(());
                    }
                } else if self.is_admin || path != self.server_root.join(CONFIG_FILE) {
                    add_file_info(path, &mut out);
                }
                self.send_data(out)?;
                println!("-> and done!");
            } else {
                self.send(ResultCode::InvalidParameterOrArgument, "No such file or directory")?;
            }
        } else {
            self.send(ResultCode::ConnectionClosed, "No opened data connection")?;
        }
        if self.data_stream.is_some() {
            self.close_data_connection();
            self.send(ResultCode::ClosingDataConnection, "Transfer done")?;
        }
        Ok(())
    }

    fn close_data_connection(&mut self) {
        self.data_stream = None;
    }

    fn pasv(&mut self) -> io::Result<()> {
        let port =
            if let Some(port) = self.data_port {
                port
            } else {
                0
            };
        if self.data_stream.is_some() {
            return self.send(ResultCode::DataConnectionAlreadyOpen, "Already listening...");
        }

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port);
        let listener = TcpListener::bind(&addr)?;
        let port = listener.local_addr()?.port();

        self.send(ResultCode::EnteringPassiveMode, &format!("127,0,0,1,{},{}", port >> 8, port & 0xFF))?;

        println!("Waiting clients on port {}...", port);
        match listener.incoming().next() {
            Some(stream) => self.data_stream = Some(stream?),
            None => unreachable!(),
        }

        Ok(())
    }

    fn cwd(&mut self, directory: PathBuf) -> io::Result<()> {
        let path = self.cwd.join(&directory);
        let res = self.complete_path(path);
        if let Ok(dir) = res {
            let res = self.strip_prefix(dir);
            if let Ok(prefix) = res {
                self.cwd = prefix.to_path_buf();
                prefix_slash(&mut self.cwd);
                self.send(ResultCode::RequestedFileActionOkay, &format!("Directory changed to \"{}\"",
                                                             directory.display()))?;
            }
        }
        self.send(ResultCode::FileNotFound, "No such file or directory")
    }

    fn complete_path(&self, path: PathBuf) -> io::Result<PathBuf> {
        let directory = self.server_root.join(if path.has_root() {
            path.iter().skip(1).collect()
        } else {
            path
        });
        let dir = directory.canonicalize();
        if let Ok(ref dir) = dir {
            if !dir.starts_with(&self.server_root) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        dir
    }

    fn strip_prefix(&self, dir: PathBuf) -> result::Result<PathBuf, StripPrefixError> {
        dir.strip_prefix(&self.server_root).map(|p| p.to_path_buf())
    }

    fn send(&mut self, result: ResultCode, msg: &str) -> io::Result<()> {
        send_cmd(&mut self.stream, result, msg)
    }

    fn send_data(&mut self, data: Vec<u8>) -> io::Result<()> {
        if let Some(ref mut stream) = self.data_stream {
            stream.write_all(&data)?;
        }
        Ok(())
    }

    fn run(&mut self) -> io::Result<()> {
        while !self.has_quit {
            let data = read_all_message(&mut self.stream);
            if data.is_empty() {
                println!("client disconnected...");
                break;
            }
            if let Ok(command) = Command::new(data) {
                self.handle_cmd(command)?;
            } else {
                println!("Error with client command...");
            }
        }
        Ok(())
    }

    fn quit(&mut self) -> io::Result<()> {
        // TODO
        if self.data_stream.is_some() {
            unimplemented!();
        } else {
            self.send(ResultCode::ServiceClosingControlConnection, "Closing connection...")?;
            self.has_quit = true;
        }
        Ok(())
    }
}

fn read_all_message(stream: &mut TcpStream) -> Vec<u8> {
    let buf = &mut [0; 1];
    let mut out = Vec::with_capacity(100);

    loop {
        match stream.read(buf) {
            Ok(received) if received > 0 => {
                if out.is_empty() && buf[0] == b' ' {
                    continue
                }
                out.push(buf[0]);
            }
            _ => return Vec::new(),
        }
        let len = out.len();
        if len > 1 && out[len - 2] == b'\r' && out[len - 1] == b'\n' {
            out.pop();
            out.pop();
            return out;
        }
    }
}

fn send_cmd(stream: &mut TcpStream, code: ResultCode, message: &str) -> io::Result<()> {
    let msg = if message.is_empty() {
        format!("{}\r\n", code as u32)
    } else {
        format!("{} {}\r\n", code as u32, message)
    };
    println!("<==== {}", msg);
    write!(stream, "{}", msg)
}

fn handle_client(mut stream: TcpStream, server_root: PathBuf) -> io::Result<()> {
    println!("new client connected!");
    send_cmd(&mut stream, ResultCode::ServiceReadyForNewUser, "Welcome to this FTP server!")?;
    let mut client = Client::new(stream, server_root);
    client.run()
}

fn server() -> io::Result<()> {
    let listener = TcpListener::bind("0.0.0.0:1234")?;
    let server_root = env::current_dir()?;

    println!("Waiting for clients to connect...");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server_root = server_root.clone();
                go!(|| {
                    if let Err(error) = handle_client(stream, server_root) {
                        println!("Error handling client: {}", error)
                    }
                });
            }
            _ => {
                println!("A client tried to connect...")
            }
        }
    }
    Ok(())
}

fn main() {
    if let Err(error) = server() {
        println!("Error running the server: {}", error);
    }
}

fn prefix_slash(path: &mut PathBuf) {
    if !path.is_absolute() {
        *path = Path::new("/").join(&path);
    }
}

// If an error occurs when we try to get file's information, we just return and don't send its info.
fn add_file_info(path: PathBuf, out: &mut Vec<u8>) {
    let extra = if path.is_dir() { "/" } else { "" };
    let is_dir = if path.is_dir() { "d" } else { "-" };

    let meta = match ::std::fs::metadata(&path) {
        Ok(meta) => meta,
        _ => return,
    };
    let (time, file_size) = get_file_info(&meta);
    let path = match path.to_str() {
        Some(path) => match path.split("/").last() {
            Some(path) => path,
            _ => return,
        },
        _ => return,
    };
    // TODO: maybe improve how we get rights in here?
    let rights = if meta.permissions().readonly() {
        "r--r--r--"
    } else {
        "rw-rw-rw-"
    };
    let file_str = format!("{is_dir}{rights} {links} {owner} {group} {size} {month} {day} {hour}:{min} {path}{extra}\r\n",
                           is_dir=is_dir,
                           rights=rights,
                           links=1, // number of links
                           owner="anonymous", // owner name
                           group="anonymous", // group name
                           size=file_size,
                           month=MONTHS[time.tm_mon as usize],
                           day=time.tm_mday,
                           hour=time.tm_hour,
                           min=time.tm_min,
                           path=path,
                           extra=extra);
    out.extend(file_str.as_bytes());
    println!("==> {:?}", &file_str);
}

#[cfg(windows)]
fn get_file_info(meta: &Metadata) -> (time::Tm, u64) {
    use std::os::windows::prelude::*;
    (time::at(time::Timespec::new((meta.last_write_time() / 10_000_000) as i64, 0)),
    meta.file_size())
}

#[cfg(not(windows))]
fn get_file_info(meta: &Metadata) -> (time::Tm, u64) {
    use std::os::unix::prelude::*;
    (time::at(time::Timespec::new(meta.mtime(), 0)), meta.size())
}
