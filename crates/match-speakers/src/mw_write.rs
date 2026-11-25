//! Naming a speaker from the menu bar, written into MacWhisper's own library.
//!
//! MacWhisper has no API for this (its `mw` tool and Shortcuts actions only transcribe and read), so
//! the change is made in its database the way MacWhisper's own rename does it (seen in its data,
//! 15.11): the recording's lines and speaker link move from the "Speaker N" stub to the person (a
//! new person is the stub renamed), and the stub is left behind unlinked. MacWhisper learns voices
//! only when its background job gets round to it, so the speaker's voiceprint is added at once,
//! computed with MacWhisper's own model (vpembed) and stored as MacWhisper stores it; the matcher
//! uses it from its next run, and so does MacWhisper.
//!
//! Safety: the structure is checked first and nothing is written if it differs; a copy of the
//! database is kept once a day (speaker-matching/macwhisper-backups, last 3); the write is one
//! short transaction (MacWhisper may be running: SQLite's locking keeps both consistent); every
//! change goes to speaker-matching/macwhisper-writes.jsonl with what it replaced. An open
//! MacWhisper window shows the new name once it reloads that recording.

use super::*;
use rand::RngCore;

/// The columns this code reads or writes, per table; anything else in MacWhisper's database is
/// left alone, and a missing column means a MacWhisper update changed it: no write.
const NEEDED: [(&str, &[&str]); 4] = [
    ("speaker", &["id", "name", "color", "isStub"]),
    ("session_speaker", &["sessionID", "speakerID", "isRecognized"]),
    ("transcriptline", &["sessionId", "speakerID"]),
    ("speakervoiceprint", &["id", "sessionID", "speakerID", "modelID", "embedding", "speechDuration", "dateCreated"]),
];
const DEFAULT_MODEL: &str = "speakerkit-pyannote-2.0.9-trainable-centroid";

fn writes_log() -> PathBuf {
    meta().join("macwhisper-writes.jsonl")
}

fn backups_dir() -> PathBuf {
    meta().join("macwhisper-backups")
}

fn check_schema(con: &Connection) -> Result<()> {
    for (table, cols) in NEEDED {
        let mut st = con.prepare(&format!("pragma table_info(\"{table}\")"))?;
        let have: Vec<String> = st.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<_>>()?;
        if let Some(c) = cols.iter().find(|c| !have.iter().any(|h| h == *c)) {
            bail!("MacWhisper's database has changed ({table}.{c} is missing); nothing was written");
        }
    }
    Ok(())
}

/// One copy of the database per day before the first write (VACUUM INTO: a consistent copy while
/// MacWhisper runs); the last 3 are kept.
fn daily_backup(con: &Connection) -> Result<()> {
    let dir = backups_dir();
    mkdir(&dir)?;
    let file = dir.join(format!("main-{}.sqlite", chrono::Local::now().format("%Y%m%d")));
    if !file.exists() {
        con.execute("vacuum into ?1", [file.to_string_lossy()]).context("backing up MacWhisper's database")?;
        let mut old: Vec<PathBuf> = entries(&dir).into_iter().filter(|p| file_name(p).starts_with("main-")).collect();
        old.sort();
        while old.len() > 3 {
            let _ = fs::remove_file(old.remove(0));
        }
    }
    Ok(())
}

