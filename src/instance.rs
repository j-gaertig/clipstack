//! Single instance on Windows and macOS (Linux uses D-Bus activation): the
//! running service listens on a local port noted in the data directory, and
//! later starts hand it their action ("toggle", "background") and exit.

use std::{
    io::{BufRead, BufReader, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    time::Duration,
};

use crate::history;

const HELLO: &str = "clipstack";

fn port_file() -> PathBuf {
    history::data_dir().join("instance")
}

/// Hands `action` to a running instance; true if there is one.
pub fn forward(action: &str) -> bool {
    let Some(port) = std::fs::read_to_string(port_file()).ok().and_then(|p| p.trim().parse::<u16>().ok()) else {
        return false;
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    if writeln!(stream, "{HELLO} {action}").is_err() {
        return false;
    }
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).is_ok() && reply.trim() == "ok"
}

/// Receives the actions of later starts.
pub fn listen(on_action: impl Fn(&str) + Send + 'static) -> std::io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    std::fs::create_dir_all(history::data_dir())?;
    std::fs::write(port_file(), port.to_string())?;
    std::thread::Builder::new().name("instance".into()).spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            let Ok(reader) = stream.try_clone() else { continue };
            if BufReader::new(reader).read_line(&mut line).is_err() {
                continue;
            }
            if let Some(action) = line.trim().strip_prefix(HELLO) {
                let _ = stream.write_all(b"ok\n");
                on_action(action.trim());
            }
        }
    })?;
    Ok(())
}
