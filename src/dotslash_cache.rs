/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::env;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

#[cfg(unix)]
use nix::unistd::getuid;

pub const DOTSLASH_CACHE_ENV: &str = "DOTSLASH_CACHE";

#[derive(Debug)]
pub struct DotslashCache {
    cache_dir: PathBuf,
}

/// The DotSlash cache is organized as follows:
/// - Any subfolder that starts with two lowercase hex digits is the parent
///   folder for artifacts whose *artifact hash* starts with those two hex
///   digits (see `ArtifactLocation::artifact_directory`).
/// - The only other subfolder is `locks/`, which internally is organized
///   to the root of the cache folder.
///
/// The motivation behind this organization is to keep the paths to artifacts
/// as short as reasonably possible to avoid exceeding `MAX_PATH` on Windows.
/// The `locks/` folder is kept separate so it can be blown away independent of
/// the artifacts.
impl DotslashCache {
    pub fn new() -> Self {
        Self::new_in(get_dotslash_cache())
    }

    pub fn new_in<P: Into<PathBuf>>(p: P) -> Self {
        Self {
            cache_dir: p.into(),
        }
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    pub fn artifacts_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// artifact_hash_prefix should be two lowercase hex digits.
    pub fn locks_dir(&self, artifact_hash_prefix: &str) -> PathBuf {
        self.cache_dir.join("locks").join(artifact_hash_prefix)
    }
}

impl Default for DotslashCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Return the directory where DotSlash should write its cached artifacts.
/// Although DotSlash does not currently have any global config files,
/// if it did, most platforms would prefer config files to be stored in
/// a separate directory that is backed up and should not be blown away
/// when the user is low on space like /tmp.
fn get_dotslash_cache() -> PathBuf {
    if let Some(val) = env::var_os(DOTSLASH_CACHE_ENV) {
        return PathBuf::from(val);
    }

    // `dirs` returns the preferred cache directory for the user and the
    // platform based on these rules: https://docs.rs/dirs/*/dirs/fn.cache_dir.html
    #[cfg_attr(windows, allow(clippy::let_and_return))]
    let cache_dir = match dirs::cache_dir() {
        Some(cache_dir) => cache_dir.join("dotslash"),
        None => panic!("could not find DotSlash root - specify $DOTSLASH_CACHE"),
    };

    // `dirs` relies on `$HOME`. When running under `sudo` `$HOME` may not be
    // the sudoer's home dir. We want to avoid the situation where some
    // privileged user (like `root`) owns the cache dir in some other user's
    // home dir.
    //
    // Note that on a devserver (and macOS is basically the same):
    //
    // ```
    // $ bash -c 'echo $SUDO_USER $USER $HOME'
    // asuarez asuarez /home/asuarez
    // $ sudo bash -c 'echo $SUDO_USER $USER $HOME'
    // asuarez root /home/asuarez
    // $ sudo -H bash -c 'echo $SUDO_USER $USER $HOME'
    // asuarez root /root
    // ```
    //
    // i.e., `$USER` is reliable in the presence of sudo but `$HOME` is not.
    #[cfg(unix)]
    if !is_safe_to_own(&cache_dir) {
        let temp_dir = env::temp_dir();
        // e.g. $TEMP/dotslash-UID
        return named_cache_dir_at(temp_dir);
    }

    cache_dir
}

#[cfg_attr(windows, allow(dead_code))]
fn named_cache_dir_at<P: Into<PathBuf>>(dir: P) -> PathBuf {
    let mut name = OsString::from("dotslash-");

    // e.g. dotslash-UID
    #[cfg(unix)]
    name.push(getuid().as_raw().to_string());

    // e.g. dotslash-$USERNAME
    #[cfg(windows)]
    name.push(env::var_os("USERNAME").unwrap_or_else(|| "".into()));

    // e.g. $DIR/dotslash-UID
    let mut dir = dir.into();
    dir.push(name);

    dir
}

// A path is considered "safe to own" if:
// (1) it exists and we own it, or,
// (2) it doesn't exist and we own the nearest parent that does exist.
#[cfg(unix)]
fn is_safe_to_own(path: &Path) -> bool {
    use std::io;
    use std::os::unix::fs::MetadataExt;

    for ancestor in path.ancestors() {
        // Use `symlink_metadata` and not `metadata` because we're not
        // interested in following symlinks. If the path is a broken
        // symlink we want to still check the owner on that, instead of
        // treating it like a "NotFound".
        match ancestor.symlink_metadata() {
            Ok(meta) => {
                return getuid().as_raw() == meta.uid();
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {
                continue;
            }
            // We tried to check `/a/b/c` but `/a/b` exists and it is not a
            // directory (could be a file). For the purposes of ownership,
            // we'll treat this case as if the path does not exist so we can
            // keep checking.
            Err(ref e) if e.raw_os_error() == Some(nix::errno::Errno::ENOTDIR as i32) => {
                continue;
            }
            Err(ref e) if e.kind() == io::ErrorKind::PermissionDenied => {
                return false;
            }
            // Not sure how this can happen.
            Err(_) => {
                return false;
            }
        }
    }

    false
}
