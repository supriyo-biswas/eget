use eget::db::Database;
use eget::installer::{InstallOptions, Installer};
use eget::scope::Scope;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::symlink;
use std::thread;

fn server(body: &'static [u8], requests: usize) -> (String, thread::JoinHandle<Vec<String>>) {
    server_with_headers(body, b"", requests)
}

fn server_with_headers(
    body: &'static [u8],
    headers: &'static [u8],
    requests: usize,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut paths = Vec::new();
        while paths.len() < requests {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).unwrap();
            let path = String::from_utf8_lossy(&request[..count])
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_owned();
            paths.push(path.clone());
            let (status, response): (&str, &[u8]) = if path == "/api/v1/version" {
                ("404 Not Found", b"")
            } else {
                ("200 OK", body)
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                String::from_utf8_lossy(headers),
                response.len()
            )
            .unwrap();
            stream.write_all(response).unwrap();
        }
        paths
    });
    (format!("http://{address}/tool"), handle)
}

fn scope(temp: &tempfile::TempDir) -> Scope {
    Scope::from_paths(
        temp.path().join("packages"),
        temp.path().join("state"),
        temp.path().join("bin"),
    )
}

fn host_kernel() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        other => panic!("unsupported test operating system {other}"),
    }
}

fn release_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => panic!("unsupported test architecture {other}"),
    }
}

#[test]
fn direct_package_round_trip_uses_new_schema_and_managed_symlink() {
    let temp = tempfile::tempdir().unwrap();
    let scope = scope(&temp);
    let installer = Installer::new(scope.clone()).unwrap();
    let (url, server) = server(b"#!/bin/sh\necho installed\n", 2);

    assert_eq!(
        installer
            .install_many(&[url], &InstallOptions::default())
            .unwrap(),
        0
    );
    assert_eq!(server.join().unwrap(), ["/api/v1/version", "/tool"]);

    let database = Database::open(&scope.database, &scope.package_root).unwrap();
    let packages = database.packages().unwrap();
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0].current_version, None);
    assert_eq!(packages[0].source_kind.as_str(), "direct");
    assert!(packages[0].pinned);
    assert_eq!(packages[0].binaries, ["tool"]);
    assert_eq!(
        fs::read_to_string(scope.bin_dir.join("tool")).unwrap(),
        "#!/bin/sh\necho installed\n"
    );

    assert_eq!(
        installer
            .uninstall_many(&[packages[0].id.to_string()])
            .unwrap(),
        0
    );
    assert!(fs::symlink_metadata(scope.bin_dir.join("tool")).is_err());
    assert!(database.packages().unwrap().is_empty());
}

#[test]
fn direct_package_install_handles_symlinked_scope_root() {
    let temp = tempfile::tempdir().unwrap();
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("scope");
    symlink(temp.path(), &alias).unwrap();
    let scope = Scope::from_paths(
        alias.join("packages"),
        alias.join("state"),
        alias.join("bin"),
    );
    let installer = Installer::new(scope.clone()).unwrap();
    let (url, server) = server(b"#!/bin/sh\necho installed\n", 2);

    assert_eq!(
        installer
            .install_many(&[url], &InstallOptions::default())
            .unwrap(),
        0
    );
    server.join().unwrap();
    assert_eq!(
        fs::read_to_string(scope.bin_dir.join("tool")).unwrap(),
        "#!/bin/sh\necho installed\n"
    );
}

#[test]
fn direct_package_with_an_etag_tracks_updates() {
    let temp = tempfile::tempdir().unwrap();
    let scope = scope(&temp);
    let installer = Installer::new(scope.clone()).unwrap();
    let (url, server) = server_with_headers(b"#!/bin/sh\nexit 0\n", b"ETag: asset-one\r\n", 2);

    assert_eq!(
        installer
            .install_many(&[url], &InstallOptions::default())
            .unwrap(),
        0
    );
    server.join().unwrap();

    let package = Database::open(&scope.database, &scope.package_root)
        .unwrap()
        .packages()
        .unwrap()
        .pop()
        .unwrap();
    assert!(!package.pinned);
    assert_eq!(package.validators.etag.as_deref(), Some("asset-one"));
}

