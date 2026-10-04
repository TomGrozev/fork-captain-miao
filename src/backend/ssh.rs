//! SSH connection policies, private socket directories and consistent master paths.
use super::*;

/// Fresh Git/work-tab connection, with forwarding decided by OpenSSH config.
/// Keep it independent of the non-forwarding dashboard master and any external
/// master, and end it with its owning command instead of persisting it.
/// Retain authentication/routing settings but override modes that would detach
/// this child, share a connection, or replace its session command. ssh's short
/// flags override even preceding -o settings, so strip them as well.
pub(super) fn session_options(options: &[String], tty: bool) -> Vec<String> {
    let mut opts: Vec<String> = [
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
    ]
    .into_iter()
    .flat_map(|option| ["-o".into(), option.into()])
    .collect();
    // Git cannot prompt on a detached child. Work tabs retain the existing
    // ability to override BatchMode through Advanced SSH options.
    if !tty {
        opts.extend(["-o".into(), "BatchMode=yes".into()]);
    }
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
            if !"fnNMtTs".contains(flag) {
                kept.push(flag);
            }
        }
        if kept.len() > 1 {
            opts.push(kept);
        }
    }
    // The short switches also win over conflicting command-line -o options.
    opts.extend([
        "-S".into(),
        "none".into(),
        if tty { "-t".into() } else { "-T".into() },
    ]);
    // Connection defaults remain overridable. Never supply ForwardAgent here:
    // OpenSSH owns its precedence, including -A/-a and agent socket paths.
    opts.extend([
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=10".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ]);
    opts
}

pub(super) async fn forward(
    target: &str,
    options: &[String],
    forward: &Forward,
    add: bool,
) -> Result<(), String> {
    let result = async {
        let child = detached("ssh")
            .args(options)
            .args(["-O", if add { "forward" } else { "cancel" }])
            .arg(&forward.flag)
            .arg(&forward.spec)
            .arg(target)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| error.to_string())?;
        let (status, _, stderr) = capped_output(child, REMOTE_OUTPUT_CAP)
            .await
            .map_err(|error| error.to_string())?;
        forwards::control_result(status.success(), &stderr, add)
    };
    tokio::time::timeout(MUX_CONTROL_TIMEOUT, result)
        .await
        .map_err(|_| "SSH forwarding request timed out; listener state is uncertain".to_string())?
}

/// Resolve with the dial's full configuration before discarding options that
/// could add unrelated forwards to a mux control command. ControlPath tokens
/// depend on HostName/User/Port (and potentially ProxyJump), not just the text
/// of ControlPath itself.
pub(super) async fn control_options(
    target: &str,
    options: &[String],
) -> Result<Vec<String>, String> {
    let child = detached("ssh")
        .arg("-G")
        .args(options)
        .arg(target)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Could not resolve SSH control socket: {error}"))?;
    let (status, stdout, stderr) =
        tokio::time::timeout(MUX_CONTROL_TIMEOUT, capped_output(child, REMOTE_OUTPUT_CAP))
            .await
            .map_err(|_| "Resolving SSH control socket timed out".to_string())?
            .map_err(|error| format!("Could not resolve SSH control socket: {error}"))?;
    if !status.success() {
        return Err(format!(
            "Could not resolve SSH control socket: {}",
            stderr.trim()
        ));
    }
    let path = stdout
        .lines()
        .find_map(|line| line.strip_prefix("controlpath "))
        .filter(|path| !path.is_empty() && *path != "none")
        .ok_or_else(|| {
            "SSH multiplexing is disabled; no control socket is available".to_string()
        })?;
    // The path has already been expanded. Preserve literal percent signs when
    // OpenSSH processes the concrete -S value a second time.
    Ok(vec![
        "-F".into(),
        "/dev/null".into(),
        "-S".into(),
        path.replace('%', "%%"),
    ])
}

pub(super) fn prepare_socket_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    // Check and chmod the same inode. Never follow a pre-created symlink or
    // chmod another user's directory under a shared temporary directory.
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)?;
    if directory.metadata()?.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "socket directory is owned by another user",
        ));
    }
    directory.set_permissions(std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn controls_use_the_dials_expanded_socket_without_configured_forwards() {
        let root = scratch_home("control-path-context");
        let config = root.join("config");
        std::fs::write(&config, "Host test-target\n HostName example.invalid\n Port 2222\n User test-user\n LocalForward 9000 localhost:9000\n").unwrap();
        let options = ssh_common_opts(
            &root.join("default"),
            &[
                "-F".into(),
                config.to_string_lossy().into_owned(),
                "-oControlPath=/tmp/cm-test-%h-%r-%p-%%".into(),
                "-L3000:localhost:3000".into(),
            ],
        );
        let controls = control_options("test-target", &options).await.unwrap();
        assert_eq!(
            &controls,
            &[
                "-F",
                "/dev/null",
                "-S",
                "/tmp/cm-test-example.invalid-test-user-2222-%%"
            ]
        );
        let output = detached("ssh")
            .arg("-G")
            .args(&controls)
            .arg("test-target")
            .stdout(Stdio::piped())
            .output()
            .await
            .unwrap();
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains("controlpath /tmp/cm-test-example.invalid-test-user-2222-%\n"));
        assert!(!output.contains("localforward "));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn socket_directory_rejects_symlinks_and_files() {
        let root = scratch_home("socket-dir-check");
        let actual = root.join("actual");
        std::fs::create_dir(&actual).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&actual, &link).unwrap();
        let file = root.join("file");
        std::fs::write(&file, "fixture").unwrap();
        let symlink_result = prepare_socket_dir(&link);
        let file_result = prepare_socket_dir(&file);
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            symlink_result.is_err(),
            "must not follow a pre-created symlink"
        );
        assert!(file_result.is_err(), "must not accept a regular file");
    }

    #[test]
    fn socket_directory_is_private_on_creation_and_upgrade() {
        let root = scratch_home("socket-dir-modes");
        let dir = root.join("sockets");
        prepare_socket_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        prepare_socket_dir(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
