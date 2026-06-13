//! M8 — media import (yt-dlp) and optional karaoke split (demucs).
//!
//! Every function here BLOCKS (spawns a child process, network or CPU heavy) and
//! is therefore meant to run on a worker thread — NEVER on the egui UI thread.
//! Both tools are external CLIs found on `PATH`; when one is missing we report it
//! gracefully (an `*_available()` probe + a clear error) instead of panicking, so
//! the GUI can disable the affected button with a hint. The pure path helpers
//! ([`import_dir`], [`find_no_vocals`]) are unit-tested without touching the
//! network or demucs.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Scratch directory for downloads + stems, under the OS temp dir. Deterministic
/// so a re-download of the same id overwrites instead of piling up.
pub fn import_dir() -> PathBuf {
    std::env::temp_dir().join("livehub_import")
}

/// True when `yt-dlp` is on `PATH` and answers `--version`.
pub fn yt_dlp_available() -> bool {
    Command::new("yt-dlp")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Download `url` into `out_dir` as a ≤720p mp4 (audio+video muxed) via yt-dlp and
/// return the final file path. The mp4 feeds BOTH the audio deck and the
/// `VideoDeck`, so one download covers picture and sound.
///
/// We let yt-dlp print the post-merge path itself (`--print after_move:filepath`),
/// which is robust to the `%(id)s`/`%(ext)s` template and any container remux —
/// far safer than reconstructing the name ourselves. The path is the last
/// non-empty stdout line.
pub fn yt_dlp_download(url: &str, out_dir: &Path) -> Result<PathBuf> {
    if !yt_dlp_available() {
        bail!("yt-dlp not found on PATH — install with: uv tool install yt-dlp");
    }
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("create import dir {}", out_dir.display()))?;

    let template = out_dir.join("livehub_%(id)s.%(ext)s");
    let output = Command::new("yt-dlp")
        .arg(url)
        .args([
            "-f",
            "bv*[height<=720]+ba/b[height<=720]/b",
            "--merge-output-format",
            "mp4",
            "--no-playlist",
            "--no-part",
            "-o",
        ])
        .arg(&template)
        .args(["--print", "after_move:filepath"])
        .output()
        .context("spawn yt-dlp")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("yt-dlp failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "yt-dlp printed no output path (stderr: {})",
                String::from_utf8_lossy(&output.stderr).trim()
            )
        })?;

    if !path.exists() {
        bail!("yt-dlp reported '{}' but it does not exist", path.display());
    }
    Ok(path)
}

/// True when `demucs` is on `PATH` and answers `-h`.
pub fn demucs_available() -> bool {
    Command::new("demucs")
        .arg("-h")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Split `input` into vocals + instrumental with demucs (`--two-stems=vocals`,
/// htdemucs default) under `out_dir`, and return the INSTRUMENTAL stem
/// (`no_vocals.wav`) for the karaoke deck. demucs nests the output under a
/// model-named directory (`<out_dir>/htdemucs/<stem>/no_vocals.wav`), so we scan
/// `out_dir` recursively for the newest `no_vocals.wav` rather than hard-coding
/// the model dir — robust if the user's default model changes.
pub fn demucs_split(input: &Path, out_dir: &Path) -> Result<PathBuf> {
    if !demucs_available() {
        bail!("demucs not installed — uv tool install demucs");
    }
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("create import dir {}", out_dir.display()))?;

    let status = Command::new("demucs")
        .arg("--two-stems=vocals")
        .arg("-o")
        .arg(out_dir)
        .arg(input)
        .status()
        .context("spawn demucs")?;

    if !status.success() {
        bail!("demucs exited with failure (status {status})");
    }

    find_no_vocals(out_dir)
        .with_context(|| format!("demucs produced no no_vocals.wav under {}", out_dir.display()))
}

/// Find the newest `no_vocals.wav` anywhere under `root` (recursive). Returns
/// `None` if there is none. Pure file-system walk — no demucs, no network — so it
/// is unit-testable against a fake tree.
fn find_no_vocals(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut consider = |p: &Path| {
        if p.file_name().is_some_and(|n| n == "no_vocals.wav") {
            let mtime = p
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(t, _)| mtime >= *t) {
                best = Some((mtime, p.to_path_buf()));
            }
        }
    };
    walk(root, &mut consider);
    best.map(|(_, p)| p)
}

/// Depth-first directory walk, calling `f` on every file encountered. Silently
/// skips unreadable entries (best-effort scan of demucs output).
fn walk(dir: &Path, f: &mut impl FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, f);
        } else {
            f(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_dir_under_temp() {
        let d = import_dir();
        assert!(d.starts_with(std::env::temp_dir()));
        assert_eq!(d.file_name().and_then(|n| n.to_str()), Some("livehub_import"));
    }

    #[test]
    fn find_no_vocals_in_nested_tree() {
        // Build a fake demucs-style tree: <root>/htdemucs/<stem>/no_vocals.wav.
        let root = std::env::temp_dir().join(format!("livehub_test_{}", std::process::id()));
        let stem = root.join("htdemucs").join("song");
        std::fs::create_dir_all(&stem).unwrap();
        let target = stem.join("no_vocals.wav");
        std::fs::write(&target, b"riff").unwrap();
        std::fs::write(stem.join("vocals.wav"), b"riff").unwrap();

        let found = find_no_vocals(&root);
        assert_eq!(found.as_deref(), Some(target.as_path()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn find_no_vocals_absent() {
        let root = std::env::temp_dir().join(format!("livehub_empty_{}", std::process::id()));
        let sub = root.join("htdemucs").join("song");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("vocals.wav"), b"riff").unwrap();
        std::fs::write(sub.join("drums.wav"), b"riff").unwrap();

        assert_eq!(find_no_vocals(&root), None);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn find_no_vocals_only_matches_exact_name() {
        // A similarly-named file must NOT be picked — only `no_vocals.wav`.
        let root = std::env::temp_dir().join(format!("livehub_name_{}", std::process::id()));
        let sub = root.join("htdemucs").join("song");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("no_vocals_backup.wav"), b"riff").unwrap();
        let real = sub.join("no_vocals.wav");
        std::fs::write(&real, b"riff").unwrap();

        assert_eq!(find_no_vocals(&root).as_deref(), Some(real.as_path()));

        std::fs::remove_dir_all(&root).ok();
    }
}
