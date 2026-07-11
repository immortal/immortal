//! Linux-specific process and filesystem integration.

use std::{fs::Metadata, os::linux::fs::MetadataExt};

use super::FileIdentity;

pub(crate) fn file_identity(metadata: &Metadata) -> FileIdentity {
    FileIdentity::new(metadata.st_dev(), metadata.st_ino())
}
