//! `herdr control`: a control stream on stdio.
//!
//! Opens one control stream on the socket API and copies newline-delimited
//! JSON between it and stdio, so a remote client can drive Herdr over an
//! SSH exec channel without a PTY.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::time::Duration;

use interprocess::local_socket::traits::Stream as _;
use interprocess::TryClone as _;

use crate::api::schema::{ControlClientInfo, ControlOpenParams, Method, Request};
use crate::ipc::LocalStream;

const USAGE: &str = "usage: herdr [--session NAME] control";
const SERVER_START_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn run_control_command(args: &[String]) -> io::Result<i32> {
    match args.first().map(String::as_str) {
        None => {}
        Some("help" | "--help" | "-h") => {
            print_help();
            return Ok(0);
        }
        Some(other) => {
            eprintln!("unknown option: {other}");
            eprintln!("{USAGE}");
            return Ok(2);
        }
    }

    ensure_server_running()?;
    let socket_path = crate::api::socket_path();
    let mut stream = crate::ipc::connect_local_stream(&socket_path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to connect to Herdr API socket {}: {err}",
                socket_path.display()
            ),
        )
    })?;
    stream.set_nonblocking(false)?;

    let open = Request {
        id: "control".into(),
        method: Method::ControlOpen(ControlOpenParams {
            client: client_info_from_env(),
        }),
    };
    let mut line = serde_json::to_string(&open).map_err(io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut opened = String::new();
    if reader.read_line(&mut opened)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Herdr closed the control stream before answering control.open",
        ));
    }
    let mut stdout = io::stdout().lock();
    stdout.write_all(opened.as_bytes())?;
    stdout.flush()?;
    let failed = serde_json::from_str::<serde_json::Value>(opened.trim())
        .map(|value| value.get("error").is_some())
        .unwrap_or(true);
    if failed {
        return Ok(1);
    }

    forward_stdio(reader, stream, stdout)?;
    Ok(0)
}

/// Identity from the environment, so a remote client can name itself
/// without flags an older `herdr control` would reject.
/// `HERDR_CONTROL_CLIENT="name/version"`, `HERDR_CONTROL_PROTOCOL=2`.
/// Neither set sends no `client` at all: a client that predates both keeps
/// the protocol 1 contract exactly as with an older binary.
fn client_info_from_env() -> Option<ControlClientInfo> {
    let protocol = std::env::var("HERDR_CONTROL_PROTOCOL")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok());
    let label = std::env::var("HERDR_CONTROL_CLIENT")
        .ok()
        .filter(|label| !label.trim().is_empty());
    if protocol.is_none() && label.is_none() {
        return None;
    }
    let protocol = protocol.unwrap_or(1);
    let (name, version) = match label {
        Some(label) => {
            let mut parts = label.trim().splitn(2, '/');
            (
                parts.next().unwrap_or_default().to_owned(),
                parts.next().unwrap_or_default().to_owned(),
            )
        }
        None => ("herdr-control".into(), crate::build_info::version()),
    };
    Some(ControlClientInfo {
        name,
        version,
        protocol,
    })
}

fn forward_stdio(
    mut socket_reader: BufReader<LocalStream>,
    mut socket_writer: LocalStream,
    mut stdout: io::StdoutLock<'_>,
) -> io::Result<()> {
    let upload = std::thread::spawn(move || -> io::Result<()> {
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let read = match stdin.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
            };
            socket_writer.write_all(&buffer[..read])?;
            socket_writer.flush()?;
        }
        // Ask the server to close so the download side sees EOF promptly.
        let close = serde_json::json!({"id":"control:close","method":"control.close","params":{}});
        let _ = socket_writer.write_all(format!("{close}\n").as_bytes());
        let _ = socket_writer.flush();
        Ok(())
    });

    let mut buffer = [0_u8; 16 * 1024];
    let download = loop {
        let read = match socket_reader.read(&mut buffer) {
            Ok(0) => break Ok(()),
            Ok(read) => read,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) if crate::ipc::is_connection_closed_error(&err) => break Ok(()),
            Err(err) => break Err(err),
        };
        if let Err(err) = stdout
            .write_all(&buffer[..read])
            .and_then(|()| stdout.flush())
        {
            break Err(err);
        }
    };
    if upload.is_finished() {
        let _ = upload.join();
    }
    download
}

fn ensure_server_running() -> io::Result<()> {
    if crate::server::autodetect::is_server_listening()? {
        return Ok(());
    }
    crate::server::autodetect::spawn_server_daemon()?;
    crate::server::autodetect::wait_for_server_socket(
        &crate::server::socket_paths::client_socket_path(),
        SERVER_START_TIMEOUT,
    )?;
    let deadline = std::time::Instant::now() + SERVER_START_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if crate::api::socket_path().exists() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "Herdr server started but its API socket did not appear",
    ))
}

fn print_help() {
    eprintln!("{USAGE}");
    eprintln!();
    eprintln!("Opens a control stream and bridges it to stdin/stdout as newline-delimited JSON.");
    eprintln!("The first line printed is the control.open response with the server's");
    eprintln!("boot id and capabilities. Send requests on stdin; responses, subscription");
    eprintln!("events, and terminal records arrive on stdout. EOF on stdin closes the stream.");
    eprintln!();
    eprintln!("HERDR_CONTROL_CLIENT=name/version names the client on control.open;");
    eprintln!("HERDR_CONTROL_PROTOCOL caps the control stream protocol it negotiates.");
}
