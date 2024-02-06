/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::path::Path;

use crate::util::fs_ctx::read_dir;
use crate::util::fs_ctx::set_permissions;
use crate::util::fs_ctx::symlink_metadata;

/// Takes the specified `folder` (which must point to a directory) and
/// recursively makes all entries within it read-only, but it does *not* change
/// the permissions on the folder itself. Symlinks are not followed and no
/// attempt is made to change their permissions.
pub fn make_tree_entries_read_only(folder: &Path) -> anyhow::Result<()> {
    debug_assert!(folder.is_dir());

    for entry in read_dir(folder)? {
        let entry = entry?;
        let metadata = symlink_metadata(&entry.path())?;
        if metadata.is_symlink() {
            continue;
        } else if metadata.is_dir() {
            make_tree_entries_read_only(&entry.path())?;
        }

        let mut perms = metadata.permissions();
        perms.set_readonly(true);
        set_permissions(&entry.path(), perms)?;
    }

    Ok(())
}
