use std::{error::Error, fs, path::PathBuf};

pub struct Marker(PathBuf);

impl Marker {
    pub fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("immortal-contract-{name}-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        Self(path)
    }

    pub fn path_string(&self) -> Result<String, Box<dyn Error>> {
        self.0
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| "contract marker path is not UTF-8".into())
    }

    pub fn exists(&self) -> bool {
        self.0.exists()
    }
}

impl Drop for Marker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
