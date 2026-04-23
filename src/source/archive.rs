//! GitHub archive download and extraction.

use std::fs;
use std::io::{self, Cursor};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::error::MarsError;
use crate::platform::cache::archive_cache_component;
use crate::platform::fs::{publish_cache_dir_if_absent, safe_remove};
use crate::source::GlobalCache;
use flate2::read::GzDecoder;
use tar::Archive;

pub(crate) fn github_owner_repo(url: &str) -> Option<(String, String)> {
    let (_, tail) = url.split_once("github.com/")?;
    let mut segments = tail.split('/');
    let owner = segments.next()?.trim();
    let repo = segments.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    Some((owner.to_string(), repo.to_string()))
}

pub(crate) fn download_archive_bytes(archive_url: &str) -> Result<Vec<u8>, MarsError> {
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 1..=MAX_ATTEMPTS {
        match ureq::get(archive_url).call() {
            Ok(mut response) => {
                return response
                    .body_mut()
                    .with_config()
                    .limit(200 * 1024 * 1024)
                    .read_to_vec()
                    .map_err(|err| MarsError::Http {
                        url: archive_url.to_string(),
                        status: 0,
                        message: err.to_string(),
                    });
            }
            Err(ureq::Error::StatusCode(status)) => {
                if status == 429 && attempt < MAX_ATTEMPTS {
                    std::thread::sleep(Duration::from_millis(150 * attempt as u64));
                    continue;
                }
                return Err(MarsError::Http {
                    url: archive_url.to_string(),
                    status,
                    message: format!("request failed with HTTP status {status}"),
                });
            }
            Err(err) => {
                return Err(MarsError::Http {
                    url: archive_url.to_string(),
                    status: 0,
                    message: err.to_string(),
                });
            }
        }
    }

    Err(MarsError::Http {
        url: archive_url.to_string(),
        status: 429,
        message: "request failed after retrying HTTP 429".to_string(),
    })
}

pub(crate) fn extract_and_strip_archive(
    archive_bytes: &[u8],
    dest: &Path,
) -> Result<(), MarsError> {
    let decoder = GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = Archive::new(decoder);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();

        if entry_type.is_symlink() || entry_type.is_hard_link() {
            continue;
        }

        let entry_path = entry.path()?;
        if entry_path.is_absolute() {
            return Err(MarsError::InvalidRequest {
                message: format!(
                    "archive entry contains absolute path: {}",
                    entry_path.display()
                ),
            });
        }

        let mut components = entry_path.components();
        // Strip the top-level `{repo}-{sha}/` directory.
        components.next();

        let mut relative_path = PathBuf::new();
        for component in components {
            match component {
                Component::Normal(seg) => relative_path.push(seg),
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(MarsError::InvalidRequest {
                        message: format!(
                            "archive entry attempts parent traversal: {}",
                            entry_path.display()
                        ),
                    });
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(MarsError::InvalidRequest {
                        message: format!(
                            "archive entry has invalid path: {}",
                            entry_path.display()
                        ),
                    });
                }
            }
        }

        if relative_path.as_os_str().is_empty() {
            continue;
        }

        let target_path = dest.join(&relative_path);

        if entry_type.is_dir() {
            fs::create_dir_all(&target_path)?;
            continue;
        }

        if !entry_type.is_file() {
            continue;
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut output = fs::File::create(&target_path)?;
        io::copy(&mut entry, &mut output)?;
    }

    Ok(())
}

