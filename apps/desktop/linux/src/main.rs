mod ui;
fn main() -> gtk::glib::ExitCode {
    if let Some(result) = memorious_linux::background::run_daemon() {
        if let Err(error) = result {
            eprintln!("{error:#}");
            return gtk::glib::ExitCode::FAILURE;
        }
        return gtk::glib::ExitCode::SUCCESS;
    }
    ui::run()
}
