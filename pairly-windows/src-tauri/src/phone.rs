//! What the window does with a phone: its texts, contacts and calls, and its files.

use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use pairly_plugins::contacts::Contact;
use pairly_plugins::files::{CHUNK, Entry, FileOp};
use pairly_plugins::sms::{ATTACHMENT_CHUNK, Conversation, Message, OutgoingAttachment};
use pairly_plugins::telephony::CallAction;
use serde::Serialize;
use tauri::Emitter;

use crate::backend::{Backend, data_url, parse, part_path};

/// Picture messages' attachments are fetched only up to this size.
const MAX_ATTACHMENT: u64 = 20 * 1024 * 1024;

#[derive(Clone, Serialize)]
struct FilesProgress<'a> {
    device: &'a str,
    path: &'a str,
    done: u64,
    total: u64,
    upload: bool,
}

/// What a fetched attachment became: shown in the page, or saved and opened.
#[derive(Serialize)]
pub struct AttachmentView {
    /// A `data:` URL for pictures.
    pub url: Option<String>,
    /// Where it was saved, for anything else.
    pub path: Option<String>,
}

impl Backend {
    // ----- texts ---------------------------------------------------------------------------

    pub async fn conversations(&self, id: &str) -> Result<Vec<Conversation>> {
        Ok(self.features()?.sms.conversations(parse(id)?).await?)
    }

    pub async fn messages(&self, id: &str, thread: i64, before_ms: i64) -> Result<Vec<Message>> {
        let before = (before_ms > 0).then_some(before_ms);
        Ok(self
            .features()?
            .sms
            .messages(parse(id)?, thread, before, 50)
            .await?)
    }

    /// Send a text (a picture message when there are files or several people).
    pub fn send_sms(
        &self,
        id: &str,
        addresses: Vec<String>,
        text: &str,
        files: &[String],
    ) -> Result<()> {
        let mut attachments = Vec::new();
        for path in files {
            let path = Path::new(path);
            let size = std::fs::metadata(path)
                .with_context(|| format!("can't read {}", path.display()))?
                .len();
            anyhow::ensure!(
                size <= u64::from(ATTACHMENT_CHUNK),
                "{} is too big for a picture message (at most {} KB)",
                path.file_name().unwrap_or_default().to_string_lossy(),
                ATTACHMENT_CHUNK / 1024
            );
            attachments.push(OutgoingAttachment {
                mime: mime_for(path).to_owned(),
                name: path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                data: std::fs::read(path)?,
            });
        }
        let addresses = addresses
            .into_iter()
            .map(|a| a.trim().to_owned())
            .filter(|a| !a.is_empty())
            .collect();
        Ok(self
            .features()?
            .sms
            .send(parse(id)?, addresses, text, attachments)?)
    }

