//! The engine process: spawn, three-phase readiness, stop.
//!
//! Readiness is the port line on stdout, then `/health`, then one
//! authenticated request, every phase raced against the process exiting and
//! against one stall timer. The token check is the phase that matters most:
//! the adapter turns every 401 into a sign-in prompt, so a token the engine
//! does not accept must fail here, where it can be named. This module is
//! written against smol alone so it can be tested by spawning a real process
//! from a plain test.

use anyhow::{Result, anyhow};
use futures::{
    AsyncBufReadExt as _, FutureExt as _, StreamExt as _,
    channel::oneshot,
    future::{self, BoxFuture, Either, Shared},
    io::BufReader,
};
use http_client::{AsyncBody, HttpClient, Method, Request};
use smol::process::ChildStdin;
use std::{
    collections::VecDeque,
    path::PathBuf,
    pin::pin,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use util::{ResultExt as _, process::Child};

pub const STALL_TIMEOUT: Duration = Duration::from_secs(60);
pub const STOP_TIMEOUT: Duration = Duration::from_secs(6);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const EXIT_GRACE: Duration = Duration::from_millis(250);
const STDERR_TAIL_LINES: usize = 40;

/// A real timer. This module runs outside gpui on purpose: a stalled engine
/// must be detected while a test is parked on real pipe I/O, which the
/// simulated clock would not advance.
#[allow(clippy::disallowed_methods)]
fn sleep(duration: Duration) -> smol::Timer {
    smol::Timer::after(duration)
}

pub struct EngineCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub set: Vec<(String, String)>,
    pub remove: Vec<&'static str>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("could not start knightcode-engine at {}: {source:#}", .path.display())]
    Spawn {
        path: PathBuf,
        #[source]
        source: anyhow::Error,
    },
    #[error("knightcode-engine exited before it was ready ({status}){}", tail(.stderr))]
    ExitedBeforeReady { status: ExitStatus, stderr: String },
    #[error("knightcode-engine did not become ready within {timeout:?}{}", tail(.stderr))]
    Stalled { timeout: Duration, stderr: String },
    #[error(
        "knightcode-engine rejected the launch token (HTTP {status}); the binary is not the one this IDE was built with"
    )]
    TokenRejected { status: u16 },
    #[error("knightcode-engine: {0:#}")]
    Io(anyhow::Error),
}

fn tail(stderr: &str) -> String {
    if stderr.trim().is_empty() {
        String::new()
    } else {
        format!(":\n{}", stderr.trim_end())
    }
}

/// The last lines the engine wrote to stderr, kept for error messages.
#[derive(Clone, Default)]
struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn push(&self, line: String) {
        let mut lines = self.0.lock().unwrap();
        if lines.len() == STDERR_TAIL_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    fn snapshot(&self) -> String {
        self.0
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug)]
pub struct ExitOutcome {
    /// `None` when the wait itself failed.
    pub status: Option<ExitStatus>,
    pub stderr: String,
}

pub struct EngineProcess {
    pub port: u16,
    stdin: Option<ChildStdin>,
    kill: Option<oneshot::Sender<()>>,
    exit: Shared<BoxFuture<'static, ExitOutcome>>,
}

impl std::fmt::Debug for EngineProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineProcess")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

/// `{"type":"listening","port":N}`, wherever it starts on the line; anything
/// else is not a port line. Text before the object is tolerated because a
/// runtime may print its own prefix on the same line.
pub fn parse_port_line(line: &str) -> Option<u16> {
    let object = &line[line.find('{')?..];
    let value: serde_json::Value = serde_json::from_str(object.trim()).ok()?;
    if value.get("type")?.as_str()? != "listening" {
        return None;
    }
    u16::try_from(value.get("port")?.as_u64()?).ok()
}

enum Phase {
    Ready(Result<u16, StartError>),
    Exited(std::io::Result<ExitStatus>),
    Stalled,
}

