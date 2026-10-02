//! Short-lived SSH agent access for Git, independent of the host's shared master.

use super::*;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

// A session channel makes sshd create its agent socket. Keeping stdin open
// holds that socket for exactly one RPC, including network Git preparation.
const AGENT_SESSION_SCRIPT: &str = "echo \"$HOME\"; echo \"$SSH_AUTH_SOCK\"; exec cat >/dev/null";

impl RemoteBackend {
    pub(crate) async fn prepare_vcs_within(
        &self,
        within: Duration,
        cwd: String,
        push: bool,
    ) -> Result<Option<ServerFrame>, String> {
        // Push preparation only inspects local refs; pull preparation fetches.
        self.vcs_request_within(within, !push, |req_id, ssh_auth_sock| {
            ClientFrame::PrepareVcs {
                req_id,
                cwd,
                push,
                ssh_auth_sock,
            }
        })
        .await
    }

    pub(crate) async fn run_vcs_within(
        &self,
        within: Duration,
        cwd: String,
        plan: Box<cm_core::vcs::VcsPlan>,
    ) -> Result<Option<ServerFrame>, String> {
        self.vcs_request_within(within, true, |req_id, ssh_auth_sock| ClientFrame::RunVcs {
            req_id,
            cwd,
            plan,
            ssh_auth_sock,
        })
        .await
    }

    async fn vcs_request_within(
        &self,
        within: Duration,
        network: bool,
        make: impl FnOnce(u64, Option<String>) -> ClientFrame,
    ) -> Result<Option<ServerFrame>, String> {
        let Some(target) = self
            .attach_target
            .as_deref()
            .filter(|_| network && self.forward_agent.load(Ordering::Relaxed))
        else {
            return Ok(self.request_within(within, |id| make(id, None)).await);
        };
        let mut command = detached("ssh");
        command
            .args(agent_session_options(&self.ssh_options))
            .arg(target)
            .arg(login_shell_safe(AGENT_SESSION_SCRIPT));
        // Include connection setup in the deadline. Cancellation at any stage
        // drops and kills the session; cleanup cannot discard a received reply.
        let started = Instant::now();
        let session = tokio::time::timeout(within, AgentSession::start(command))
            .await
            .unwrap_or_else(|_| Err("Git's SSH agent connection timed out".into()));
        let mut session = session.inspect_err(|error| self.log.error(error))?;
        self.log.info("SSH agent forwarding opened for Git");
        let remaining = within.saturating_sub(started.elapsed());
        let reply = if remaining.is_zero() {
            None
        } else {
            self.request_during_agent_session(remaining, &mut session, make)
                .await
        };
        session.close().await;
        self.log.info("SSH agent forwarding closed after Git");
        Ok(reply)
    }

    async fn request_during_agent_session(
        &self,
        within: Duration,
        session: &mut AgentSession,
        make: impl FnOnce(u64, Option<String>) -> ClientFrame,
    ) -> Option<ServerFrame> {
        // The RPC still goes to the persistent daemon: confirmed plans retain
        // its instance identity, and its environment never acquires the agent.
        let socket = session.socket.clone();
        tokio::select! {
            reply = self.request_within(within, |id| make(id, Some(socket))) => reply,
            _ = session.child.wait() => {
                self.log.error("Git's SSH agent connection ended before the host replied");
                None
            }
        }
    }
}

/// Retain authentication/routing settings but override modes that would detach
/// this child, share a connection, or replace its session command. ssh's short
/// flags override even preceding -o settings, so strip them as well.
fn agent_session_options(options: &[String]) -> Vec<String> {
    let mut opts: Vec<String> = [
        "ForwardAgent=yes",
        "ControlPath=none",
        "ControlMaster=no",
        "ControlPersist=no",
        "ForkAfterAuthentication=no",
        "StdinNull=no",
        "SessionType=default",
        "RemoteCommand=none",
        "ClearAllForwardings=yes",
        "PermitLocalCommand=no",
        "Tunnel=no",
        "BatchMode=yes",
    ]
    .into_iter()
    .flat_map(|option| ["-o".into(), option.into()])
    .collect();
    let mut rest = options.iter();
    while let Some(arg) = rest.next() {
        let Some(flags) = arg.strip_prefix('-').filter(|flags| !flags.is_empty()) else {
            opts.push(arg.clone());
            continue;
        };
        let mut kept = String::from("-");
        for (index, flag) in flags.char_indices() {
            if "BbceEFIiJlmopPQSLRDOWw".contains(flag) {
                let value = &flags[index + flag.len_utf8()..];
                let separate = value.is_empty().then(|| rest.next()).flatten();
                if !"SOWwLRD".contains(flag) {
                    kept.push(flag);
                    kept.push_str(value);
                    opts.push(kept.clone());
                    if let Some(value) = separate {
                        opts.push(value.clone());
                    }
                    kept.clear();
                }
                break;
            }
            if !"AafnNMtTs".contains(flag) {
                kept.push(flag);
            }
        }
        if kept.len() > 1 {
            opts.push(kept);
        }
    }
    // The short switches also win over conflicting command-line -o options.
    opts.extend(["-S".into(), "none".into(), "-A".into(), "-T".into()]);
    opts
}

