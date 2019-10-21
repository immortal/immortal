extern crate immortal;
extern crate getopts;
extern crate time;

use immortal::scan;
use getopts::Options;
use std::env;
use std::thread;
use std::time::Duration;
use time::now_utc;

fn print_usage(program: &str, opts: Options) {
    let m = format!("usage: {} dir\n\n    dir    The directory that will be scanned.", program);
    print!("{}", opts.usage(&m));
}

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    let args: Vec<String> = env::args().collect();
    let program = &args[0];
    let mut opts = Options::new();

    opts.optflag("v", "version", &format!("Print version: {}", version));

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => { m }
        Err(_) => {
            print_usage(&program, opts);
            return;
        }
    };

    if matches.opt_present("v") {
        println!("{}", version);
        return;
    }

    let input = if !matches.free.is_empty() {
        matches.free[0].clone()
    } else {
        print_usage(&program, opts);
        return;
    };

    let sd = match scan::Scandir::new(&input, 5) {
        Err(_) => {
            println!("No such file or directory");
            return;
        },
        Ok(val) => val
    };

    println!("Scandir: {}", sd.path.display());
    sd.scan();

    // repeat every 5 seconds
    loop {
        println!("{}", now_utc().rfc3339());
        thread::sleep(Duration::new(sd.interval, 0));
        sd.scan();
    }
}