pub(crate) fn fetch_archive(
    url: &str,
    sha: &str,
    cache: &GlobalCache,
) -> Result<PathBuf, MarsError> {
    let (owner, repo) = github_owner_repo(url).ok_or_else(|| MarsError::Source {
        source_name: url.to_string(),
        message: "expected GitHub URL in the form https://github.com/owner/repo".to_string(),
    })?;

    let archive_url = format!("https://github.com/{owner}/{repo}/archive/{sha}.tar.gz");
    let cache_key = archive_cache_component(url, sha)?;
    let cache_path = cache.archives_dir().join(cache_key);

    if cache_path.exists() {
        return Ok(cache_path);
    }

    let archive_bytes = download_archive_bytes(&archive_url)?;
    let temp_name = format!(
        "{}.tmp.{}",
        cache_path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    );
    let temp_path = cache_path.with_file_name(temp_name);

    if temp_path.exists() {
        let _ = safe_remove(&temp_path);
    }
    fs::create_dir_all(&temp_path)?;

    let extract_result = extract_and_strip_archive(&archive_bytes, &temp_path);
    if let Err(err) = extract_result {
        let _ = safe_remove(&temp_path);
        return Err(err);
    }

    publish_cache_dir_if_absent(&temp_path, &cache_path)?;
    Ok(cache_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Cursor;
    use std::io::Write;
    use tar::Builder;
    use tempfile::TempDir;

    fn build_tar_gz(files: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);

        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_mode(0o644);
            header.set_size(contents.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, *path, Cursor::new(*contents))
                .unwrap();
        }

        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    fn build_tar_gz_with_symlink() -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);

        let file_contents = b"safe\n";
        let mut file_header = tar::Header::new_gnu();
        file_header.set_mode(0o644);
        file_header.set_size(file_contents.len() as u64);
        file_header.set_cksum();
        builder
            .append_data(
                &mut file_header,
                "repo-abc/agents/coder.md",
                Cursor::new(file_contents),
            )
            .unwrap();

        let mut symlink_header = tar::Header::new_gnu();
        symlink_header.set_entry_type(tar::EntryType::Symlink);
        symlink_header.set_mode(0o777);
        symlink_header.set_size(0);
        symlink_header.set_cksum();
        builder
            .append_link(&mut symlink_header, "repo-abc/agents/link.md", "coder.md")
            .unwrap();

        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap()
    }

    fn write_tar_field(dst: &mut [u8], value: &[u8]) {
        let len = value.len().min(dst.len());
        dst[..len].copy_from_slice(&value[..len]);
    }

    fn write_tar_octal(dst: &mut [u8], value: u64) {
        let width = dst.len().saturating_sub(1);
        let octal = format!("{value:0width$o}");
        let bytes = octal.as_bytes();
        let copy_len = bytes.len().min(width);
        dst[..copy_len].copy_from_slice(&bytes[..copy_len]);
        dst[dst.len() - 1] = 0;
    }

    fn build_raw_tar_gz_single_file(path: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = [0_u8; 512];
        write_tar_field(&mut header[0..100], path.as_bytes());
        write_tar_octal(&mut header[100..108], 0o644);
        write_tar_octal(&mut header[108..116], 0);
        write_tar_octal(&mut header[116..124], 0);
        write_tar_octal(&mut header[124..136], contents.len() as u64);
        write_tar_octal(&mut header[136..148], 0);
        header[156] = b'0';
        write_tar_field(&mut header[257..263], b"ustar\0");
        write_tar_field(&mut header[263..265], b"00");

        for b in &mut header[148..156] {
            *b = b' ';
        }
        let checksum: u32 = header.iter().map(|b| *b as u32).sum();
        let checksum_field = format!("{checksum:06o}\0 ");
        write_tar_field(&mut header[148..156], checksum_field.as_bytes());

        let mut tar = Vec::new();
        tar.extend_from_slice(&header);
        tar.extend_from_slice(contents);
        let padding = (512 - (contents.len() % 512)) % 512;
        tar.extend(vec![0_u8; padding]);
        tar.extend(vec![0_u8; 1024]);

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn extract_and_strip_archive_flattens_top_level_directory() {
        let tarball = build_tar_gz(&[
            ("repo-abc/agents/coder.md", b"agent"),
            ("repo-abc/skills/review/SKILL.md", b"skill"),
        ]);
        let out = TempDir::new().unwrap();

        extract_and_strip_archive(&tarball, out.path()).unwrap();

        assert_eq!(
            fs::read_to_string(out.path().join("agents/coder.md")).unwrap(),
            "agent"
        );
        assert_eq!(
            fs::read_to_string(out.path().join("skills/review/SKILL.md")).unwrap(),
            "skill"
        );
    }

    #[test]
    fn extract_and_strip_archive_rejects_parent_traversal() {
        let tarball = build_raw_tar_gz_single_file("repo-abc/../escape.txt", b"bad");
        let out = TempDir::new().unwrap();

        let err = extract_and_strip_archive(&tarball, out.path()).unwrap_err();
        assert!(matches!(err, MarsError::InvalidRequest { .. }));
        assert!(!out.path().join("escape.txt").exists());
    }

    #[test]
    fn extract_and_strip_archive_skips_symlinks() {
        let tarball = build_tar_gz_with_symlink();
        let out = TempDir::new().unwrap();

        extract_and_strip_archive(&tarball, out.path()).unwrap();

        assert!(out.path().join("agents/coder.md").exists());
        assert!(!out.path().join("agents/link.md").exists());
    }

    #[test]
    fn fetch_archive_uses_safe_cache_key_before_network() {
        let cache_root = TempDir::new().unwrap();
        let cache = GlobalCache {
            root: cache_root.path().join("cache"),
        };
        fs::create_dir_all(cache.archives_dir()).unwrap();

        let url = "https://github.com/group/pkg.git";
        let sha = "abc123";
        let expected_cache_path = cache
            .archives_dir()
            .join(archive_cache_component(url, sha).unwrap());
        fs::create_dir_all(&expected_cache_path).unwrap();

        let resolved = fetch_archive(url, sha, &cache).unwrap();

        assert_eq!(resolved, expected_cache_path);
        let file_name = resolved.file_name().unwrap().to_string_lossy();
        assert!(
            !file_name.contains(':'),
            "archive cache key must be one Windows-safe component: {file_name}"
        );
    }
}