struct AgentSession {
    child: tokio::process::Child,
    // Child::wait closes child.stdin. Keep this separately while monitoring it.
    stdin: Option<tokio::process::ChildStdin>,
    socket: String,
}

impl AgentSession {
    async fn start(mut command: Command) -> Result<Self, String> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "Could not start Git's SSH agent connection".to_string())?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout.take(4096));
        let mut home = String::new();
        let mut socket = String::new();
        let read = tokio::time::timeout(MUX_CONTROL_TIMEOUT, async {
            reader.read_line(&mut home).await?;
            reader.read_line(&mut socket).await
        })
        .await;
        if !matches!(read, Ok(Ok(n)) if n > 0 && socket.ends_with('\n')) {
            return Err("SSH did not report Git's forwarded agent before the deadline".into());
        }
        let home = home.trim_end_matches(['\r', '\n']);
        let socket = socket.trim_end_matches(['\r', '\n']);
        if !Path::new(home).is_absolute() || !Path::new(socket).is_absolute() {
            return Err(
                "SSH provided no forwarded agent; check your local agent and the host's SSH forwarding policy"
                    .into(),
            );
        }
        Ok(Self {
            child,
            stdin,
            socket: cm_core::paths::collapse_home(socket, home),
        })
    }

    async fn close(&mut self) {
        drop(self.stdin.take());
        if tokio::time::timeout(Duration::from_secs(1), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(enabled: bool) -> (Arc<RemoteBackend>, mpsc::UnboundedReceiver<PendingRequest>) {
        let (backend, _, requests) = RemoteBackend::build(
            &Transport::Ssh {
                target: "example.invalid".into(),
                local_sock: PathBuf::from("/tmp/cm-git-test.sock"),
                options: vec![],
                forwards: vec![],
                clipboard: false,
                forward_agent: enabled,
            },
            HostId("git-test".into()),
        );
        (backend, requests)
    }

    fn agent_command(socket: &str) -> Command {
        let mut command = Command::new("sh");
        command
            .args(["-c", AGENT_SESSION_SCRIPT])
            .env("HOME", "/path/to/repo")
            .env("SSH_AUTH_SOCK", socket);
        command
    }

    #[tokio::test]
    async fn preparation_only_requests_an_agent_for_network_git() {
        let (backend, mut requests) = backend(true);
        for enabled in [true, false] {
            backend.set_git_agent_forwarding(enabled);
            // Push preparation never invokes ssh, even when forwarding is on.
            let (reply, ()) = tokio::join!(
                backend.prepare_vcs_within(Duration::from_secs(1), "~/project".into(), true),
                async {
                    let request = requests.recv().await.unwrap();
                    assert!(matches!(
                        request.frame,
                        ClientFrame::PrepareVcs {
                            push: true,
                            ssh_auth_sock: None,
                            ..
                        }
                    ));
                    request
                        .reply
                        .send(ServerFrame::VcsPrepared {
                            req_id: request.req_id,
                            plan: None,
                            error: Some("test reply".into()),
                        })
                        .unwrap();
                }
            );
            assert!(matches!(reply, Ok(Some(ServerFrame::VcsPrepared { .. }))));
        }
        // Disabling the per-host option also bypasses ssh for pull preparation.
        let (reply, ()) = tokio::join!(
            backend.prepare_vcs_within(Duration::from_secs(1), "~/project".into(), false),
            async {
                let request = requests.recv().await.unwrap();
                assert!(matches!(
                    request.frame,
                    ClientFrame::PrepareVcs {
                        push: false,
                        ssh_auth_sock: None,
                        ..
                    }
                ));
                request
                    .reply
                    .send(ServerFrame::VcsPrepared {
                        req_id: request.req_id,
                        plan: None,
                        error: Some("test reply".into()),
                    })
                    .unwrap();
            }
        );
        assert!(reply.unwrap().is_some());
    }

    #[tokio::test]
    async fn git_requests_keep_their_own_live_agent_until_the_reply() {
        let (backend, mut requests) = backend(true);
        for socket in ["/path/to/repo/agent-first.sock", "/tmp/agent-second.sock"] {
            let mut session = AgentSession::start(agent_command(socket)).await.unwrap();
            let expected = cm_core::paths::collapse_home(socket, "/path/to/repo");
            let (reply, ()) = tokio::join!(
                backend.request_during_agent_session(
                    Duration::from_secs(1),
                    &mut session,
                    |req_id, ssh_auth_sock| ClientFrame::PrepareVcs {
                        req_id,
                        cwd: "~/project".into(),
                        push: false,
                        ssh_auth_sock,
                    },
                ),
                async {
                    let request = requests.recv().await.unwrap();
                    let ClientFrame::PrepareVcs { ssh_auth_sock, .. } = request.frame else {
                        panic!("expected Git preparation");
                    };
                    assert_eq!(ssh_auth_sock, Some(expected));
                    // Polling Child::wait must leave the session's stdin open.
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    assert!(!request.reply.is_closed());
                    request
                        .reply
                        .send(ServerFrame::VcsPrepared {
                            req_id: request.req_id,
                            plan: None,
                            error: Some("test reply".into()),
                        })
                        .unwrap();
                }
            );
            assert!(reply.is_some());
            assert!(session.child.try_wait().unwrap().is_none());
            session.close().await;
            assert!(session.child.try_wait().unwrap().unwrap().success());
        }
    }

    #[tokio::test]
    async fn timeout_and_connection_loss_release_the_agent_session() {
        let (backend, mut requests) = backend(true);
        for disconnect in [false, true] {
            let mut session = AgentSession::start(agent_command("/tmp/agent.sock"))
                .await
                .unwrap();
            let (reply, request) = tokio::join!(
                backend.request_during_agent_session(
                    Duration::from_millis(50),
                    &mut session,
                    |req_id, ssh_auth_sock| ClientFrame::PrepareVcs {
                        req_id,
                        cwd: "~/project".into(),
                        push: false,
                        ssh_auth_sock,
                    },
                ),
                async {
                    let request = requests.recv().await.unwrap();
                    if disconnect {
                        drop(request);
                        None
                    } else {
                        Some(request)
                    }
                }
            );
            assert!(reply.is_none());
            if let Some(request) = request {
                assert!(request.reply.is_closed());
            }
            session.close().await;
            assert!(session.child.try_wait().unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn missing_agent_refuses_to_start_git() {
        assert!(AgentSession::start(agent_command("")).await.is_err());
    }

    #[tokio::test]
    async fn cancelling_a_git_request_kills_its_agent_connection() {
        let (backend, mut requests) = backend(true);
        let (pid_tx, pid_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut session = AgentSession::start(agent_command("/tmp/agent.sock"))
                .await
                .unwrap();
            pid_tx.send(session.child.id().unwrap()).unwrap();
            backend
                .request_during_agent_session(
                    Duration::from_secs(30),
                    &mut session,
                    |req_id, ssh_auth_sock| ClientFrame::PrepareVcs {
                        req_id,
                        cwd: "~/project".into(),
                        push: false,
                        ssh_auth_sock,
                    },
                )
                .await
        });
        let pid = pid_rx.await.unwrap();
        let request = requests.recv().await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(request.reply.is_closed());
        tokio::time::timeout(Duration::from_secs(2), async {
            while unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn ssh_configuration_keeps_git_connections_fresh_and_other_connections_unforwarded() {
        // Exercise OpenSSH's actual precedence, including grouped short flags
        // and arguments that look like flags. No network connection is made.
        let options: Vec<String> = [
            "-vCAfMnNt",
            "-S",
            "/tmp/shared.sock",
            "-oControlPersist=600",
            "-oForwardAgent=no",
            "-o",
            "ForkAfterAuthentication=yes",
            "-o",
            "RemoteCommand=unwanted-command",
            "-oStdinNull=yes",
            "-oSessionType=none",
            "-oLocalForward=3000 localhost:3000",
            "-i",
            "-f",
            "-p2222",
            "-J",
            "jump.example.invalid",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let config = |opts: Vec<String>| {
            let output = std::process::Command::new("ssh")
                .args(["-G", "-F", "/dev/null"])
                .args(opts)
                .arg("example.invalid")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        let fresh_options = agent_session_options(&options);
        assert!(fresh_options.windows(2).any(|pair| pair == ["-i", "-f"]));
        assert!(fresh_options.contains(&"-vC".into()));
        let fresh = config(fresh_options);
        for expected in [
            "forwardagent yes",
            "controlmaster false",
            "controlpersist no",
            "forkafterauthentication no",
            "stdinnull no",
            "sessiontype default",
            "requesttty false",
            "clearallforwardings yes",
            "port 2222",
            "proxyjump jump.example.invalid",
        ] {
            assert!(
                fresh.lines().any(|line| line == expected),
                "missing {expected}"
            );
        }
        assert!(!fresh.lines().any(|line| line.starts_with("controlpath ")));
        assert!(!fresh.lines().any(|line| line.starts_with("remotecommand ")));
        assert!(!fresh.lines().any(|line| line.starts_with("localforward ")));
        let normal = config(ssh_common_opts(
            Path::new("/tmp/cm-control.sock"),
            &["-A".into(), "-oForwardAgent=yes".into()],
        ));
        assert!(normal.lines().any(|line| line == "forwardagent no"));
    }
}
