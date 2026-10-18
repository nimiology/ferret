use super::*;
use std::fs;

fn home() -> PathBuf {
    PathBuf::from("/Users/x")
}

fn hit(p: &str) -> HitOut {
    HitOut {
        path: p.into(),
        kind: Kind::File,
        size: 1,
        mtime: 0,
    }
}

#[test]
fn default_deny_list_always_applies() {
    let p = Policy::new(&home(), None, vec![]);
    assert!(p.allows(Path::new("/Users/x/Documents/a.pdf")));
    for d in DEFAULT_DENY {
        assert!(!p.allows(&home().join(d).join("child")), "{d}");
        assert!(!p.allows(&home().join(d)), "{d}");
    }
    assert!(!p.allows(Path::new("/etc/passwd")), "outside $HOME");
    assert!(!p.allows(Path::new("relative/path")));
}

#[test]
fn prefix_is_component_wise() {
    let p = Policy::new(&home(), None, vec![]);
    assert!(p.allows(Path::new("/Users/x/.sshfoo/key")));
    assert!(!p.allows(Path::new("/Users/x/.ssh/id_ed25519")));
    assert!(
        !p.allows(Path::new("/Users/xy/file")),
        "/Users/xy is not under /Users/x"
    );
}

#[test]
fn deny_ignores_case_and_unicode_form() {
    let p = Policy::new(
        &home(),
        None,
        vec!["~/Secrets".into(), "~/Caf\u{e9}".into()],
    );
    assert!(!p.allows(Path::new("/Users/x/.SSH/id_rsa")));
    assert!(!p.allows(Path::new("/Users/x/secrets/a")));
    assert!(
        !p.allows(Path::new("/Users/x/Cafe\u{301}/menu.pdf")),
        "NFD path vs NFC deny"
    );
    assert!(p.allows(Path::new("/Users/x/Documents/a")));
}

#[test]
fn dotdot_is_resolved_lexically() {
    let p = Policy::new(&home(), None, vec![]);
    assert!(!p.allows(Path::new("/Users/x/Documents/../.ssh/id_rsa")));
    assert!(!p.allows(Path::new("/Users/x/../y/file")));
    assert!(!p.allows(Path::new("/Users/x/./.aws/credentials")));
    assert!(p.allows(Path::new("/Users/x/.ssh/../Documents/a")));
}

#[test]
fn config_parsing_tilde_and_deny_beats_allow() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("config.toml");
    fs::write(&cfg, "[mcp]\nallow = [\"~/Projects\", \"/Volumes/Work\"]\ndeny = [\"~/Projects/secret\", \"/Volumes/Work\"]\n").unwrap();
    let p = Policy::load(&home(), &cfg).unwrap();
    assert!(p.allows(Path::new("/Users/x/Projects/a.rs")));
    assert!(
        !p.allows(Path::new("/Users/x/Documents/a")),
        "allow replaced the $HOME default"
    );
    assert!(!p.allows(Path::new("/Users/x/Projects/secret/a")));
    assert!(!p.allows(Path::new("/Volumes/Work/a")), "deny beats allow");

    assert!(Policy::load(&home(), &dir.path().join("missing.toml"))
        .unwrap()
        .allows(Path::new("/Users/x/a")));
    fs::write(&cfg, "[mcp\nallow=").unwrap();
    assert!(
        Policy::load(&home(), &cfg).is_err(),
        "broken config must not silently fall back"
    );
    // Default deny still added with a custom allow list.
    fs::write(&cfg, "[mcp]\nallow = [\"~\"]\n").unwrap();
    assert!(!Policy::load(&home(), &cfg)
        .unwrap()
        .allows(Path::new("/Users/x/.gnupg/k")));
}

fn ctx_for(home: &Path) -> Ctx {
    let policy = Policy::new(home, None, vec![]);
    Ctx::new(
        policy,
        home.into(),
        home.join("none.bin"),
        home.join("log/mcp.log"),
        false,
    )
}

