use immortallog::cli::{
    self,
    actions::{self, Action},
};

fn main() -> std::process::ExitCode {
    let action = match cli::start() {
        Ok(action) => action,
        Err(error) => return error.report(),
    };
    let result = match action {
        Action::Write(action) => actions::write::execute(&action),
        Action::Archives(action) => actions::archives::execute(&action),
    };
    cli::finish(result)
}