pub async fn start(
    command: EngineCommand,
    token: &str,
    http: Arc<dyn HttpClient>,
    stall_timeout: Duration,
) -> Result<EngineProcess, StartError> {
    let mut std_command = util::command::new_std_command(&command.program);
    std_command.args(&command.args);
    for (key, value) in &command.set {
        std_command.env(key, value);
    }
    for key in &command.remove {
        std_command.env_remove(key);
    }
    let mut child = Child::spawn(std_command, Stdio::piped(), Stdio::piped(), Stdio::piped())
        .map_err(|source| StartError::Spawn {
            path: command.program.clone(),
            source,
        })?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr_pipe = child.stderr.take().expect("stderr is piped");

    // Drained for the life of the process: a full pipe would block the engine.
    let stderr = StderrTail::default();
    smol::spawn({
        let stderr = stderr.clone();
        async move {
            let mut lines = BufReader::new(stderr_pipe).lines();
            while let Some(Ok(line)) = lines.next().await {
                log::warn!("knightcode-engine: {line}");
                stderr.push(line);
            }
        }
    })
    .detach();

    let ready = async {
        let mut lines = BufReader::new(stdout).lines();
        let port = loop {
            match lines.next().await {
                Some(Ok(line)) => match parse_port_line(&line) {
                    Some(port) => break port,
                    None => log::info!("knightcode-engine: {line}"),
                },
                Some(Err(error)) => return Err(StartError::Io(error.into())),
                None => {
                    return Err(StartError::Io(anyhow!(
                        "stdout closed before the listening line"
                    )));
                }
            }
        };
        smol::spawn(async move { while let Some(Ok(_)) = lines.next().await {} }).detach();

        let url = format!("http://127.0.0.1:{port}");
        loop {
            if let Ok(response) = http
                .get(&format!("{url}/health"), AsyncBody::empty(), false)
                .await
                && response.status().is_success()
            {
                break;
            }
            sleep(POLL_INTERVAL).await;
        }

        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("{url}/v1/accounts"))
            .header("Authorization", format!("Bearer {token}"))
            .body(AsyncBody::empty())
            .map_err(|error| StartError::Io(error.into()))?;
        let response = http.send(request).await.map_err(StartError::Io)?;
        if response.status().as_u16() == 401 {
            return Err(StartError::TokenRejected { status: 401 });
        }
        Ok(port)
    };

    let phase = {
        let ready = pin!(ready);
        let exited = pin!(child.status());
        let deadline = pin!(sleep(stall_timeout));
        match future::select(future::select(ready, exited), deadline).await {
            Either::Left((Either::Left((result, _)), _)) => Phase::Ready(result),
            Either::Left((Either::Right((status, _)), _)) => Phase::Exited(status),
            Either::Right(_) => Phase::Stalled,
        }
    };

    let port = match phase {
        Phase::Ready(Ok(port)) => port,
        Phase::Ready(Err(error)) => {
            // The engine may have exited between its last line and our read;
            // its status and stderr are the better report.
            let exited = {
                let status = pin!(child.status());
                let grace = pin!(sleep(EXIT_GRACE));
                match future::select(status, grace).await {
                    Either::Left((Ok(status), _)) => Some(status),
                    _ => None,
                }
            };
            return Err(match exited {
                Some(status) => StartError::ExitedBeforeReady {
                    status,
                    stderr: stderr.snapshot(),
                },
                None => {
                    child.kill().log_err();
                    error
                }
            });
        }
        Phase::Exited(status) => {
            let status = status.map_err(|error| StartError::Io(error.into()))?;
            return Err(StartError::ExitedBeforeReady {
                status,
                stderr: stderr.snapshot(),
            });
        }
        Phase::Stalled => {
            child.kill().log_err();
            return Err(StartError::Stalled {
                timeout: stall_timeout,
                stderr: stderr.snapshot(),
            });
        }
    };

    // From here the child lives inside the exit future: whoever awaits it
    // observes the exit, and a kill request is honoured by the same future.
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let exit = {
        let stderr = stderr.clone();
        async move {
            let mut child = child;
            let exited = {
                let status = pin!(child.status());
                match future::select(status, kill_rx).await {
                    Either::Left((status, _)) => Some(status),
                    Either::Right(_) => None,
                }
            };
            let status = match exited {
                Some(status) => status,
                None => {
                    child.kill().log_err();
                    child.status().await
                }
            };
            ExitOutcome {
                status: status.ok(),
                stderr: stderr.snapshot(),
            }
        }
        .boxed()
        .shared()
    };

    Ok(EngineProcess {
        port,
        stdin,
        kill: Some(kill_tx),
        exit,
    })
}