fn new_id() -> Vec<u8> {
    let mut b = vec![0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40; // UUID v4, like MacWhisper's ids
    b[8] = (b[8] & 0x3f) | 0x80;
    b
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// People named in MacWhisper (not "Speaker N" stubs) that appear in a recording or have a
/// voiceprint, by name; for the menu's list.
pub fn known_people() -> Result<Vec<String>> {
    let con = db()?;
    let mut st = con.prepare(
        "select distinct s.name from speaker s
         where s.isStub = 0 and trim(s.name) != ''
           and (exists (select 1 from session_speaker ss where ss.speakerID = s.id)
                or exists (select 1 from speakervoiceprint v where v.speakerID = s.id))
         order by s.name collate nocase")?;
    let names: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(names.into_iter().filter(|n| !is_speaker_label(n)).collect())
}

/// What happened, for the notification and the log.
pub struct Renamed {
    pub from: String,
    pub to: String,
    pub new_person: bool,
    pub voiceprint_added: bool,
}

/// Name speaker `speaker_hex` of MacWhisper recording `session_hex` as `name`: an existing person
/// of that name (the recording's lines and link move to them) or a new one.
pub fn rename_speaker(session_hex: &str, speaker_hex: &str, name: &str, transcript: &str) -> Result<Renamed> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() || is_speaker_label(&name) {
        bail!("'{name}' is not a name");
    }
    let (sess, src) = (hex_blob(session_hex)?, hex_blob(speaker_hex)?);

    // Read and compute first (vpembed takes a while); the write below is short.
    let (from, src_is_stub, src_sessions, target, spans, audio) = {
        let con = db()?;
        check_schema(&con)?;
        let mut st = con.prepare(
            "select s.name, s.isStub, (select count(*) from session_speaker x where x.speakerID = s.id)
             from speaker s join session_speaker ss on ss.speakerID = s.id
             where ss.sessionID = ?1 and s.id = ?2")?;
        let mut rows = st.query(rusqlite::params![sess, src])?;
        let Some(r) = rows.next()? else { bail!("that speaker is not in this recording in MacWhisper (any more)") };
        let (from, is_stub, n): (String, bool, i64) = (r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?);
        drop(rows);
        if from == name {
            bail!("the speaker is already called {name}");
        }
        // the existing person of that name with the most recordings
        let target: Option<Vec<u8>> = con.query_row(
            "select id from speaker s where isStub = 0 and name = ?1 and id != ?2
             order by (select count(*) from session_speaker x where x.speakerID = s.id) desc limit 1",
            rusqlite::params![name, src], |r| r.get(0)).ok();
        let lines = session_lines(&con, session_hex)?;
        let spans: Vec<_> = text::speaker_spans(&lines, 0.3).into_iter().filter(|(k, _)| *k == speaker_hex).collect();
        let stem = py::stem(transcript).to_string();
        let entry = obj(load_json_obj(&state_file()).get(transcript));
        let own = |p: &Path| if entry.contains_key("inbox_audio") { own_audio(p, &entry, &stem) }
            else { pipeline_common::name_key(py::stem(&file_name(p))) == pipeline_common::name_key(&stem) };
        let audio = session_media(&con, session_hex, &own)?;
        (from, is_stub, n, target, spans, audio)
    };
    let voice = match (&audio, spans.is_empty()) {
        (Some(a), false) => vpembed_means(a, &spans)?.into_iter().next(),
        _ => None,
    };

    let mut con = Connection::open_with_flags(db_path(), OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .context("unable to open MacWhisper's database for writing")?;
    con.busy_timeout(Duration::from_secs(30))?;
    check_schema(&con)?;
    daily_backup(&con)?;
    let tx = con.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let new_person = target.is_none();
    let (to_id, moved_lines, moved_vps) = match target {
        // A "Speaker N" stub belongs to this one recording: renaming it makes it the new person,
        // keeping its lines and any voiceprint MacWhisper made for it.
        None if src_is_stub && src_sessions <= 1 => {
            tx.execute("update speaker set name = ?1, isStub = 0 where id = ?2", rusqlite::params![name, src])?;
            (src.clone(), 0, 0)
        }
        other => {
            let to = match other {
                Some(t) => t,
                None => {
                    // a person wrongly named in this recording: a new person for this one
                    let id = new_id();
                    let color: Option<String> = tx.query_row("select color from speaker where id = ?1", [&src], |r| r.get(0)).ok().flatten();
                    tx.execute("insert into speaker (id, name, color, isStub) values (?1, ?2, ?3, 0)", rusqlite::params![id, name, color])?;
                    id
                }
            };
            let lines = tx.execute("update transcriptline set speakerID = ?1 where sessionId = ?2 and speakerID = ?3",
                                   rusqlite::params![to, sess, src])?;
            tx.execute("insert or ignore into session_speaker (sessionID, speakerID, isRecognized) values (?1, ?2, 0)",
                       rusqlite::params![sess, to])?;
            tx.execute("delete from session_speaker where sessionID = ?1 and speakerID = ?2", rusqlite::params![sess, src])?;
            let vps = tx.execute("update speakervoiceprint set speakerID = ?1 where sessionID = ?2 and speakerID = ?3",
                                 rusqlite::params![to, sess, src])?;
            (to, lines, vps)
        }
    };
    let has_vp: bool = tx.query_row("select count(*) from speakervoiceprint where sessionID = ?1 and speakerID = ?2",
                                    rusqlite::params![sess, to_id], |r| r.get::<_, i64>(0))? > 0;
    let mut added = None;
    if !has_vp {
        if let Some((_, vec, secs)) = &voice {
            let model: String = tx.query_row("select modelID from speakervoiceprint order by dateCreated desc limit 1", [], |r| r.get(0))
                .unwrap_or_else(|_| DEFAULT_MODEL.to_string());
            let blob: Vec<u8> = vec.iter().flat_map(|x| (*x as f32).to_le_bytes()).collect();
            let id = new_id();
            let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
            tx.execute("insert into speakervoiceprint (id, sessionID, speakerID, modelID, embedding, speechDuration, dateCreated)
                        values (?1, ?2, ?3, ?4, ?5, ?6, ?7)", rusqlite::params![id, sess, to_id, model, blob, secs, now])?;
            added = Some(hex(&id));
        }
    }
    tx.commit()?;

    let record = json!({
        "at": common::now_iso(), "transcript": transcript, "session": session_hex,
        "from": {"id": speaker_hex, "name": from, "stub": src_is_stub},
        "to": {"id": hex(&to_id), "name": name, "new": new_person},
        "moved_lines": moved_lines, "moved_voiceprints": moved_vps, "added_voiceprint": added,
    });
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(writes_log()) {
        let _ = writeln!(f, "{record}");
    }
    log(&format!("{transcript}: '{from}' named '{name}' in MacWhisper{}{}", if new_person { " (new person)" } else { "" },
                 if added.is_some() { "; voiceprint added" } else { "" }));
    Ok(Renamed { from, to: name, new_person, voiceprint_added: added.is_some() })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const SCHEMA: &str = r#"
        CREATE TABLE "session" ("id" BLOB PRIMARY KEY NOT NULL, "originalFilename" TEXT, "dateDeleted" DOUBLE, "hasBeenDiarized" BOOLEAN, "isTransient" BOOLEAN, "isFromWatchFolder" BOOLEAN, "dateCreated" DATETIME);
        CREATE TABLE "mediafile" ("sessionID" BLOB, "filename" TEXT, "type" TEXT, "dateCreated" DATETIME);
        CREATE TABLE "speaker" ("id" BLOB PRIMARY KEY NOT NULL, "name" TEXT NOT NULL, "color" TEXT, "isStub" BOOLEAN NOT NULL DEFAULT 0, "photoData" BLOB);
        CREATE TABLE "session_speaker" ("sessionID" BLOB NOT NULL, "speakerID" BLOB NOT NULL, "isRecognized" BOOLEAN NOT NULL DEFAULT 0, PRIMARY KEY ("sessionID", "speakerID"));
        CREATE TABLE "speakervoiceprint" ("id" BLOB PRIMARY KEY NOT NULL, "sessionID" BLOB NOT NULL, "speakerID" BLOB NOT NULL, "modelID" TEXT NOT NULL, "embedding" BLOB NOT NULL, "speechDuration" DOUBLE NOT NULL, "dateCreated" DATETIME NOT NULL);
        CREATE TABLE "transcriptline" ("id" BLOB PRIMARY KEY NOT NULL, "text" TEXT NOT NULL, "start" INTEGER NOT NULL, "end" INTEGER NOT NULL, "sessionId" BLOB NOT NULL, "speakerID" BLOB, "wordsJson" TEXT, "orderIndex" INTEGER);
        INSERT INTO session VALUES (X'01', 'rec', NULL, 1, 0, 1, '2026-11-24');
        INSERT INTO session VALUES (X'02', 'other', NULL, 1, 0, 1, '2026-11-24');
        INSERT INTO speaker VALUES (X'A1', 'Speaker 1', 'D46F6F', 1, NULL), (X'A2', 'Speaker 2', '6FA9D4', 1, NULL),
                                   (X'B1', 'Ann Lee', '67B9AA', 0, NULL);
        INSERT INTO session_speaker VALUES (X'01', X'A1', 0), (X'01', X'A2', 0), (X'02', X'B1', 0);
        INSERT INTO speakervoiceprint VALUES (X'C1', X'01', X'A1', 'speakerkit-pyannote-2.0.9-trainable-centroid', zeroblob(1024), 5.0, '2026-11-24 08:00:00.000');
        INSERT INTO transcriptline VALUES (X'D1', 'hi', 0, 1000, X'01', X'A1', NULL, 0), (X'D2', 'yo', 1000, 2000, X'01', X'A2', NULL, 1),
                                          (X'D3', 'hey', 2000, 3000, X'01', X'A1', NULL, 2);
    "#;

    fn q(con: &Connection, sql: &str) -> Vec<String> {
        let mut st = con.prepare(sql).unwrap();
        st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|x| x.unwrap()).collect()
    }

    #[test]
    fn renames_like_macwhisper() {
        let _g = crate::TEST_ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("mw-write-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("root/speaker-matching")).unwrap();
        let db = dir.join("main.sqlite");
        Connection::open(&db).unwrap().execute_batch(SCHEMA).unwrap();
        std::env::set_var("RECORDINGS_MW_DB", &db);
        std::env::set_var("RECORDINGS_ROOT", dir.join("root"));

        assert_eq!(known_people().unwrap(), vec!["Ann Lee"]);
        // "Speaker 1" -> an existing person: its lines, link and voiceprint move to her; the stub stays, unlinked
        let r = rename_speaker("01", "A1", "Ann  Lee", "rec.txt").unwrap();
        assert!(!r.new_person && r.to == "Ann Lee");
        let con = Connection::open(&db).unwrap();
        assert_eq!(q(&con, "select hex(speakerID) from transcriptline where sessionId = X'01' order by orderIndex"), ["B1", "A2", "B1"]);
        assert_eq!(q(&con, "select hex(speakerID) from session_speaker where sessionID = X'01' order by 1"), ["A2", "B1"]);
        assert_eq!(q(&con, "select hex(speakerID) from speakervoiceprint"), ["B1"]);
        assert_eq!(q(&con, "select name from speaker where id = X'A1'"), ["Speaker 1"]);
        // "Speaker 2" -> a new person: the stub becomes that person
        let r = rename_speaker("01", "A2", "Bob", "rec.txt").unwrap();
        assert!(r.new_person);
        assert_eq!(q(&con, "select name || ':' || isStub from speaker where id = X'A2'"), ["Bob:0"]);
        // refused: not a name, same name, a speaker not in the recording
        assert!(rename_speaker("01", "A2", "Speaker 3", "rec.txt").is_err());
        assert!(rename_speaker("01", "A2", "Bob", "rec.txt").is_err());
        assert!(rename_speaker("01", "A1", "Dave", "rec.txt").is_err());
        // a changed database is not written
        con.execute_batch("ALTER TABLE speaker RENAME COLUMN isStub TO isPlaceholder").unwrap();
        let e = rename_speaker("01", "A2", "Dan", "rec.txt").err().unwrap().to_string();
        assert!(e.contains("speaker.isStub is missing"), "{e}");
        // log and backup
        let log = fs::read_to_string(dir.join("root/speaker-matching/macwhisper-writes.jsonl")).unwrap();
        assert_eq!(log.lines().count(), 2);
        assert_eq!(entries(&dir.join("root/speaker-matching/macwhisper-backups")).len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
