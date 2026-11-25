use super::*;
use std::net::TcpListener;
use std::sync::{Arc, Mutex, Once};

/// Tests log through drive.log: point RECORDINGS_ROOT at a temp folder, never the real inbox.
fn sandbox() -> PathBuf {
    static ONCE: Once = Once::new();
    // RECORDINGS_CACHE too: the partial-download folder must not be the real one
    let root = std::env::temp_dir().join(format!("drive-fetch-test-{}", std::process::id()));
    ONCE.call_once(|| {
        fs::create_dir_all(root.join("speaker-matching")).unwrap();
        std::env::set_var("RECORDINGS_ROOT", &root);
        std::env::set_var("RECORDINGS_CACHE", root.join("cache"));
    });
    root
}

#[test]
fn standard_names_match_python() {
    // (input, what drive_fetch.py's standard_name returns), computed with the Python.
    let cases: &[(&str, Option<&str>)] = &[
        ("20261123 - встреча с Ромашка", Some("20261123_-_встреча_с_Ромашка")),
        ("20261123_-_Acme_call", Some("20261123_-_Acme_call")),
        ("2026-11-23 Acme call", Some("20261123_-_Acme_call")),
        ("20261124_103522", None),
        ("20261124_-_Acme_call", Some("20261124_-_Acme_call")),
        ("20261123 - встреча с Ромашка Групп", Some("20261123_-_встреча_с_Ромашка_Групп")),
        ("2026-11-23 Dave sync", Some("20261123_-_Dave_sync")),
        ("20261117 - Разговор с Борисом", Some("20261117_-_Разговор_с_Борисом")),
        ("2026.11.23_x", Some("20261123_-_x")),
        ("20261323 - bad month", None),
        ("20260230 title", None),
        ("2026-11-23", None),
        ("2026-11-23 -", None),
        ("20261123—Atlas  debrief", Some("20261123_-_Atlas_debrief")),
        ("20261123 – a/b:c", Some("20261123_-_a_b_c")),
        ("20261123___x__y", Some("20261123_-_x_y")),
        ("20261123-x", Some("20261123_-_x")),
        ("2026112-3 x", None),
        ("20261123 - 123", None),
        ("20261123 - ._ -", None),
        ("20261123x title", None),
        ("audio_2026-11-24_07-50-12", None),
        ("Standard recording 13", None),
        ("20261123 - Q4 review.", Some("20261123_-_Q4_review")),
    ];
    for (input, want) in cases {
        assert_eq!(standard_name(input).as_deref(), *want, "{input}");
    }
}

#[test]
fn python_path_pieces() {
    let cases = [("a.ogg", "a", ".ogg"), ("name.", "name", "."), (".hidden", ".hidden", ""), ("a.b.c", "a.b", ".c"),
                 ("x/y.m4a", "y", ".m4a"), ("foo/", "foo", ""), ("  x .mp3", "  x ", ".mp3"), ("a. b", "a", ". b"),
                 ("Call 1/2.m4a", "2", ".m4a"), ("./a.mp3", "a", ".mp3")];
    for (n, stem, suffix) in cases {
        assert_eq!((py_stem(n), py_suffix(n)), (stem, suffix), "{n}");
    }
}

#[test]
fn drive_times() {
    let t = parse_drive_time("2026-11-24T04:48:46.123Z").unwrap();
    assert_eq!(util::utc_iso(t), "2026-11-24T04:48:46+00:00");
    assert!(parse_drive_time("2026-11-24").is_err());
    assert_eq!(parse_creation_time("2026-11-24T04:48:46.000000Z\n").map(util::utc_iso).as_deref(),
               Some("2026-11-24T04:48:46+00:00"));
    assert_eq!(parse_creation_time("2026-11-24 04:48:46\nsecond\n").map(util::utc_iso).as_deref(),
               Some("2026-11-24T04:48:46+00:00"));
    assert_eq!(parse_creation_time("1970-01-01T00:00:00.000000Z"), None);
    assert_eq!(parse_creation_time("1904-01-01T00:00:00.000000Z"), None);
    assert_eq!(parse_creation_time(""), None);
    assert_eq!(parse_creation_time("garbage"), None);
}

