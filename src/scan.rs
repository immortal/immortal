use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub struct Scandir {
    pub path: PathBuf,
    pub interval: u64,
}

impl Scandir {
    pub fn new(dir: &str, seconds: u64) -> Result<Scandir, io::Error> {
        let path = try!(fs::canonicalize(dir));
        Ok(
            Scandir {
                path: path,
                interval: seconds
            }
          )
    }

    pub fn scan(&self) {
        let files = match fs::read_dir(&self.path) {
            Err(f) => {
                println!("{}", f);
                return;
            }
            Ok(f) => f
        };

        for f in files {
            let file = f.unwrap();
            let mode = file.metadata().unwrap().permissions().mode();
            let mut is_exec: bool = false;
            if !file.file_type().unwrap().is_dir() {
                is_exec =  mode & 0o111 != 0;
            }
            println!("path: {} name: {} mode: {:o} is_exec: {}", file.path().display(), file.file_name().into_string().unwrap(), mode, is_exec);
        }
    }
}
