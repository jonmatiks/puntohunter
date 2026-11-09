//! Runs the `puntohunter` binary as a process and checks what it serves.

use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    time::{Duration, Instant},
};

const BIN: &str = env!("CARGO_BIN_EXE_puntohunter");

/// Kills the child when the test ends, pass or fail.
struct Pool(Child);

impl Drop for Pool {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write_config(name: &str, body: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::write(&path, body).unwrap();
    path
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn scrape(port: u16) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(
        stream,
        "GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    Some(response)
}

fn run_to_exit(config: &PathBuf) -> Output {
    Command::new(BIN)
        .args(["serve", "--config"])
        .arg(config)
        .output()
        .unwrap()
}

/// config/regtest.toml with the metrics endpoint on `port`, the other listeners on free ports, and a fresh data
/// directory under the target directory, so no database left by an older binary is opened.
fn regtest_config(port: u16) -> String {
    regtest_config_in(port, "serve-data")
}

/// regtest_config with its data directory `name`, of its own, so tests running together keep theirs.
fn regtest_config_in(port: u16, name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/regtest.toml");
    let mut config: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
    let mut bind = |section: &str, key: &str, port: u16| {
        config[section]
            .as_table_mut()
            .unwrap()
            .insert(key.into(), format!("127.0.0.1:{port}").into());
    };
    bind("metrics", "bind", port);
    bind("portal", "bind", free_port());
    for pool in config["pools"].as_array_mut().unwrap() {
        let pool = pool.as_table_mut().unwrap();
        for protocol in ["v1", "v2"] {
            pool.insert(protocol.into(), format!("127.0.0.1:{}", free_port()).into());
        }
    }
    let data_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&data_dir);
    config.insert(
        "data_dir".into(),
        data_dir.to_string_lossy().into_owned().into(),
    );
    config.to_string()
}

#[test]
fn serves_ready_with_regtest_config() {
    let port = free_port();
    let config = write_config("regtest.toml", &regtest_config(port));
    let _pool = Pool(
        Command::new(BIN)
            .args(["serve", "--config"])
            .arg(&config)
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if scrape(port).is_some_and(|body| body.lines().any(|l| l == "puntohunter_ready 1")) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "puntohunter_ready 1 not served on port {port}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn missing_config_names_its_path() {
    let config = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("does-not-exist.toml");
    let out = run_to_exit(&config);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(&*config.to_string_lossy()));
}

#[test]
fn unparseable_config_names_its_path() {
    let config = write_config("broken.toml", "[metrics\nbind = 1\n");
    let out = run_to_exit(&config);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(&*config.to_string_lossy()));
}

#[test]
fn a_portal_site_it_cannot_use_names_its_path() {
    let site = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("no-description-site.json");
    for (name, exists) in [("missing", false), ("unusable", true)] {
        let _ = std::fs::remove_file(&site);
        if exists {
            std::fs::write(&site, r#"{"name": "A pool"}"#).unwrap();
        }
        let mut config: toml::Table = regtest_config_in(free_port(), "site-data").parse().unwrap();
        config["portal"]
            .as_table_mut()
            .unwrap()
            .insert("site".into(), site.to_string_lossy().into_owned().into());
        let config = write_config(&format!("site-{name}.toml"), &config.to_string());
        let out = run_to_exit(&config);
        assert!(!out.status.success(), "{name}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&*site.to_string_lossy()),
            "{name}: {stderr}"
        );
    }
}