#[test]
fn versioned_direct_package_with_an_etag_is_automatically_pinned() {
    let temp = tempfile::tempdir().unwrap();
    let scope = scope(&temp);
    let installer = Installer::new(scope.clone()).unwrap();
    let (url, server) = server_with_headers(b"#!/bin/sh\nexit 0\n", b"ETag: asset-one\r\n", 2);
    let url = url.replace("/tool", "/tool-v1.2.3");

    assert_eq!(
        installer
            .install_many(std::slice::from_ref(&url), &InstallOptions::default())
            .unwrap(),
        0
    );
    server.join().unwrap();

    let package = Database::open(&scope.database, &scope.package_root)
        .unwrap()
        .packages()
        .unwrap()
        .pop()
        .unwrap();
    assert!(package.pinned);
    assert_eq!(package.current_version, None);
    assert_eq!(package.validators.etag.as_deref(), Some("asset-one"));
}

#[test]
fn install_time_unpin_overrides_a_versioned_url_when_an_etag_exists() {
    let temp = tempfile::tempdir().unwrap();
    let scope = scope(&temp);
    let installer = Installer::new(scope.clone()).unwrap();
    let (url, server) = server_with_headers(b"#!/bin/sh\nexit 0\n", b"ETag: asset-one\r\n", 2);
    let url = url.replace("/tool", "/tool-v1.2.3");

    assert_eq!(
        installer
            .install_many(
                &[url],
                &InstallOptions {
                    pin: Some(false),
                    ..InstallOptions::default()
                }
            )
            .unwrap(),
        0
    );
    server.join().unwrap();

    let package = Database::open(&scope.database, &scope.package_root)
        .unwrap()
        .packages()
        .unwrap()
        .pop()
        .unwrap();
    assert!(!package.pinned);
    assert_eq!(package.current_version, None);
}

#[test]
fn direct_package_without_validators_ignores_install_time_unpin() {
    let temp = tempfile::tempdir().unwrap();
    let scope = scope(&temp);
    let installer = Installer::new(scope.clone()).unwrap();
    let (url, server) = server(b"#!/bin/sh\nexit 0\n", 2);
    let url = url.replace("/tool", "/tool-v1.2.3");

    assert_eq!(
        installer
            .install_many(
                &[url],
                &InstallOptions {
                    pin: Some(false),
                    ..InstallOptions::default()
                }
            )
            .unwrap(),
        0
    );
    server.join().unwrap();

    assert!(
        Database::open(&scope.database, &scope.package_root)
            .unwrap()
            .packages()
            .unwrap()
            .pop()
            .unwrap()
            .pinned
    );
}

#[test]
fn reinstall_repins_a_manually_unpinned_validatorless_direct_package() {
    let temp = tempfile::tempdir().unwrap();
    let scope = scope(&temp);
    let installer = Installer::new(scope.clone()).unwrap();
    let (url, server) = server(b"#!/bin/sh\nexit 0\n", 3);

    assert_eq!(
        installer
            .install_many(std::slice::from_ref(&url), &InstallOptions::default())
            .unwrap(),
        0
    );
    let id = Database::open(&scope.database, &scope.package_root)
        .unwrap()
        .packages()
        .unwrap()
        .pop()
        .unwrap()
        .id
        .to_string();
    assert_eq!(
        installer
            .mark_many(std::slice::from_ref(&id), Some(false), None)
            .unwrap(),
        0
    );
    assert!(
        !Database::open(&scope.database, &scope.package_root)
            .unwrap()
            .package(&id)
            .unwrap()
            .unwrap()
            .pinned
    );
    assert_eq!(
        installer
            .update_many(std::slice::from_ref(&id), |_| Ok(false))
            .unwrap(),
        0
    );
    assert_eq!(
        installer
            .install_many(
                &[url],
                &InstallOptions {
                    reinstall: true,
                    ..InstallOptions::default()
                }
            )
            .unwrap(),
        0
    );
    server.join().unwrap();
    assert!(
        Database::open(&scope.database, &scope.package_root)
            .unwrap()
            .package(&id)
            .unwrap()
            .unwrap()
            .pinned
    );
}

