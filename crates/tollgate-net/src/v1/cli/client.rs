//! Synchronous CLI client that connects to the running server's Unix domain socket.
//!
//! Used by the `tollgate cli` subcommand (and the `tollgate` symlink) to send
//! commands like `tollgate --json status` to the server.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::types::{CLIMessage, CLIResponse};

const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run_cli_client(
    socket_path: &str,
    command: &str,
    args: &[String],
    json_mode: bool,
) -> Result<(), String> {
    let stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("Failed to connect to {socket_path}: {e}"))?;

    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| format!("Failed to set read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(SOCKET_TIMEOUT))
        .map_err(|e| format!("Failed to set write timeout: {e}"))?;

    let mut writer = &stream;
    let reader = BufReader::new(&stream);

    let msg = CLIMessage {
        command: command.to_owned(),
        args: args.to_vec(),
        flags: HashMap::new(),
    };

    let mut request =
        serde_json::to_string(&msg).map_err(|e| format!("Failed to serialize request: {e}"))?;
    request.push('\n');

    writer
        .write_all(request.as_bytes())
        .map_err(|e| format!("Failed to write to socket: {e}"))?;
    writer
        .flush()
        .map_err(|e| format!("Failed to flush socket: {e}"))?;

    let response_line = {
        let mut lines = reader.lines();
        lines
            .next()
            .ok_or_else(|| "No response from server".to_owned())?
            .map_err(|e| format!("Failed to read response: {e}"))?
    };

    let resp: CLIResponse = serde_json::from_str(&response_line)
        .map_err(|e| format!("Failed to parse response: {e}"))?;

    if json_mode {
        println!("{response_line}");
    } else if resp.success {
        if let Some(msg) = &resp.message {
            println!("{msg}");
        }
    } else if let Some(err) = &resp.error {
        eprintln!("{err}");
    }

    if resp.success {
        Ok(())
    } else {
        Err(resp.error.unwrap_or_else(|| "Unknown error".to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    fn setup_mock_server(
        handler: Box<dyn Fn(CLIMessage) -> CLIResponse + Send + 'static>,
    ) -> (tempfile::TempDir, String, thread::JoinHandle<()>) {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");
        let socket_path_str = socket_path.to_str().unwrap().to_owned();
        let listener = UnixListener::bind(&socket_path).unwrap();

        let handle = thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                stream.set_read_timeout(Some(SOCKET_TIMEOUT)).unwrap();
                stream.set_write_timeout(Some(SOCKET_TIMEOUT)).unwrap();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap();
                let incoming = String::from_utf8_lossy(&buf[..n]);
                let msg: CLIMessage = serde_json::from_str(incoming.trim()).unwrap();
                let resp = handler(msg);
                let mut output = serde_json::to_string(&resp).unwrap();
                output.push('\n');
                stream.write_all(output.as_bytes()).unwrap();
                stream.flush().unwrap();
            }
        });

        thread::sleep(Duration::from_millis(100));

        (dir, socket_path_str, handle)
    }

    #[test]
    fn client_status_success_human_readable() {
        let (_dir, socket_path, handle) = setup_mock_server(Box::new(|msg| {
            assert_eq!(msg.command, "status");
            CLIResponse::ok("Service status retrieved")
        }));

        let result = run_cli_client(&socket_path, "status", &[], false);
        assert!(result.is_ok());
        handle.join().unwrap();
    }

    #[test]
    fn client_status_success_json_mode() {
        let (_dir, socket_path, handle) = setup_mock_server(Box::new(|msg| {
            assert_eq!(msg.command, "status");
            CLIResponse::ok_with_data(
                "Service status retrieved",
                serde_json::json!({"running": true}),
            )
        }));

        let result = run_cli_client(&socket_path, "status", &[], true);
        assert!(result.is_ok());
        handle.join().unwrap();
    }

    #[test]
    fn client_error_response() {
        let (_dir, socket_path, handle) = setup_mock_server(Box::new(|msg| {
            assert_eq!(msg.command, "unknown_cmd");
            CLIResponse::error("Unknown command: unknown_cmd")
        }));

        let result = run_cli_client(&socket_path, "unknown_cmd", &[], false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown command"));
        handle.join().unwrap();
    }

    #[test]
    fn client_sends_args() {
        let (_dir, socket_path, handle) = setup_mock_server(Box::new(|msg| {
            assert_eq!(msg.command, "wallet");
            assert_eq!(msg.args, vec!["balance"]);
            CLIResponse::ok_with_data(
                "Total wallet balance: 500 sats",
                serde_json::json!({"balance": 500}),
            )
        }));

        let result = run_cli_client(&socket_path, "wallet", &["balance".to_owned()], true);
        assert!(result.is_ok());
        handle.join().unwrap();
    }

    #[test]
    fn client_connection_refused() {
        let result = run_cli_client("/nonexistent/path.sock", "status", &[], false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to connect"));
    }

    #[test]
    fn client_empty_flags() {
        let (_dir, socket_path, handle) = setup_mock_server(Box::new(|msg| {
            assert!(msg.flags.is_empty());
            CLIResponse::ok("ok")
        }));

        let result = run_cli_client(&socket_path, "version", &[], false);
        assert!(result.is_ok());
        handle.join().unwrap();
    }

    #[test]
    fn client_wallet_with_fund_args() {
        let (_dir, socket_path, handle) = setup_mock_server(Box::new(|msg| {
            assert_eq!(msg.command, "wallet");
            assert_eq!(msg.args, vec!["fund", "cashuA_test_token"]);
            CLIResponse::ok_with_data(
                "Successfully funded wallet with 100 sats",
                serde_json::json!({"amount_received": 100}),
            )
        }));

        let result = run_cli_client(
            &socket_path,
            "wallet",
            &["fund".to_owned(), "cashuA_test_token".to_owned()],
            false,
        );
        assert!(result.is_ok());
        handle.join().unwrap();
    }
}
