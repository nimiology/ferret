use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    sock: PathBuf,
    root: PathBuf,
    _dir: tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("root");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/foo.txt"), b"hello").unwrap();
    std::fs::create_dir(root.join("foodir")).unwrap();
    let sock = dir.path().join("d.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_ferretd"))
        .arg("--root")
        .arg(&root)
        .arg("--snapshot")
        .arg(dir.path().join("index.bin"))
        .arg("--socket")
        .arg(&sock)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let d = Daemon { child, sock, root, _dir: dir };
    let t = Instant::now();
    loop {
        if UnixStream::connect(&d.sock).is_ok()
            && rpc(&d.sock, r#"{"v":1,"id":0,"op":"status"}"#)["status"]["indexing"] == false
        {
            return d;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "daemon did not come up");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn rpc(sock: &Path, line: &str) -> serde_json::Value {
    let mut s = UnixStream::connect(sock).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(format!("{line}\n").as_bytes()).unwrap();
    let mut buf = String::new();
    BufReader::new(s).read_line(&mut buf).unwrap();
    serde_json::from_str(&buf).unwrap()
}

fn paths(v: &serde_json::Value) -> Vec<String> {
    v["hits"].as_array().unwrap().iter().map(|h| h["path"].as_str().unwrap().to_owned()).collect()
}

#[test]
fn search_status_and_errors() {
    let d = start();
    let st = rpc(&d.sock, r#"{"v":1,"id":5,"op":"status"}"#);
    assert_eq!(st["id"], 5);
    assert_eq!(st["status"]["files"], 4); // root, sub, foo.txt, foodir

    let r = rpc(&d.sock, r#"{"v":1,"id":6,"op":"search","q":"foo"}"#);
    assert_eq!((r["id"].as_u64(), r["v"].as_u64()), (Some(6), Some(1)));
    assert!(r.get("error").is_none());
    assert_eq!(r["total"], 2);

    let r = rpc(&d.sock, r#"{"v":1,"id":7,"op":"search","q":"foo","kind":"file"}"#);
    assert_eq!(paths(&r), vec![format!("{}/sub/foo.txt", d.root.display())]);
    assert_eq!(r["hits"][0]["size"], 5);
    let r = rpc(&d.sock, r#"{"v":1,"id":8,"op":"search","q":"foo type:dir","kind":"file"}"#);
    assert_eq!(r["total"], 0);

    let r = rpc(&d.sock, r#"{"v":1,"id":9,"op":"search","q":"\"unclosed"}"#);
    assert_eq!(r["id"], 9);
    assert!(r["error"].as_str().unwrap().contains("quote"));

    let r = rpc(&d.sock, r#"{"v":1,"id":10,"op":"nope"}"#);
    assert_eq!(r["id"], 10);
    assert!(r["error"].is_string());
    let r = rpc(&d.sock, "not json");
    assert!(r["error"].is_string());

    // several requests on one connection
    let mut s = UnixStream::connect(&d.sock).unwrap();
    s.write_all(b"{\"v\":1,\"id\":1,\"op\":\"status\"}\n{\"v\":1,\"id\":2,\"op\":\"status\"}\n").unwrap();
    let mut lines = BufReader::new(s).lines();
    let ids: Vec<u64> = (0..2)
        .map(|_| serde_json::from_str::<serde_json::Value>(&lines.next().unwrap().unwrap()).unwrap()["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![1, 2]);

    let mode = std::fs::metadata(&d.sock).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions())).unwrap();
    assert_eq!(mode & 0o777, 0o600);
}

fn wait_for(d: &Daemon, q: &str, want: usize) {
    let t = Instant::now();
    loop {
        let r = rpc(&d.sock, &format!(r#"{{"v":1,"id":1,"op":"search","q":"{q}"}}"#));
        if r["total"] == want {
            return;
        }
        assert!(t.elapsed() < Duration::from_secs(3), "{q}: wanted {want}, got {r}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn fsevents_live_updates() {
    let d = start();
    std::fs::write(d.root.join("sub/zebra.txt"), b"").unwrap();
    wait_for(&d, "zebra", 1);
    std::fs::rename(d.root.join("sub/zebra.txt"), d.root.join("giraffe.txt")).unwrap();
    wait_for(&d, "giraffe", 1);
    wait_for(&d, "zebra", 0);
    std::fs::create_dir_all(d.root.join("new/deep")).unwrap();
    std::fs::write(d.root.join("new/deep/okapi.md"), b"").unwrap();
    wait_for(&d, "okapi", 1);
    std::fs::remove_dir_all(d.root.join("new")).unwrap();
    wait_for(&d, "okapi", 0);
}
