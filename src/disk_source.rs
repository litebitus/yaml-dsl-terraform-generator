//! The working tree as the compiler's file source: files are read where named,
//! and no directory is ever listed.

use std::io;
use std::path::{Path, PathBuf};

use yaml_dsl_compiler::FileSource;

pub struct DiskSource;

impl FileSource for DiskSource {
    fn list_files(&self, directory: &Path) -> io::Result<Vec<PathBuf>> {
        Err(io::Error::other(format!(
            "tfgen lists no directory: {} was asked for",
            directory.display()
        )))
    }

    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.is_file()
    }
}