#[test]
fn uninstall_commit_failure_restores_package_and_links() {
    let temp = tempfile::tempdir().unwrap();
    let scope = scope(&temp);
    let installer = Installer::new(scope.clone()).unwrap();
    let (url, server) = server(b"#!/bin/sh\necho installed\n", 2);

    assert_eq!(
        installer
            .install_many(&[url], &InstallOptions::default())
            .unwrap(),
        0
    );
    server.join().unwrap();

    let database = Database::open(&scope.database, &scope.package_root).unwrap();
    let package = database.packages().unwrap().pop().unwrap();
    let id = package.id.to_string();
    let link = scope.bin_dir.join("tool");
    assert!(package.installation_dir.is_dir());
    assert!(link.is_symlink());
    drop(database);

    let guard = rusqlite::Connection::open(&scope.database).unwrap();
    guard
        .execute_batch(
            "PRAGMA foreign_keys=ON;
         CREATE TABLE uninstall_guard (
             package_id TEXT REFERENCES packages(id) DEFERRABLE INITIALLY DEFERRED
         ) STRICT;",
        )
        .unwrap();
    guard
        .execute(
            "INSERT INTO uninstall_guard(package_id) VALUES(?1)",
            [id.as_str()],
        )
        .unwrap();
    drop(guard);

    assert_eq!(
        installer.uninstall_many(std::slice::from_ref(&id)).unwrap(),
        1
    );

    let database = Database::open(&scope.database, &scope.package_root).unwrap();
    assert!(database.package(&id).unwrap().is_some());
    assert!(package.installation_dir.is_dir());
    assert!(link.is_symlink());
    assert_eq!(
        fs::read_to_string(link).unwrap(),
        "#!/bin/sh\necho installed\n"
    );
}

