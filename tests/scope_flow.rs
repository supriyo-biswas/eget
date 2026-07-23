use eget::db::Database;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

fn server(body: &'static [u8]) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).unwrap();
            let path = String::from_utf8_lossy(&request[..count])
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_owned();
            let (status, response): (&str, &[u8]) = if path == "/api/v1/version" {
                ("404 Not Found", b"")
            } else {
                ("200 OK", body)
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .unwrap();
            stream.write_all(response).unwrap();
        }
    });
    (format!("http://{address}/tool"), handle)
}

#[test]
fn local_install_and_uninstall_report_scope_and_use_project_state() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let marker = project.join("eget-packages.txt");
    std::fs::write(&marker, "").unwrap();
    let (url, server) = server(b"#!/bin/sh\nexit 0\n");

    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_eget"));
        command
            .arg("--scope=local")
            .current_dir(&project)
            .env("HOME", &home)
            .env_remove("EGET_SCOPE")
            .env_remove("EGET_BIN_DIR")
            .env_remove("EGET_BIN");
        command
    };
    let install = command().arg(&url).output().unwrap();
    assert!(install.status.success(), "{:?}", install.stderr);
    server.join().unwrap();

    let state = project.join(".eget");
    let database = Database::open(&state.join("eget.sqlite3"), &state).unwrap();
    let package = database.packages().unwrap().pop().unwrap();
    assert_eq!(
        String::from_utf8(install.stdout).unwrap(),
        format!("Installed {} in local scope (~/project)\n", package.id)
    );
    assert!(state.join("bin/tool").is_symlink());
    assert!(package.pinned);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        format!("{url}\n")
    );

    let mark = command()
        .args(["mark", "--no-pin", package.id.as_str()])
        .output()
        .unwrap();
    assert!(mark.status.success(), "{:?}", mark.stderr);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        format!("{url} --no-pin\n")
    );

    let uninstall = command()
        .args(["uninstall", package.id.as_str()])
        .output()
        .unwrap();
    assert!(uninstall.status.success(), "{:?}", uninstall.stderr);
    assert_eq!(
        String::from_utf8(uninstall.stdout).unwrap(),
        format!("Uninstalled {} in local scope (~/project)\n", package.id)
    );
    assert_eq!(std::fs::read_to_string(marker).unwrap(), "");
}

#[test]
fn bare_local_install_reads_the_manifest_without_rewriting_it() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let (url, server) = server(b"#!/bin/sh\nexit 0\n");
    let contents = format!("# project tools\n{url} # keep this comment\n");
    std::fs::write(project.join("eget-packages.txt"), &contents).unwrap();

    let install = Command::new(env!("CARGO_BIN_EXE_eget"))
        .arg("install")
        .current_dir(&project)
        .env("HOME", &home)
        .env_remove("EGET_SCOPE")
        .env_remove("EGET_BIN_DIR")
        .env_remove("EGET_BIN")
        .output()
        .unwrap();
    assert!(install.status.success(), "{:?}", install.stderr);
    server.join().unwrap();

    assert_eq!(
        std::fs::read_to_string(project.join("eget-packages.txt")).unwrap(),
        contents
    );
    assert!(project.join(".eget/bin/tool").is_symlink());

    let repeat = Command::new(env!("CARGO_BIN_EXE_eget"))
        .args(["install", "--ignore-existing"])
        .current_dir(&project)
        .env("HOME", &home)
        .env_remove("EGET_SCOPE")
        .output()
        .unwrap();
    assert!(repeat.status.success(), "{:?}", repeat.stderr);
    assert!(
        String::from_utf8(repeat.stdout)
            .unwrap()
            .contains("Skipped")
    );
    assert_eq!(
        std::fs::read_to_string(project.join("eget-packages.txt")).unwrap(),
        contents
    );
}

#[test]
fn bare_install_requires_local_scope_and_package_flags_belong_in_the_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let outside_local = Command::new(env!("CARGO_BIN_EXE_eget"))
        .args(["--scope=user", "install"])
        .current_dir(&home)
        .env("HOME", &home)
        .env_remove("EGET_SCOPE")
        .output()
        .unwrap();
    assert!(!outside_local.status.success());
    assert!(
        String::from_utf8(outside_local.stderr)
            .unwrap()
            .contains("install requires at least one package outside local scope")
    );

    let project = home.join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("eget-packages.txt"), "").unwrap();
    let empty = Command::new(env!("CARGO_BIN_EXE_eget"))
        .arg("install")
        .current_dir(&project)
        .env("HOME", &home)
        .env_remove("EGET_SCOPE")
        .output()
        .unwrap();
    assert!(empty.status.success(), "{:?}", empty.stderr);
    assert!(!project.join(".eget").exists());

    let durable_flag = Command::new(env!("CARGO_BIN_EXE_eget"))
        .args(["install", "--no-pin"])
        .current_dir(&project)
        .env("HOME", &home)
        .env_remove("EGET_SCOPE")
        .output()
        .unwrap();
    assert!(!durable_flag.status.success());
    assert!(
        String::from_utf8(durable_flag.stderr)
            .unwrap()
            .contains("package-specific install options require an explicit package")
    );
}

#[test]
fn invalid_manifest_fails_before_network_access() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/tool", listener.local_addr().unwrap());
    std::fs::write(
        project.join("eget-packages.txt"),
        format!("{url} --force\n"),
    )
    .unwrap();

    let install = Command::new(env!("CARGO_BIN_EXE_eget"))
        .arg("install")
        .current_dir(&project)
        .env("HOME", &home)
        .env_remove("EGET_SCOPE")
        .output()
        .unwrap();
    assert!(!install.status.success());
    assert!(
        String::from_utf8(install.stderr)
            .unwrap()
            .contains("run-only option")
    );
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn explicit_local_install_records_successes_from_a_partially_failing_batch() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = home.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let marker = project.join("eget-packages.txt");
    std::fs::write(&marker, "").unwrap();
    let (url, server) = server(b"#!/bin/sh\nexit 0\n");

    let install = Command::new(env!("CARGO_BIN_EXE_eget"))
        .args(["install", "not a locref", &url])
        .current_dir(&project)
        .env("HOME", &home)
        .env_remove("EGET_SCOPE")
        .output()
        .unwrap();
    assert_eq!(install.status.code(), Some(1));
    server.join().unwrap();
    assert_eq!(std::fs::read_to_string(marker).unwrap(), format!("{url}\n"));
}