#[test]
fn media_kinds() {
    let f = |name: &str, mime: &str| is_media(&json!({"name": name, "mimeType": mime})).unwrap();
    assert!(f("audio_2026.ogg", "audio/ogg"));
    assert!(f("x", "audio/mpeg"));
    assert!(f("clip.MOV", "application/octet-stream"));
    assert!(f("a.m4a", ""));
    assert!(!f("notes.pdf", "application/pdf"));
    assert!(!f("a.m4a", "application/vnd.google-apps.document"));
}

#[test]
fn claude_replies() {
    let p = |s: &str| parse_name_reply(s).map_err(|e| format!("{e:?}"));
    assert_eq!(p(r#"{"date": "20261123", "title": "Ромашка групп"}"#).unwrap(),
               (Some("20261123".into()), Some("Ромашка групп".into())));
    assert_eq!(p("Sure:\n```json\n{\"date\": null, \"title\": \"call_with: Dave \"}\n```").unwrap(),
               (None, Some("call with Dave".into())));
    assert_eq!(p(r#"{"date": "20261332", "title": null}"#).unwrap(), (None, None));
    assert_eq!(p(r#"{"date": 20261123, "title": ""}"#).unwrap(), (None, None));
    assert_eq!(p("no json here").unwrap(), (None, None));
    let long = "x".repeat(80);
    assert_eq!(p(&format!(r#"{{"title": "{long}"}}"#)).unwrap().1.unwrap().len(), 60);
    assert!(matches!(parse_name_reply("{not json}"), Err(NameErr::Soft(_))));
    // Python bug kept: a non-text title fails the file (retried next run) instead of being ignored.
    assert!(matches!(parse_name_reply(r#"{"title": 5}"#), Err(NameErr::Hard(_))));
    assert_eq!(p(r#"{"title": 0}"#).unwrap(), (None, None));
}

#[test]
fn prompt_wording() {
    let local = Local.with_ymd_and_hms(2026, 11, 24, 10, 0, 0).unwrap();
    let p = read_name_prompt(local);
    assert!(p.starts_with("A recording was uploaded with the file name below (without extension). It was recorded on \
                           2026-11-24 (Tuesday). Reply with JSON only: {\"date\": \"YYYYMMDD\" or null, \"title\": string or null}.\n- title: "));
    assert!(p.ends_with("day before the recording date); otherwise null."));
    assert_eq!(p.matches('\n').count(), 2);
}

#[test]
fn names() {
    sandbox();
    let when = Utc.with_ymd_and_hms(2026, 11, 24, 7, 22, 36).unwrap();
    let local = when.with_timezone(&Local);
    let (day, hhmm, hhmmss) = (local.format("%Y%m%d").to_string(), local.format("%H%M").to_string(),
                               local.format("%H%M%S").to_string());
    let free = |_: &str| Ok(false);
    let never = |_: &str, _: DateTime<Local>| -> Result<(Option<String>, Option<String>), NameErr> {
        panic!("Claude must not be asked")
    };
    // standard form: kept, Claude not asked
    assert_eq!(make_name_with("20261117 - Разговор с Борисом.mp3", when, Some("claude"), never, &free).unwrap(),
               ("20261117_-_Разговор_с_Борисом".to_string(), false));
    // no letters: Claude not asked, placeholder
    assert_eq!(make_name_with("20261124_103522.m4a", when, Some("claude"), never, &free).unwrap(),
               (format!("{day}_-_Drive_{hhmm}"), true));
    // name_reader none
    assert_eq!(make_name_with("Standard recording 13.mp3", when, Some("none"), never, &free).unwrap(),
               (format!("{day}_-_Drive_{hhmm}"), true));
    // Claude finds a title, with or without a date
    let reader = |stem: &str, _: DateTime<Local>| -> Result<(Option<String>, Option<String>), NameErr> {
        Ok(match stem {
            "Ромашка групп 23 ноября" => (Some("20261123".into()), Some("Ромашка групп".into())),
            "call with Dave" => (None, Some("call with Dave".into())),
            "down" => return Err(NameErr::Soft("no answer from Claude in 120s".into())),
            "bad" => return Err(NameErr::Hard("title is not text: 5".into())),
            _ => (None, None),
        })
    };
    assert_eq!(make_name_with("Ромашка групп 23 ноября.ogg", when, Some("claude"), reader, &free).unwrap(),
               ("20261123_-_Ромашка_групп".to_string(), false));
    assert_eq!(make_name_with("call with Dave.m4a", when, Some("claude"), reader, &free).unwrap(),
               (format!("{day}_-_call_with_Dave"), false));
    assert_eq!(make_name_with("Standard recording 13.mp3", when, Some("claude"), reader, &free).unwrap(),
               (format!("{day}_-_Drive_{hhmm}"), true));
    assert_eq!(make_name_with("down.mp3", when, Some("claude"), reader, &free).unwrap(),
               (format!("{day}_-_Drive_{hhmm}"), true));
    assert!(make_name_with("bad.mp3", when, Some("claude"), reader, &free).is_err());
    // collision: local time with seconds
    let busy = |n: &str| Ok(n == "20261117_-_x");
    assert_eq!(make_name_with("20261117 - x.mp3", when, Some("none"), never, &busy).unwrap(),
               (format!("20261117_-_x_{hhmmss}"), false));
    // ... and when that is taken too: _2, _3 (never an overwrite)
    let timed = format!("20261117_-_x_{hhmmss}");
    let busier = |n: &str| Ok(n == "20261117_-_x" || n == timed || n == format!("{timed}_2"));
    assert_eq!(make_name_with("20261117 - x.mp3", when, Some("none"), never, &busier).unwrap(),
               (format!("{timed}_3"), false));
    // a '/' in a Drive name keeps what comes before it
    assert_eq!(make_name_with("20261117 - Call 1/2.m4a", when, Some("claude"), never, &free).unwrap(),
               ("20261117_-_Call_1_2".to_string(), false));
}

#[test]
fn taken_looks_for_name_dot() {
    let dir = std::env::temp_dir().join(format!("drive-fetch-taken-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("20261124_-_Drive_0748.mp3"), "").unwrap();
    assert!(taken_in("20261124_-_Drive_0748", &[dir.join("missing"), dir.clone()]).unwrap());
    assert!(!taken_in("20261124_-_Drive_07", std::slice::from_ref(&dir)).unwrap());
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- the guard

#[test]
fn guard_rules() {
    let mut listed = HashMap::new();
    listed.insert("f1".to_string(), vec!["inbox".to_string()]);
    listed.insert("f2".to_string(), vec!["elsewhere".to_string()]);
    let trash = json!({"trashed": true});
    let ok = |m, p, b: Option<&Value>, f| guard::check(m, p, b, f, &listed).is_ok();
    let why = |m, p, b: Option<&Value>, f| guard::check(m, p, b, f, &listed).unwrap_err().0;

    assert!(ok("GET", "/files", None, None));
    assert!(ok("GET", "/files/f9", None, None));
    assert_eq!(why("DELETE", "/files/f1", None, Some("inbox")), "HTTP DELETE is permanent in Drive and is never sent");
    assert_eq!(why("PUT", "/files/f1", Some(&trash), Some("inbox")), "PUT is not allowed");
    assert_eq!(why("POST", "/upload", None, None), "the only POST allowed is creating the inbox folder");

    // POST: only a new folder, with nothing but name, mimeType, parents
    let folder = json!({"name": "Recordings inbox", "mimeType": FOLDER_MIME, "parents": ["root"]});
    assert!(ok("POST", "/files", Some(&folder), None));
    assert!(ok("POST", "/files", Some(&json!({"mimeType": FOLDER_MIME})), None));
    for bad in [json!({"name": "x", "mimeType": "audio/mpeg"}), json!({}),
                json!({"name": "x", "mimeType": FOLDER_MIME, "description": "y"}), json!([1])] {
        assert_eq!(why("POST", "/files", Some(&bad), None), "the only POST allowed is creating the inbox folder", "{bad}");
    }
    assert!(!ok("POST", "/files/f1/copy", Some(&folder), None));
    assert!(!ok("POST", "/files", None, None));

    // PATCH: only {"trashed": true}, only on a file listed in the watched folder
    let refused = "PATCH may only trash a file listed in the inbox folder";
    assert!(ok("PATCH", "/files/f1", Some(&trash), Some("inbox")));
    assert_eq!(why("PATCH", "/files/f2", Some(&trash), Some("inbox")), refused);
    assert_eq!(why("PATCH", "/files/f9", Some(&trash), Some("inbox")), refused);
    assert_eq!(why("PATCH", "/files/f1", Some(&trash), None), refused);
    assert_eq!(why("PATCH", "/files/f1", Some(&trash), Some("")), refused);
    assert_eq!(why("PATCH", "/files/f1", None, Some("inbox")), refused);
    for bad in [json!({"trashed": false}), json!({"trashed": true, "name": "x"}), json!({"name": "x"}),
                json!({"trashed": "true"}), json!({"parents": ["inbox"]})] {
        assert_eq!(why("PATCH", "/files/f1", Some(&bad), Some("inbox")), refused, "{bad}");
    }
}

// ---------------------------------------------------------------- against a local fake Drive

type Seen = Arc<Mutex<Vec<String>>>;

/// A one-thread HTTP server: answers each request with `reply(method, path)` and records
/// "METHOD path body".
fn fake_drive(reply: impl Fn(&str, &str) -> (u16, Vec<u8>) + Send + 'static) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut req = Vec::new();
            let mut buf = [0u8; 8192];
            let head_end = loop {
                let n = s.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                req.extend_from_slice(&buf[..n]);
                if let Some(i) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i + 4);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&req[..head_end]).to_string();
            let len: usize = head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok()).flatten()
            }).unwrap_or(0);
            while req.len() < head_end + len {
                let n = s.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
            }
            let body = String::from_utf8_lossy(&req[head_end..]).to_string();
            let mut first = head.lines().next().unwrap_or("").split(' ');
            let (m, p) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
            seen2.lock().unwrap().push(format!("{m} {p} {body}").trim_end().to_string());
            let (code, data) = reply(&m, &p);
            let _ = s.write_all(format!("HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                        data.len()).as_bytes());
            let _ = s.write_all(&data);
        }
    });
    (base, seen)
}

fn test_drive(base: &str) -> Drive {
    Drive { cred: json!({}), token: Some("tok".into()), listed: HashMap::new(), api: base.into(),
            token_url: format!("{base}/token") }
}

const AUDIO: &[u8] = b"fake audio bytes";

fn md5_hex(b: &[u8]) -> String {
    use md5::Digest;
    md5::Md5::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn download_checks_size_and_md5() {
    let (base, seen) = fake_drive(|m, p| match (m, p) {
        ("GET", p) if p.starts_with("/files/f1?alt=media") => (200, AUDIO.to_vec()),
        _ => (404, b"{}".to_vec()),
    });
    let dir = std::env::temp_dir().join(format!("drive-fetch-dl-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let mut d = test_drive(&base);
    let dest = dir.join("f1.ogg");

    let f = json!({"id": "f1", "size": AUDIO.len().to_string(), "md5Checksum": md5_hex(AUDIO)});
    d.download(&f, &dest).unwrap();
    assert_eq!(fs::read(&dest).unwrap(), AUDIO);
    assert!(!dir.join("f1.ogg.part").exists());
    fs::remove_file(&dest).unwrap();

    let short = json!({"id": "f1", "size": "999"});
    let e = d.download(&short, &dest).unwrap_err().to_string();
    assert_eq!(e, format!("incomplete download ({} of 999 bytes)", AUDIO.len()));
    assert!(!dest.exists() && !dir.join("f1.ogg.part").exists());

    let wrong = json!({"id": "f1", "size": AUDIO.len().to_string(), "md5Checksum": "00"});
    assert_eq!(d.download(&wrong, &dest).unwrap_err().to_string(), "checksum does not match Drive's");
    assert!(!dest.exists() && !dir.join("f1.ogg.part").exists());

    let missing = json!({"id": "nope"});
    let e = d.download(&missing, &dest).unwrap_err();
    assert_eq!(e.downcast_ref::<HttpError>().map(|h| h.code), Some(404));
    assert_eq!(e.to_string(), "HTTP Error 404: X");

    assert!(seen.lock().unwrap().iter().all(|r| r.starts_with("GET ")));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn trash_only_what_was_listed() {
    let (base, seen) = fake_drive(|m, p| match (m, p) {
        ("GET", p) if p.starts_with("/files?") => (200, br#"{"files": [
            {"id": "f1", "name": "a.ogg", "parents": ["inbox"]},
            {"id": "f2", "name": "b.ogg", "parents": ["inbox", "other"]}]}"#.to_vec()),
        ("PATCH", _) => (200, br#"{"id": "f1", "trashed": true}"#.to_vec()),
        _ => (404, b"{}".to_vec()),
    });
    let mut d = test_drive(&base);
    // nothing listed yet: refused before anything is sent
    let e = d.trash("f1", "inbox").unwrap_err();
    assert!(e.downcast_ref::<guard::Refused>().is_some());
    assert!(seen.lock().unwrap().is_empty());

    let files = d.list_inbox("inbox").unwrap();
    assert_eq!(files.len(), 2);
    d.trash("f1", "inbox").unwrap();
    assert!(d.trash("f3", "inbox").is_err());
    assert!(d.trash("f1", "other").is_err());
    assert!(d.request("DELETE", "/files/f1", &[], None, Some("inbox")).is_err());
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(seen[0].starts_with("GET /files?q=%27inbox%27+in+parents+and+trashed+%3D+false&"), "{}", seen[0]);
    assert_eq!(seen[1], r#"PATCH /files/f1?fields=id%2Ctrashed {"trashed":true}"#);
}

#[test]
fn expired_login_message() {
    let (base, _) = fake_drive(|_, p| match p {
        "/token" => (400, br#"{"error": "invalid_grant", "error_description": "Token has been expired or revoked."}"#.to_vec()),
        _ => (500, b"oops".to_vec()),
    });
    let mut d = test_drive(&base);
    d.token = None;
    d.cred = json!({"client_id": "c", "client_secret": "s", "refresh_token": "r"});
    let e = d.token().unwrap_err();
    let msg = e.downcast_ref::<Exit>().expect("an Exit").0.clone();
    assert!(msg.starts_with("ERROR Google login expired or was revoked"), "{msg}");
    assert!(msg.contains("login expired"));
}

#[test]
fn convert_and_creation_time() {
    let Some(ffmpeg) = common::tool("ffmpeg") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("drive-fetch-ff-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let ogg = dir.join("in.ogg");
    let st = Command::new(&ffmpeg).args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=1",
                                        "-c:a", "libopus", "-metadata", "creation_time=2026-11-20T08:15:00Z"])
        .arg(&ogg).status().unwrap();
    assert!(st.success());
    let m4a = dir.join("out.m4a");
    assert!(convert(&ogg, &m4a).is_ok());
    assert!(fs::metadata(&m4a).unwrap().len() > 0);
    let fallback = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
    // ffprobe reads the ogg's creation_time from its stream tags only for some containers; the m4a
    // written by ffmpeg has a format-level creation_time only if one was set, so check both ways.
    let t = recorded_at(&ogg, fallback);
    assert!(t == fallback || util::utc_iso(t) == "2026-11-20T08:15:00+00:00", "{t}");
    let bad = dir.join("bad.ogg");
    fs::write(&bad, b"not audio").unwrap();
    assert!(matches!(convert(&bad, &dir.join("bad.m4a")), Err(FetchErr::Convert(e)) if !e.is_empty()));
    assert_eq!(recorded_at(&bad, fallback), fallback);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dumps_like_python() {
    assert_eq!(py_dumps_flat(&[("type", "authorized_user"), ("client_id", "a\"b")]),
               r#"{"type": "authorized_user", "client_id": "a\"b"}"#);
    assert_eq!(py_dumps_flat(&[("k", "é")]), "{\"k\": \"\\u00e9\"}");
}

#[test]
fn loopback_query() {
    let q = query_params("/?state=abc&code=4%2F0Ab&scope=x+y&empty=");
    assert_eq!(q.get("code").map(String::as_str), Some("4/0Ab"));
    assert_eq!(q.get("scope").map(String::as_str), Some("x y"));
    assert!(!q.contains_key("empty"));
}

#[test]
fn creation_time_from_ffprobe() {
    let Some(ffmpeg) = common::tool("ffmpeg") else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("drive-fetch-ct-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let m4a = dir.join("t.m4a");
    let st = Command::new(&ffmpeg).args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=1",
                                        "-c:a", "aac", "-metadata", "creation_time=2026-11-20T08:15:00Z"])
        .arg(&m4a).status().unwrap();
    assert!(st.success());
    let fallback = Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap();
    assert_eq!(util::utc_iso(recorded_at(&m4a, fallback)), "2026-11-20T08:15:00+00:00");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn folder_found_or_created() {
    let _g = SANDBOX.lock().unwrap_or_else(|e| e.into_inner());
    sandbox();
    let mut cfg = default_config();
    cfg["folder_name"] = json!("Recordings 'inbox'");
    let (base, seen) = fake_drive(|m, _| match m {
        "GET" => (200, br#"{"files": [{"id": "F1", "name": "Recordings inbox"}]}"#.to_vec()),
        _ => (500, b"{}".to_vec()),
    });
    assert_eq!(test_drive(&base).ensure_folder(&mut cfg, true).unwrap().as_deref(), Some("F1"));
    assert_eq!(cfg["folder_id"], "F1");
    assert_eq!(common::load_json(&config_file()).unwrap()["folder_id"], "F1");
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("q=name+%3D+%27Recordings+%5C%27inbox%5C%27%27+and+mimeType"), "{}", seen[0]);
    }
    // folder_id set: no request at all
    assert_eq!(test_drive(&base).ensure_folder(&mut cfg, true).unwrap().as_deref(), Some("F1"));
    assert_eq!(seen.lock().unwrap().len(), 1);

    let mut cfg = default_config();
    let (base, seen) = fake_drive(|m, _| match m {
        "GET" => (200, br#"{"files": []}"#.to_vec()),
        "POST" => (200, br#"{"id": "NEW"}"#.to_vec()),
        _ => (500, b"{}".to_vec()),
    });
    // dry run: looks, creates nothing, writes no config
    let _ = fs::remove_file(config_file());
    assert_eq!(test_drive(&base).ensure_folder(&mut cfg, false).unwrap(), None);
    assert!(!config_file().exists());
    assert_eq!(test_drive(&base).ensure_folder(&mut cfg, true).unwrap().as_deref(), Some("NEW"));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3, "the dry run only looked: {seen:?}");
    assert_eq!(seen[2], format!(r#"POST /files?fields=id {{"name":"Recordings inbox","mimeType":"{FOLDER_MIME}","parents":["root"]}}"#));
}

/// Tests that write the sandbox's config and state take turns.
static SANDBOX: Mutex<()> = Mutex::new(());

#[test]
fn whole_run_against_fake_drive() {
    let Some(ffmpeg) = common::tool("ffmpeg") else {
        return;
    };
    let _g = SANDBOX.lock().unwrap_or_else(|e| e.into_inner());
    let root = sandbox();
    let src = root.join("src.ogg");
    assert!(Command::new(&ffmpeg).args(["-v", "error", "-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=1",
                                       "-c:a", "libopus"]).arg(&src).status().unwrap().success());
    let ogg = fs::read(&src).unwrap();
    fs::remove_file(&src).unwrap();
    let mp3 = b"ID3 not really an mp3".to_vec();

    let mut cfg = default_config();
    cfg["folder_id"] = json!("inbox");
    cfg["name_reader"] = json!("none");
    save_config(&cfg).unwrap();
    common::save_json(&state_file(), &json!({"old": {"status": "fetched", "name": "20261120_-_old", "title_pending": false,
        "drive_name": "old.m4a", "recorded": "2026-11-20T10:00:00+00:00", "at": "2026-11-20T10:01:00+00:00", "trashed": false}})).unwrap();

    let fresh = Utc::now().format("%Y-%m-%dT%H:%M:%S.000Z").to_string();
    let file = |id: &str, name: &str, mime: &str, data: Option<&[u8]>, created: &str, modified: &str| {
        let mut f = json!({"id": id, "name": name, "mimeType": mime, "createdTime": created, "modifiedTime": modified,
                           "parents": ["inbox"]});
        if let Some(d) = data {
            f["size"] = json!(d.len().to_string());
            f["md5Checksum"] = json!(md5_hex(d));
        }
        f
    };
    let t0 = "2026-11-24T07:05:00.000Z";
    let mut broken = file("f6", "broken.m4a", "audio/mp4", Some(b"zzz"), t0, t0);
    broken["md5Checksum"] = json!("00");
    let listing = json!({"files": [
        file("f2", "voice.ogg", "audio/ogg", Some(&ogg), t0, t0),
        file("old", "old.m4a", "audio/mp4", None, "2026-11-20T10:00:00.000Z", "2026-11-20T10:00:00.000Z"),
        file("f1", "20261117 - Разговор с Борисом.mp3", "audio/mpeg", Some(&mp3), "2026-11-24T07:00:00.000Z", ""),
        file("f3", "notes.pdf", "application/pdf", None, t0, t0),
        file("f4", "new.m4a", "audio/mp4", None, &fresh, &fresh),
        file("f5", "sub", FOLDER_MIME, None, t0, t0),
        broken,
    ]}).to_string().into_bytes();
    let (ogg2, mp32) = (ogg.clone(), mp3.clone());
    let (base, seen) = fake_drive(move |m, p| match (m, p) {
        ("GET", p) if p.starts_with("/files?") => (200, listing.clone()),
        ("GET", p) if p.starts_with("/files/f1?alt=media") => (200, mp32.clone()),
        ("GET", p) if p.starts_with("/files/f2?alt=media") => (200, ogg2.clone()),
        ("GET", p) if p.starts_with("/files/f6?alt=media") => (200, b"zzz".to_vec()),
        ("PATCH", _) => (200, br#"{"trashed": true}"#.to_vec()),
        _ => (404, b"{}".to_vec()),
    });
    let connect = || -> Result<Drive> { Ok(test_drive(&base)) };

    // dry run: lists, downloads nothing, trashes nothing
    let dry = util::Args { flags: vec!["--dry-run".into()], values: vec![] };
    let state_before = fs::read(state_file()).ok();
    assert_eq!(run(&dry, &connect).unwrap(), 0);
    assert_eq!(fs::read(state_file()).ok(), state_before, "a dry run writes no state");
    assert!(seen.lock().unwrap().iter().all(|r| r.starts_with("GET /files?")), "{:?}", seen.lock().unwrap());
    let log_text = fs::read_to_string(log_file()).unwrap();
    assert!(log_text.contains("[dry run] would fetch 'voice.ogg' (0.0 MB)"));
    assert!(log_text.contains("[dry run] 'notes.pdf': not audio or video (application/pdf); would leave it"));
    seen.lock().unwrap().clear();

    assert_eq!(run(&util::Args::default(), &connect).unwrap(), 0);
    let local = parse_drive_time(t0).unwrap().with_timezone(&Local);
    let pending_name = format!("{}_-_Drive_{}", local.format("%Y%m%d"), local.format("%H%M"));
    assert_eq!(fs::read(root.join("20261117_-_Разговор_с_Борисом.mp3")).unwrap(), mp3);
    assert!(fs::metadata(root.join(format!("{pending_name}.m4a"))).unwrap().len() > 0);
    let cache: Vec<_> = fs::read_dir(root.join("cache/local.drive-recordings")).unwrap().collect();
    assert!(cache.is_empty(), "{cache:?}");

    let state = common::load_json(&state_file()).unwrap();
    assert_eq!(state["old"]["trashed"], true);
    assert_eq!(state["f1"], json!({"status": "fetched", "name": "20261117_-_Разговор_с_Борисом", "title_pending": false,
        "drive_name": "20261117 - Разговор с Борисом.mp3", "recorded": "2026-11-24T07:00:00+00:00",
        "at": state["f1"]["at"], "trashed": true}));
    assert_eq!(state["f2"]["name"], json!(pending_name));
    assert_eq!(state["f2"]["title_pending"], true);
    assert_eq!(state["f2"]["trashed"], true);
    assert_eq!(state["f3"]["status"], "skipped");
    assert_eq!(state["f3"]["mime"], "application/pdf");
    for id in ["f4", "f5", "f6"] {
        assert!(state.get(id).is_none(), "{id}");
    }
    let keys: Vec<_> = state["f1"].as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, ["status", "name", "title_pending", "drive_name", "recorded", "at", "trashed"]);

    let patches: Vec<String> = seen.lock().unwrap().iter().filter(|r| r.starts_with("PATCH")).cloned().collect();
    assert_eq!(patches.len(), 3, "{patches:?}");
    for id in ["old", "f1", "f2"] {
        assert!(patches.iter().any(|p| p.starts_with(&format!("PATCH /files/{id}?"))), "{id}");
    }
    let log_text = fs::read_to_string(log_file()).unwrap();
    for line in ["'old.m4a': moved to the Drive trash",
                 "fetched '20261117 - Разговор с Борисом.mp3' -> '20261117_-_Разговор_с_Борисом.mp3'\n",
                 &format!("fetched 'voice.ogg' -> '{pending_name}.m4a' (temporary name; titled from the full transcript later)"),
                 "'broken.m4a': ERROR checksum does not match Drive's; will retry"] {
        assert!(log_text.contains(line), "{line}\n{log_text}");
    }
    assert_eq!(log_text.matches("'notes.pdf': not audio or video (application/pdf); left in Drive").count(), 1);
}

/// Asks the real Claude (two calls): `cargo test -p drive-fetch -- --ignored real_claude`.
#[test]
#[ignore]
fn real_claude_reads_names() {
    sandbox();
    let local = Local.with_ymd_and_hms(2026, 11, 24, 10, 0, 0).unwrap();
    let (date, title) = read_name("Ромашка групп 23 ноября", local).unwrap();
    println!("Ромашка групп 23 ноября -> {date:?} {title:?}");
    assert_eq!(date.as_deref(), Some("20261123"));
    assert!(title.unwrap().contains("Ромашка"));
    let (date, title) = read_name("Standard recording 13", local).unwrap();
    println!("Standard recording 13 -> {date:?} {title:?}");
    assert_eq!(title, None);
}

#[test]
fn default_config_file_as_python_writes_it() {
    // json.dumps(drive_fetch.DEFAULT_CONFIG, indent=2, ensure_ascii=False) + "\n", from the Python
    assert_eq!(common::to_pretty(&default_config()), include_str!("default_config.python.json"));
}

/// A file that cannot be converted is downloaded once, not every minute.
#[test]
fn unconvertible_file_is_not_fetched_again() {
    let _g = SANDBOX.lock().unwrap_or_else(|e| e.into_inner());
    let _root = sandbox();
    let mut cfg = default_config();
    cfg["folder_id"] = json!("inbox");
    cfg["name_reader"] = json!("none");
    cfg["trash_after_fetch"] = json!(false);
    save_config(&cfg).unwrap();
    common::save_json(&state_file(), &json!({})).unwrap();
    let junk = b"not audio at all".to_vec();
    let t0 = "2026-11-24T07:05:00.000Z";
    let listing = json!({"files": [{"id": "u1", "name": "memo.ogg", "mimeType": "audio/ogg", "createdTime": t0,
        "modifiedTime": t0, "parents": ["inbox"], "size": junk.len().to_string(), "md5Checksum": md5_hex(&junk)}]})
        .to_string().into_bytes();
    let junk2 = junk.clone();
    let (base, seen) = fake_drive(move |m, p| match (m, p) {
        ("GET", p) if p.starts_with("/files?") => (200, listing.clone()),
        ("GET", p) if p.starts_with("/files/u1?alt=media") => (200, junk2.clone()),
        _ => (404, b"{}".to_vec()),
    });
    let connect = || -> Result<Drive> { Ok(test_drive(&base)) };
    let downloads = || seen.lock().unwrap().iter().filter(|r| r.starts_with("GET /files/u1?alt=media")).count();
    assert_eq!(run(&util::Args::default(), &connect).unwrap(), 0);
    assert_eq!(downloads(), 1);
    assert_eq!(common::load_json(&state_file()).unwrap()["u1"]["status"], "unconvertible");
    assert_eq!(run(&util::Args::default(), &connect).unwrap(), 0);
    assert_eq!(downloads(), 1, "not downloaded again");
}

#[test]
fn long_names_are_capped() {
    let long = format!("20261124 - {}", "word ".repeat(40));
    let n = standard_name(&long).unwrap();
    let title = n.trim_start_matches("20261124_-_");
    assert!(title.chars().count() <= TITLE_MAX && title.ends_with("word"), "{n}");
    assert_eq!(cap_words("abc", 60), "abc");
    assert_eq!(cap_words(&"x".repeat(70), 60).len(), 60);
}
