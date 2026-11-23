use anyhow::{Context, Result};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{watch, Semaphore},
    time::timeout,
};

const LINE: usize = 1000;
const MAX: usize = 1024 * 1024;
const MAX_MESSAGES: usize = 100;
const MAX_DATA: usize = 65536;

static STORE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Account {
    pub name: String,
    pub password: String,
    pub no_reply: bool,
    pub created_at: String,
    #[serde(default)]
    pub messages: Vec<StoredMessage>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct StoredMessage {
    pub id: String,
    pub from: String,
    pub to: Vec<String>,
    pub received_at: String,
    pub data: String,
}

#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct MailStore {
    #[serde(default)]
    pub accounts: Vec<Account>,
}

fn current_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

fn extract_username(recipient: &str) -> String {
    let clean = recipient.trim().trim_start_matches('<').trim_end_matches('>');
    if let Some((user, _domain)) = clean.split_once('@') {
        user.trim().to_string()
    } else {
        clean.trim().to_string()
    }
}

impl MailStore {
    pub fn load_from_dir(dir: &Path) -> Self {
        let path = dir.join("accounts.json");
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(store) = serde_json::from_str::<Self>(&data) {
                return store;
            }
        }
        Self::default()
    }

    pub fn save_to_dir(&self, dir: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let path = dir.join("accounts.json");
        let content = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".accounts-")
            .tempfile_in(dir)?;
        tmp.write_all(content.as_bytes())?;
        tmp.as_file().sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::remove_file(&path);
        tmp.persist(&path).map_err(|e| e.error)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn add_or_update(&mut self, name: &str, password: &str, no_reply: bool) {
        let now = current_timestamp();
        if let Some(acc) = self.accounts.iter_mut().find(|a| a.name.eq_ignore_ascii_case(name)) {
            acc.password = password.to_string();
            acc.no_reply = no_reply;
        } else {
            self.accounts.push(Account {
                name: name.to_string(),
                password: password.to_string(),
                no_reply,
                created_at: now,
                messages: Vec::new(),
            });
        }
    }

    pub fn record_message(&mut self, from: &str, to: &[String], data: &[u8]) {
        let now = current_timestamp();
        let hash = blake3::hash(data).to_hex().to_string();
        let body = if data.len() > MAX_DATA {
            String::from_utf8_lossy(&data[..MAX_DATA]).to_string()
        } else {
            String::from_utf8_lossy(data).to_string()
        };
        let msg = StoredMessage {
            id: hash,
            from: from.to_string(),
            to: to.to_vec(),
            received_at: now.clone(),
            data: body,
        };
        let mut matched = false;
        for recipient in to {
            let user = extract_username(recipient);
            if let Some(acc) = self.accounts.iter_mut().find(|a| a.name.eq_ignore_ascii_case(&user)) {
                acc.messages.push(msg.clone());
                if acc.messages.len() > MAX_MESSAGES {
                    acc.messages.remove(0);
                }
                matched = true;
            }
        }
        if !matched {
            let default_name = to.first().map(|r| extract_username(r)).unwrap_or_else(|| "inbox".to_string());
            if let Some(acc) = self.accounts.iter_mut().find(|a| a.name.eq_ignore_ascii_case(&default_name)) {
                acc.messages.push(msg);
                if acc.messages.len() > MAX_MESSAGES {
                    acc.messages.remove(0);
                }
            } else {
                self.accounts.push(Account {
                    name: default_name,
                    password: String::new(),
                    no_reply: false,
                    created_at: now,
                    messages: vec![msg],
                });
            }
        }
    }
}

pub fn save_account(dir: &Path, name: &str, password: &str, no_reply: bool) -> io::Result<()> {
    let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    std::fs::create_dir_all(dir)?;
    let mut store = MailStore::load_from_dir(dir);
    store.add_or_update(name, password, no_reply);
    store.save_to_dir(dir)
}

async fn line<R: AsyncBufRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> io::Result<bool> {
    buf.clear();
    let n = timeout(
        Duration::from_secs(30),
        (&mut *r).take(LINE as u64 + 1).read_until(b'\n', buf),
    )
    .await
    .map_err(io::Error::other)??;
    if n == 0 {
        return Ok(false);
    }
    if n > LINE || !buf.ends_with(b"\r\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid SMTP line",
        ));
    }
    Ok(true)
}

