//! The working tree as the compiler's file source: files are read where named, and
//! a directory is listed only when the compiler asks, through git.

use std::io;
use std::path::{Path, PathBuf};

use yaml_dsl_compiler::FileSource;

use crate::stack_discovery::git_files;

pub struct DiskSource;

impl FileSource for DiskSource {
    fn list_files(&self, directory: &Path) -> io::Result<Vec<PathBuf>> {
        if !directory.is_dir() {
            return Ok(Vec::new());
        }
        git_files(directory, None).map_err(io::Error::other)
    }

    fn read(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.is_file()
    }
}
