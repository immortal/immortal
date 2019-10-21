extern crate immortal;
extern crate clap;

use immortal::daemon;
use std::env;
use clap::{Arg, App, SubCommand};

fn main() {
    let matches = App::new("immortal")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Run a command forever")
        .arg(Arg::with_name("q")
             .short("q")
             .long("quiet")
             .help("Redirect standard input, output, error to /dev/null")
             .takes_value(false))
        .arg(Arg::with_name("f")
             .short("f")
             .long("follow")
             .help("Follow pid")
             .takes_value(false))
        .arg(Arg::with_name("u")
             .short("u")
             .long("user")
             .help("Execute command on behalf user")
             .takes_value(true))
        .arg(Arg::with_name("command")
             .help("Command to daemonize")
             .required(true)
             .index(1))
        .get_matches();

    if matches.is_present("u") {
        println!("user: {}",  matches.value_of("u").unwrap());
    }
}