#[test]
fn file_info_resolves_symlinks_before_checking() {
    let dir = tempfile::tempdir().unwrap();
    let h = dir.path();
    fs::create_dir_all(h.join(".ssh")).unwrap();
    fs::write(h.join(".ssh/id_rsa"), "secret").unwrap();
    fs::create_dir_all(h.join("docs/sub")).unwrap();
    fs::write(h.join("docs/a.txt"), "hello").unwrap();
    std::os::unix::fs::symlink(h.join(".ssh"), h.join("innocent")).unwrap();
    std::os::unix::fs::symlink("/etc", h.join("etc_link")).unwrap();
    let ctx = ctx_for(h);

    let info = file_info(&ctx, h.join("docs").to_str().unwrap()).unwrap();
    assert!(
        info.contains("kind: dir") && info.contains("children: 2"),
        "{info}"
    );
    let info = file_info(&ctx, h.join("docs/a.txt").to_str().unwrap()).unwrap();
    assert!(
        info.contains("kind: file") && info.contains("(5 bytes)") && !info.contains("hello"),
        "{info}"
    );

    for bad in [
        "innocent/id_rsa",
        "innocent",
        "etc_link/hosts",
        ".ssh/id_rsa",
        "docs/../.ssh",
    ] {
        let e = file_info(&ctx, h.join(bad).to_str().unwrap()).unwrap_err();
        assert!(e.contains("not found or not allowed"), "{bad}: {e}");
    }
    assert!(file_info(&ctx, "relative").is_err());
}

#[test]
fn filtering_and_limit_cap() {
    let p = Policy::new(&home(), None, vec![]);
    let hits = vec![
        hit("/Users/x/.ssh/a"),
        hit("/Users/x/a"),
        hit("/etc/b"),
        hit("/Users/x/Docs/c"),
        hit("/Users/x/d"),
    ];
    let out: Vec<_> = filter_hits(&p, hits.clone(), None, 2)
        .into_iter()
        .map(|h| h.path)
        .collect();
    assert_eq!(out, ["/Users/x/a", "/Users/x/Docs/c"]);
    let out: Vec<_> = filter_hits(&p, hits, Some(Path::new("/Users/x/Docs")), 10)
        .into_iter()
        .map(|h| h.path)
        .collect();
    assert_eq!(out, ["/Users/x/Docs/c"]);
    let many: Vec<_> = (0..500).map(|i| hit(&format!("/Users/x/f{i}"))).collect();
    assert_eq!(filter_hits(&p, many, None, 1000).len(), MAX_LIMIT);
}

#[test]
fn sort_and_compact_render() {
    assert_eq!(sort_arg(None).unwrap(), SortArg::Score);
    assert_eq!(sort_arg(Some("size")).unwrap(), SortArg::Size);
    assert_eq!(sort_arg(Some("newest")).unwrap(), SortArg::Mtime);
    assert!(sort_arg(Some("bogus")).is_err());
    let hits = vec![
        hit("/Users/x/Docs/a.pdf"),
        hit("/Users/xy/b"),
        hit("/etc/c"),
    ];
    assert_eq!(
        render(&hits, &home()),
        "~/Docs/a.pdf  1 B  1970-01-01 00:00\n/Users/xy/b  1 B  1970-01-01 00:00\n/etc/c  1 B  1970-01-01 00:00\n"
    );
    assert_eq!(render(&[], &home()), "no matches");
}

#[test]
fn log_line_format() {
    let l = log_line(
        1_700_000_000,
        "search_files",
        &serde_json::json!({"query": "a"}),
        3,
        12,
    );
    let v: serde_json::Value = serde_json::from_str(&l).unwrap();
    assert_eq!(v["timestamp"], 1_700_000_000);
    assert_eq!(v["tool"], "search_files");
    assert_eq!(v["args"]["query"], "a");
    assert_eq!(v["result_count"], 3);
    assert_eq!(v["took_ms"], 12);
    assert!(!l.contains('\n'));
}

