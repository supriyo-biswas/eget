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
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "");

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