#[test]
fn operation_batch_continues_after_an_individual_failure() {
    let temp = tempfile::tempdir().unwrap();
    let scope = scope(&temp);
    let installer = Installer::new(scope.clone()).unwrap();
    let (url, server) = server(b"#!/bin/sh\nexit 0\n", 2);
    let result = installer
        .install_many(&["not a locref".into(), url], &InstallOptions::default())
        .unwrap();
    assert_eq!(result, 1);
    server.join().unwrap();
    assert_eq!(
        Database::open(&scope.database, &scope.package_root)
            .unwrap()
            .packages()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn version_url_requires_a_template_before_making_a_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let installer = Installer::new(scope(&temp)).unwrap();
    let options = InstallOptions {
        version_url: Some(format!("http://{address}/version")),
        ..InstallOptions::default()
    };

    assert_eq!(
        installer
            .install_many(&[format!("http://{address}/tool-v1")], &options)
            .unwrap(),
        1
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn invalid_url_template_fails_before_making_a_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let installer = Installer::new(scope(&temp)).unwrap();

    assert_eq!(
        installer
            .install_many(
                &[format!("http://{address}/tool-{{{{ '' }}}}")],
                &InstallOptions::default()
            )
            .unwrap(),
        1
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn host_template_is_preserved_and_rendered_for_validator_updates() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let asset_path = format!("/tool-{}-{}", host_kernel(), release_arch());
    let expected_path = asset_path.clone();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut downloads = 0;
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            let mut request_line = request.lines().next().unwrap().split_whitespace();
            let method = request_line.next().unwrap().to_owned();
            let path = request_line.next().unwrap().to_owned();
            requests.push((method.clone(), path.clone()));
            let (status, headers, body): (&str, &[u8], &[u8]) = if path == "/api/v1/version" {
                ("404 Not Found", b"", b"")
            } else if path == expected_path && method == "HEAD" {
                ("200 OK", b"ETag: asset-two\r\n", b"")
            } else if path == expected_path && method == "GET" {
                downloads += 1;
                if downloads == 1 {
                    ("200 OK", b"ETag: asset-one\r\n", b"#!/bin/sh\necho one\n")
                } else {
                    ("200 OK", b"ETag: asset-two\r\n", b"#!/bin/sh\necho two\n")
                }
            } else {
                panic!("unexpected request {method} {path}")
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                String::from_utf8_lossy(headers),
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
        requests
    });

    let temp = tempfile::tempdir().unwrap();
    let scope = scope(&temp);
    let installer = Installer::new(scope.clone()).unwrap();
    let template = format!(
        "{base}/tool-{{{{kernel}}}}-{{% if arch == 'x86_64' %}}amd64{{% else %}}arm64{{% endif %}}"
    );
    assert_eq!(
        installer
            .install_many(std::slice::from_ref(&template), &InstallOptions::default())
            .unwrap(),
        0
    );
    let package = Database::open(&scope.database, &scope.package_root)
        .unwrap()
        .packages()
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(package.installed_asset_url, template);
    assert!(!package.pinned);

    assert_eq!(
        installer
            .update_many(&[package.id.to_string()], |_| Ok(true))
            .unwrap(),
        0
    );
    assert_eq!(
        fs::read_to_string(scope.bin_dir.join("tool")).unwrap(),
        "#!/bin/sh\necho two\n"
    );
    assert_eq!(
        Database::open(&scope.database, &scope.package_root)
            .unwrap()
            .package(package.id.as_str())
            .unwrap()
            .unwrap()
            .installed_asset_url,
        template
    );
    assert_eq!(
        handle.join().unwrap(),
        [
            ("GET".into(), "/api/v1/version".into()),
            ("GET".into(), asset_path.clone()),
            ("HEAD".into(), asset_path.clone()),
            ("GET".into(), asset_path),
        ]
    );
}

#[test]
fn version_url_persists_nullable_schema_tracking_fields() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let handle = thread::spawn(move || {
        let mut paths = Vec::new();
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 4096];
            let count = stream.read(&mut request).unwrap();
            let path = String::from_utf8_lossy(&request[..count])
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_owned();
            paths.push(path.clone());
            let (status, headers, body): (&str, &[u8], &[u8]) = match path.as_str() {
                "/version" => (
                    "200 OK",
                    b"Content-Type: application/json; charset=utf-8\r\n",
                    br#"{"version":"v1.2.3"}"#,
                ),
                "/api/v1/version" => ("404 Not Found", b"", b""),
                "/tool-v1.2.3" => ("200 OK", b"ETag: asset-one\r\n", b"#!/bin/sh\nexit 0\n"),
                _ => panic!("unexpected path {path}"),
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                String::from_utf8_lossy(headers),
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
        paths
    });

    let temp = tempfile::tempdir().unwrap();
    let scope = scope(&temp);
    let installer = Installer::new(scope.clone()).unwrap();
    let template = format!("{base}/tool-{{{{version}}}}");
    let options = InstallOptions {
        version_url: Some(format!("{base}/version")),
        ..InstallOptions::default()
    };
    assert_eq!(
        installer
            .install_many(std::slice::from_ref(&template), &options)
            .unwrap(),
        0
    );
    assert_eq!(
        handle.join().unwrap(),
        ["/version", "/api/v1/version", "/tool-v1.2.3"]
    );

    let package = Database::open(&scope.database, &scope.package_root)
        .unwrap()
        .packages()
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(package.current_version.as_deref(), Some("v1.2.3"));
    assert_eq!(package.installed_asset_url, template);
    assert_eq!(
        package.version_check_url.as_deref(),
        Some(format!("{base}/version").as_str())
    );
    assert!(!package.pinned);
    assert_eq!(package.validators.etag, None);
}
