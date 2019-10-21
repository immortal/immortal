extern crate immortal;

use immortal::ctrl;

fn main() {
    let msg = ctrl::hello();
    println!("{}", msg);
}