async fn persist(
    dir: Arc<PathBuf>,
    data: Vec<u8>,
    from: String,
    to: Vec<String>,
) -> io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let mut tmp = tempfile::Builder::new()
            .prefix(".incoming-")
            .tempfile_in(&*dir)?;
        write!(tmp, "X-Eafrei-Envelope-From: {from}\r\n")?;
        for recipient in &to {
            write!(tmp, "X-Eafrei-Envelope-To: {recipient}\r\n")?;
        }
        tmp.write_all(&data)?;
        tmp.as_file().sync_all()?;
        let name = tmp
            .path()
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_prefix(".incoming-"))
            .ok_or_else(|| io::Error::other("invalid spool name"))?;
        let dest = dir.join(format!("{name}.eml"));
        tmp.persist_noclobber(dest).map_err(|e| e.error)?;
        #[cfg(unix)]
        std::fs::File::open(&*dir)?.sync_all()?;

        let _guard = STORE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut store = MailStore::load_from_dir(&dir);
        store.record_message(&from, &to, &data);
        let _ = store.save_to_dir(&dir);

        Ok(())
    })
    .await
    .map_err(io::Error::other)?
}

fn mailbox(arg: &str, null_ok: bool) -> bool {
    arg.starts_with('<')
        && arg.ends_with('>')
        && (null_ok || arg.len() > 2)
        && arg[1..arg.len() - 1]
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'<' | b'>'))
}

async fn session(sock: TcpStream, dir: Arc<PathBuf>) -> io::Result<()> {
    let (r, mut w) = sock.into_split();
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    let mut hello = false;
    let mut from = None::<String>;
    let mut to = Vec::<String>::new();
    w.write_all(b"220 eafrei local SMTP inbox\r\n").await?;
    while line(&mut r, &mut buf).await? {
        let Ok(cmd) = std::str::from_utf8(&buf[..buf.len() - 2]) else {
            w.write_all(b"500 Invalid command\r\n").await?;
            continue;
        };
        let upper = cmd.to_ascii_uppercase();
        let reply: &[u8] = if upper == "QUIT" {
            w.write_all(b"221 Bye\r\n").await?;
            return Ok(());
        } else if upper.starts_with("EHLO ") || upper.starts_with("HELO ") {
            hello = true;
            from = None;
            to.clear();
            b"250 eafrei\r\n"
        } else if upper == "NOOP" {
            b"250 OK\r\n"
        } else if upper == "RSET" {
            from = None;
            to.clear();
            b"250 OK\r\n"
        } else if !hello {
            b"503 Send EHLO first\r\n"
        } else if upper.starts_with("MAIL FROM:") {
            let v = cmd[10..].trim();
            if from.is_some() {
                b"503 Already in transaction\r\n"
            } else if mailbox(v, true) {
                from = Some(v.into());
                b"250 OK\r\n"
            } else {
                b"501 Invalid sender\r\n"
            }
        } else if upper.starts_with("RCPT TO:") {
            let v = cmd[8..].trim();
            if from.is_none() {
                b"503 Send MAIL first\r\n"
            } else if to.len() >= 100 {
                b"452 Too many recipients\r\n"
            } else if mailbox(v, false) {
                to.push(v.into());
                b"250 OK\r\n"
            } else {
                b"501 Invalid recipient\r\n"
            }
        } else if upper == "DATA" {
            if to.is_empty() {
                b"503 Missing recipient\r\n"
            } else {
                w.write_all(b"354 End with a dot on its own line\r\n")
                    .await?;
                let mut data = Vec::new();
                loop {
                    if !line(&mut r, &mut buf).await? {
                        return Ok(());
                    }
                    if buf == b".\r\n" {
                        break;
                    }
                    let text = buf.strip_prefix(b".").unwrap_or(&buf);
                    if data.len() + text.len() > MAX {
                        w.write_all(b"552 Message too large\r\n").await?;
                        return Ok(());
                    }
                    data.extend_from_slice(text);
                }
                let sender = from.take().unwrap_or_default();
                let recipients = std::mem::take(&mut to);
                if persist(dir.clone(), data, sender, recipients).await.is_ok() {
                    b"250 Stored\r\n"
                } else {
                    b"451 Cannot store mail\r\n"
                }
            }
        } else {
            b"502 Command not implemented\r\n"
        };
        w.write_all(reply).await?;
    }
    Ok(())
}

pub async fn serve(
    listener: TcpListener,
    path: &Path,
    mut stop: watch::Receiver<bool>,
) -> Result<()> {
    let dir = Arc::new(path.to_owned());
    let slots = Arc::new(Semaphore::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {biased;
            _=stop.changed()=>break,
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>(),
            accepted=listener.accept()=>{let (mut sock,_)=accepted.context("accept SMTP")?;if let Ok(slot)=slots.clone().try_acquire_owned(){let d=dir.clone();tasks.spawn(async move{let _permit=slot;let _=timeout(Duration::from_secs(300),session(sock,d)).await;});}else{let _=sock.write_all(b"421 Try later\r\n").await;}}
        }
    }
    tasks.shutdown().await;
    Ok(())
}