    /// A picture message's attachment: pictures come back to show; other files are saved in
    /// the received-files folder and opened.
    pub async fn sms_attachment(
        &self,
        id: &str,
        part_id: i64,
        name: &str,
        mime: &str,
    ) -> Result<AttachmentView> {
        let peer = parse(id)?;
        let dir = self.cache_dir.join("mms");
        let file = dir.join(format!(
            "{peer}-{part_id}-{}",
            pairly_plugins::share::safe_file_name(name)
        ));
        let data = if let Ok(data) = std::fs::read(&file) {
            data
        } else {
            let data = self
                .features()?
                .sms
                .attachment(peer, part_id, MAX_ATTACHMENT)
                .await?;
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&file, &data)?;
            data
        };
        if mime.starts_with("image/") {
            return Ok(AttachmentView {
                url: Some(data_url(&data)),
                path: None,
            });
        }
        std::fs::create_dir_all(&self.downloads)?;
        let name = if name.is_empty() { "attachment" } else { name };
        let target = Self::free_name(&self.downloads, name);
        std::fs::write(&target, &data)?;
        crate::platform::open(&target.to_string_lossy());
        Ok(AttachmentView {
            url: None,
            path: Some(target.display().to_string()),
        })
    }

    // ----- contacts and calls --------------------------------------------------------------

    pub async fn contacts(&self, id: &str) -> Result<Vec<Contact>> {
        Ok(self.features()?.contacts.fetch(parse(id)?).await?)
    }

    pub fn dial(&self, id: &str, number: &str) -> Result<()> {
        let number = number.trim();
        anyhow::ensure!(!number.is_empty(), "no number to call");
        Ok(self.features()?.telephony.dial(parse(id)?, number)?)
    }

    /// Act on the phone's call: `answer`, `speaker`, `reject` or `hangup`.
    pub fn call_action(&self, id: &str, action: &str) -> Result<()> {
        let action = match action {
            "answer" => CallAction::Answer,
            "speaker" => CallAction::AnswerOnSpeaker,
            "reject" => CallAction::Reject,
            "hangup" => CallAction::HangUp,
            other => anyhow::bail!("unknown call action {other}"),
        };
        Ok(self.features()?.telephony.control(parse(id)?, action)?)
    }

    // ----- the phone's files ---------------------------------------------------------------

    pub async fn files_list(&self, id: &str, path: &str) -> Result<Vec<Entry>> {
        let mut list = self.features()?.files.list(parse(id)?, path).await?;
        // Folders first, then by name.
        list.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        Ok(list)
    }

    /// Copy a device's file into the received-files folder; returns where it went.
    pub async fn files_download(&self, id: &str, path: &str, size: u64) -> Result<String> {
        let peer = parse(id)?;
        let files = self.features()?.files;
        let name = path.rsplit('/').next().unwrap_or("file");
        std::fs::create_dir_all(&self.downloads)?;
        let target = Self::free_name(&self.downloads, name);
        let part = part_path(&target);
        let result = async {
            let mut out = std::fs::File::create(&part)?;
            files
                .download(
                    peer,
                    path,
                    size,
                    |data| out.write_all(data),
                    |done| self.files_progress(id, path, done, size.max(done), false),
                )
                .await?;
            out.sync_all()?;
            drop(out);
            std::fs::rename(&part, &target)?;
            anyhow::Ok(())
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_file(&part);
        }
        result?;
        Ok(target.display().to_string())
    }

    /// Copy local files into a folder on the phone.
    pub async fn files_upload(
        &self,
        id: &str,
        local: &[String],
        remote_dir: &str,
    ) -> Result<usize> {
        let peer = parse(id)?;
        let files = self.features()?.files;
        let mut sent = 0;
        for local in local {
            let local = Path::new(local);
            if !local.is_file() {
                continue;
            }
            let mut file = std::fs::File::open(local)?;
            let size = file.metadata()?.len();
            let name = local
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .context("not a file")?;
            let dir = remote_dir.trim_matches('/');
            let remote = if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            };
            let mut offset = 0u64;
            loop {
                let mut buf = vec![0u8; CHUNK as usize];
                let n = read_full(&mut file, &mut buf)?;
                buf.truncate(n);
                if n == 0 && offset > 0 {
                    break;
                }
                let op = FileOp::Write {
                    path: remote.clone(),
                    offset,
                    data: buf,
                    create: offset == 0,
                };
                files.ask(peer, op).await?;
                offset += n as u64;
                self.files_progress(id, &remote, offset, size, true);
                if n == 0 {
                    break;
                }
            }
            sent += 1;
        }
        Ok(sent)
    }

    pub async fn files_op(&self, id: &str, op: &str, path: &str, to: &str) -> Result<()> {
        let op = match op {
            "delete" => FileOp::Delete {
                path: path.to_owned(),
            },
            "mkdir" => FileOp::Mkdir {
                path: path.to_owned(),
            },
            "rename" => FileOp::Rename {
                from: path.to_owned(),
                to: to.to_owned(),
            },
            other => anyhow::bail!("unknown file operation {other}"),
        };
        self.features()?.files.ask(parse(id)?, op).await?;
        Ok(())
    }

    fn files_progress(&self, device: &str, path: &str, done: u64, total: u64, upload: bool) {
        let _ = self.app.emit_to(
            "main",
            "files-progress",
            FilesProgress {
                device,
                path,
                done,
                total,
                upload,
            },
        );
    }
}

/// Fill `buf` as far as the file goes.
fn read_full(file: &mut std::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match file.read(&mut buf[n..])? {
            0 => break,
            got => n += got,
        }
    }
    Ok(n)
}

/// MIME type for a file to send in a picture message.
pub fn mime_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "amr" => "audio/amr",
        "vcf" => "text/x-vcard",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}