#[test]
fn formatting() {
    assert_eq!(fmt_time(0), "1970-01-01 00:00");
    assert_eq!(fmt_time(1_791_731_000), "2026-10-11 15:03");
    assert_eq!(human_size(999), "999 B");
    assert_eq!(human_size(1_200_000), "1.2 MB");
}

#[test]
fn missing_snapshot_is_retried_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let h = dir.path().canonicalize().unwrap();
    fs::write(h.join("report.pdf"), "x").unwrap();
    let snap = h.join("index.bin");
    let ctx = Ctx::new(
        Policy::new(&h, None, vec![]),
        h.clone(),
        snap.clone(),
        h.join("mcp.log"),
        false,
    );
    assert!(ctx
        .filtered("report", None, SortArg::Score, KindFilter::All, None)
        .is_err());
    let (index, _) = ferret_core::scan::scan(&h).unwrap();
    ferret_core::snapshot::save(&index, &snap).unwrap();
    let hits = ctx
        .filtered("report", None, SortArg::Score, KindFilter::All, None)
        .unwrap();
    assert_eq!(hits.len(), 1);
}

#[tokio::test]
async fn end_to_end_over_duplex() {
    use rmcp::model::CallToolRequestParams;

    let dir = tempfile::tempdir().unwrap();
    let h = dir.path().canonicalize().unwrap();
    fs::create_dir_all(h.join("Documents")).unwrap();
    fs::create_dir_all(h.join(".ssh")).unwrap();
    fs::write(h.join("Documents/invoice-2026.pdf"), vec![0u8; 2048]).unwrap();
    fs::write(h.join(".ssh/invoice-key"), "x").unwrap();
    let (index, _) = ferret_core::scan::scan(&h).unwrap();
    let snap = h.join("index.bin");
    ferret_core::snapshot::save(&index, &snap).unwrap();
    let log = h.join("logs/mcp.log");
    let ctx = Ctx::new(
        Policy::new(&h, None, vec![]),
        h.clone(),
        snap,
        log.clone(),
        false,
    );

    let (server_io, client_io) = tokio::io::duplex(1 << 16);
    let server = tokio::spawn(serve(FerretMcp::new(ctx), server_io));
    let client = ().serve(client_io).await.unwrap();

    let info = client.peer_info().unwrap();
    assert!(info
        .instructions
        .as_deref()
        .unwrap_or("")
        .contains("find_files"));
    let tools = client.list_all_tools().await.unwrap();
    // Only find_files is loaded up front; file_info stays deferred so it costs nothing per turn.
    for t in &tools {
        let load = t
            .meta
            .as_ref()
            .and_then(|m| m.get("anthropic/alwaysLoad"))
            .cloned();
        assert_eq!(load.is_some(), t.name == "find_files", "{}", t.name);
    }
    let mut names: Vec<_> = tools.into_iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(names, ["file_info", "find_files"]);

    let args = serde_json::json!({"query": "invoice", "limit": 500})
        .as_object()
        .unwrap()
        .clone();
    let r = client
        .call_tool(CallToolRequestParams::new("find_files").with_arguments(args))
        .await
        .unwrap();
    let text = r.content[0].as_text().unwrap().text.clone();
    assert!(
        text.starts_with("~/Documents/invoice-2026.pdf  2.0 KB"),
        "{text}"
    );
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(!text.contains(".ssh"), "{text}");

    let q = format!("mod:<1d type:file in:{}", h.join("Documents").display());
    let args = serde_json::json!({"query": q, "sort": "newest"})
        .as_object()
        .unwrap()
        .clone();
    let r = client
        .call_tool(CallToolRequestParams::new("find_files").with_arguments(args))
        .await
        .unwrap();
    assert!(r.content[0]
        .as_text()
        .unwrap()
        .text
        .contains("invoice-2026.pdf"));

    client.cancel().await.unwrap();
    server.await.unwrap().unwrap();
    let logged = fs::read_to_string(&log).unwrap();
    assert_eq!(logged.lines().count(), 2);
    assert!(logged.contains("\"tool\":\"find_files\""));
}
