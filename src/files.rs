//! Image attachments: screenshots and mock-ups posted with a message.
//!
//! Files are copied into the board (`<root>/files/<ulid>.<ext>`) so every
//! machine that reads the board can show them, and the message records them in
//! `meta.attachments` as `{file, type, name, bytes}`. Only images are accepted,
//! recognised by their contents rather than their name, and only from the
//! machine that holds the board: a remote agent cannot make the server read
//! its own disk.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

/// Largest file accepted (a full-screen Retina PNG is usually 2–6 MB).
pub const MAX_BYTES: u64 = 10 * 1024 * 1024;
/// At most this many files on one message.
pub const MAX_FILES: usize = 8;

pub fn dir(root: &Path) -> PathBuf {
    root.join("files")
}

/// The image type from the file's first bytes.
fn sniff(head: &[u8]) -> Option<(&'static str, &'static str)> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("png", "image/png"))
    } else if head.starts_with(b"\xff\xd8\xff") {
        Some(("jpg", "image/jpeg"))
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some(("gif", "image/gif"))
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some(("webp", "image/webp"))
    } else {
        None
    }
}

fn content_type(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// A stored file name: a ULID and one of our extensions, nothing else (so a
/// request can never name a path outside the folder).
pub fn valid_name(name: &str) -> bool {
    let Some((stem, ext)) = name.split_once('.') else { return false };
    stem.len() == 26 && stem.bytes().all(|b| b.is_ascii_alphanumeric()) && content_type(ext).is_some()
}

/// Copy an image into the board and describe it for `meta.attachments`.
pub fn store(root: &Path, source: &Path) -> Result<Value> {
    let size = fs::metadata(source)
        .with_context(|| format!("read {}", source.display()))?
        .len();
    if size > MAX_BYTES {
        bail!("{} is {} MB; attachments must be at most {} MB", source.display(), size / 1_000_000, MAX_BYTES / 1_000_000);
    }
    let mut bytes = Vec::with_capacity(size as usize);
    fs::File::open(source)
        .with_context(|| format!("open {}", source.display()))?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    store_bytes(root, &bytes, &name)
        .with_context(|| format!("attach {}", source.display()))
}

/// Store image bytes (an upload from the chat page, or a file read above).
pub fn store_bytes(root: &Path, bytes: &[u8], name: &str) -> Result<Value> {
    if bytes.len() as u64 > MAX_BYTES {
        bail!("attachments must be at most {} MB", MAX_BYTES / 1_000_000);
    }
    let Some((ext, kind)) = sniff(bytes) else {
        bail!("not a PNG, JPEG, GIF or WebP image");
    };
    let folder = dir(root);
    fs::create_dir_all(&folder).context("create the board's files folder")?;
    let file = format!("{}.{ext}", ulid::Ulid::new().to_string().to_ascii_lowercase());
    let temp = folder.join(format!(".{file}.tmp"));
    fs::write(&temp, bytes).context("write attachment")?;
    fs::rename(&temp, folder.join(&file)).context("save attachment")?;
    Ok(describe(&file, kind, name, bytes.len()))
}

fn describe(file: &str, kind: &str, name: &str, bytes: usize) -> Value {
    // Keep the original name readable but harmless: no paths, short.
    let name: String = name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    let name = if name.trim().is_empty() { file.to_owned() } else { name };
    json!({"file": file, "type": kind, "name": name, "bytes": bytes})
}

/// Re-check attachments a client claims it uploaded: each must name a file
/// already stored in this board. The description is rebuilt from the file.
pub fn verify(root: &Path, claimed: &[Value]) -> Result<Vec<Value>> {
    if claimed.len() > MAX_FILES {
        bail!("at most {MAX_FILES} attachments per message");
    }
    claimed
        .iter()
        .map(|item| {
            let file = item["file"].as_str().unwrap_or_default();
            let Some((bytes, kind)) = read(root, file) else {
                bail!("attachment {file} is not on this board");
            };
            Ok(describe(file, kind, item["name"].as_str().unwrap_or(file), bytes.len()))
        })
        .collect()
}

/// Store every file and add them to the message's metadata.
pub fn attach(root: &Path, sources: &[String], meta: Option<Value>) -> Result<Option<Value>> {
    if sources.is_empty() {
        return Ok(meta);
    }
    if sources.len() > MAX_FILES {
        bail!("at most {MAX_FILES} attachments per message");
    }
    let stored = sources
        .iter()
        .map(|s| store(root, Path::new(s)))
        .collect::<Result<Vec<_>>>()?;
    with_attachments(meta, stored).map(Some)
}

/// Add stored attachments to a message's metadata.
pub fn with_attachments(meta: Option<Value>, stored: Vec<Value>) -> Result<Value> {
    let mut meta = meta.unwrap_or_else(|| json!({}));
    let Some(map) = meta.as_object_mut() else { bail!("meta must be a JSON object to add attachments") };
    map.insert("attachments".into(), Value::Array(stored));
    Ok(meta)
}

/// A stored file's bytes and content type, for the chat page.
pub fn read(root: &Path, name: &str) -> Option<(Vec<u8>, &'static str)> {
    if !valid_name(name) {
        return None;
    }
    let kind = content_type(name.rsplit_once('.')?.1)?;
    let bytes = fs::read(dir(root).join(name)).ok()?;
    // Serve it only if it still is the image its name says.
    (sniff(&bytes)?.1 == kind).then_some((bytes, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_images_and_refuses_everything_else() {
        let board = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let png = src.path().join("shot.png");
        fs::write(&png, b"\x89PNG\r\n\x1a\nrest").unwrap();
        let meta = attach(board.path(), &[png.to_string_lossy().into()], Some(json!({"x": 1}))).unwrap().unwrap();
        let a = &meta["attachments"][0];
        assert_eq!((a["type"].as_str(), a["name"].as_str(), meta["x"].as_i64()), (Some("image/png"), Some("shot.png"), Some(1)));
        let file = a["file"].as_str().unwrap();
        assert!(valid_name(file));
        assert_eq!(read(board.path(), file).unwrap().1, "image/png");

        let text = src.path().join("notes.png");
        fs::write(&text, b"not an image").unwrap();
        assert!(store(board.path(), &text).is_err());
        for bad in ["../secret.png", "x.png", "01m6tmkvr4tq35fkgh2nf2295z.html", "01m6tmkvr4tq35fkgh2nf2295z"] {
            assert!(!valid_name(bad), "{bad}");
        }

        // Uploads: names are cleaned, claims are checked against the board.
        let up = store_bytes(board.path(), b"\xff\xd8\xffjpeg", "../../etc/My Shot.jpg").unwrap();
        assert_eq!(up["name"], "My Shot.jpg");
        assert_eq!(up["type"], "image/jpeg");
        let ok = verify(board.path(), &[json!({"file": up["file"], "name": "renamed.jpg", "bytes": 1})]).unwrap();
        assert_eq!((ok[0]["bytes"].as_u64(), ok[0]["name"].as_str()), (Some(7), Some("renamed.jpg")));
        assert!(verify(board.path(), &[json!({"file": "01m6tmkvr4tq35fkgh2nf2295z.png"})]).is_err());
        assert!(store_bytes(board.path(), b"<svg onload=alert(1)>", "x.svg").is_err());
    }
}
