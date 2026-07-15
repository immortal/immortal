use immortaldir::cli::{
    self,
    actions::{self, Action},
};

fn main() -> std::process::ExitCode {
    let action = match cli::start() {
        Ok(action) => action,
        Err(error) => return error.report(),
    };

    let result = match action {
        Action::Reconcile(action) => actions::reconcile::execute(&action),
    };

    cli::finish(result)
}