impl EngineProcess {
    /// Resolves when the process has exited. Clone-able; every holder sees
    /// the same outcome.
    pub fn exit(&self) -> Shared<BoxFuture<'static, ExitOutcome>> {
        self.exit.clone()
    }

    /// Close stdin. The engine shuts itself down on EOF; this is all a quit
    /// can afford inside gpui's 200 ms.
    pub fn release(&mut self) {
        self.stdin.take();
    }

    pub fn kill(&mut self) {
        if let Some(kill) = self.kill.take() {
            kill.send(()).ok();
        }
    }

    /// Release, wait up to `STOP_TIMEOUT`, then kill.
    pub async fn stop(mut self) -> ExitOutcome {
        self.release();
        let exit = self.exit();
        let outcome = {
            let exit = pin!(exit);
            let deadline = pin!(sleep(STOP_TIMEOUT));
            match future::select(exit, deadline).await {
                Either::Left((outcome, _)) => Some(outcome),
                Either::Right(_) => None,
            }
        };
        match outcome {
            Some(outcome) => outcome,
            None => {
                log::warn!("knightcode-engine did not exit within {STOP_TIMEOUT:?}; killing it");
                self.kill();
                self.exit().await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::{PORT_ENV, engine_environment};
    use http_client::{FakeHttpClient, Response};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

    /// The test binary is the fake engine. Spawned with `KNIGHTCODE_FAKE_ENGINE`
    /// set, this "test" behaves like an engine in the named mode and never
    /// returns to the harness. The harness's own lines on stdout ("running 1
    /// test") are what a real engine's stray output looks like, and the port
    /// reader must skip them.
    #[test]
    fn fake_engine() {
        let Ok(mode) = std::env::var("KNIGHTCODE_FAKE_ENGINE") else {
            return;
        };
        match mode.as_str() {
            "listening" => {
                let port = std::env::var(PORT_ENV).unwrap_or_else(|_| "4321".into());
                println!("{{\"type\":\"listening\",\"port\":{port}}}");
                // Like the real engine: exit when stdin closes.
                let _ = std::io::stdin().read_line(&mut String::new());
                std::process::exit(0);
            }
            "exit" => {
                eprintln!("boom: the fake engine refuses to start");
                std::process::exit(3);
            }
            "silent" => std::thread::sleep(Duration::from_secs(120)),
            other => panic!("unknown fake engine mode {other}"),
        }
    }

    fn fake_command(mode: &str, port: Option<u16>) -> EngineCommand {
        let (set, remove) = engine_environment(TOKEN, port, &[]);
        let mut set = set;
        set.push(("KNIGHTCODE_FAKE_ENGINE".into(), mode.into()));
        EngineCommand {
            program: std::env::current_exe().unwrap(),
            args: [
                "--exact",
                "process::tests::fake_engine",
                "--nocapture",
                "--test-threads=1",
            ]
            .map(String::from)
            .to_vec(),
            set,
            remove,
        }
    }

    /// A fake engine on the wire: /health is open, /v1/accounts checks the bearer.
    fn fake_http(accept_token: bool) -> Arc<dyn HttpClient> {
        FakeHttpClient::create(move |request| async move {
            let status = match request.uri().path() {
                "/health" => 200,
                "/v1/accounts" => {
                    let bearer = request
                        .headers()
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    if accept_token && bearer == format!("Bearer {TOKEN}") {
                        200
                    } else {
                        401
                    }
                }
                _ => 404,
            };
            Ok(Response::builder()
                .status(status)
                .body(AsyncBody::from("{}"))
                .unwrap())
        })
    }

    #[test]
    fn a_port_line_is_recognised_among_other_output() {
        assert_eq!(
            parse_port_line(r#"{"type":"listening","port":54376}"#),
            Some(54376)
        );
        assert_eq!(
            parse_port_line(
                r#"test process::tests::fake_engine ... {"type":"listening","port":7}"#
            ),
            Some(7)
        );
        assert_eq!(parse_port_line("running 1 test"), None);
        assert_eq!(parse_port_line(r#"{"type":"other","port":1}"#), None);
        assert_eq!(parse_port_line(r#"{"type":"listening","port":"x"}"#), None);
        assert_eq!(parse_port_line(""), None);
    }

    #[test]
    fn a_missing_binary_is_reported_by_path() {
        let path = std::env::temp_dir().join("knightcode-engine-that-does-not-exist");
        let command = EngineCommand {
            program: path.clone(),
            args: vec![],
            set: vec![],
            remove: vec![],
        };
        let error =
            smol::block_on(start(command, TOKEN, fake_http(true), STALL_TIMEOUT)).unwrap_err();
        assert!(matches!(error, StartError::Spawn { .. }));
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn an_exit_before_ready_carries_the_status_and_stderr() {
        let error = smol::block_on(start(
            fake_command("exit", None),
            TOKEN,
            fake_http(true),
            STALL_TIMEOUT,
        ))
        .unwrap_err();
        let StartError::ExitedBeforeReady { status, stderr } = &error else {
            panic!("{error}");
        };
        assert_eq!(status.code(), Some(3));
        assert!(stderr.contains("boom"), "{stderr}");
        assert!(error.to_string().contains("boom"), "{error}");
    }

    #[test]
    fn a_silent_engine_stalls_and_is_killed() {
        let started = std::time::Instant::now();
        let error = smol::block_on(start(
            fake_command("silent", None),
            TOKEN,
            fake_http(true),
            Duration::from_millis(500),
        ))
        .unwrap_err();
        assert!(matches!(error, StartError::Stalled { .. }), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_rejected_token_is_its_own_error() {
        let error = smol::block_on(start(
            fake_command("listening", None),
            TOKEN,
            fake_http(false),
            STALL_TIMEOUT,
        ))
        .unwrap_err();
        assert!(
            matches!(error, StartError::TokenRejected { status: 401 }),
            "{error}"
        );
    }

    #[test]
    fn ready_after_three_phases_and_stopped_by_stdin() {
        smol::block_on(async {
            let process = start(
                fake_command("listening", Some(4545)),
                TOKEN,
                fake_http(true),
                STALL_TIMEOUT,
            )
            .await
            .unwrap();
            assert_eq!(process.port, 4545);
            let started = std::time::Instant::now();
            let outcome = process.stop().await;
            assert_eq!(outcome.status.and_then(|status| status.code()), Some(0));
            // The engine exited on EOF; the kill deadline was never reached.
            assert!(started.elapsed() < STOP_TIMEOUT);
        });
    }
}
